//! [`ReconnectingClient`]: a connection that re-establishes itself when the daemon on the other end is replaced.
//!
//! A handover carries the listening socket across `execve` but not an accepted
//! one, so a dog's own process survives holding a dead socket. [`Client`] has
//! no such mode: the CLI's one-shot verbs must never see a request silently
//! retried, so in-flight requests here fail too, and only the connection
//! re-establishes. A background task reconnects as soon as the connection
//! dies rather than on the next request, so an idle dog still re-handshakes
//! before `shep daemon reload` polls dog staleness, and a refusing successor
//! stops the supervisor ([`LinkState::Refused`]) rather than retrying.
//! [`ReconnectingClient::connect_as_dog`] names the dog on every handshake so
//! a refusal is actionable.

use core::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::Duration;

use tokio::task::JoinHandle;

use shep_core::protocol::{HelloAck, Request, Response};

use crate::client::{Client, RequestError};
use crate::connection::{ConnectError, HANDSHAKE_TIMEOUT};
use crate::events::EventStream;

/// How long the supervisor waits after its FIRST failed reconnect attempt
/// before trying again.
///
/// The first attempt carries no delay: across a handover the listening
/// socket stays bound, so `connect(2)` succeeds into the backlog and only
/// the handshake waits for the successor to start accepting. 50ms is the
/// pause before a second attempt, short enough that a successor a few
/// milliseconds late costs one of these rather than a visible outage.
pub const RECONNECT_MIN_DELAY: Duration = Duration::from_millis(50);

/// The ceiling [`RECONNECT_MIN_DELAY`] doubles up to.
///
/// Same order as [`HANDSHAKE_TIMEOUT`]: past this point one further attempt
/// costs about as much as the wait between attempts, so the loop is
/// bounded noise rather than a spin. A dog whose daemon is genuinely gone
/// sits here indefinitely: the daemon that would have reaped it is the one
/// that vanished.
pub const RECONNECT_MAX_DELAY: Duration = Duration::from_secs(5);

/// What a [`ReconnectingClient`]'s supervisor is currently doing.
///
/// Non-exhaustive: expect more variants.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum LinkState {
    /// Connected. Requests go out on this generation of the connection.
    Connected,
    /// The connection dropped and the supervisor is re-establishing it.
    /// Requests issued now fail with [`RequestError::Closed`]; they are not
    /// queued and not retried.
    Reconnecting,
    /// A successor refused the handshake on protocol-version skew. The
    /// supervisor has stopped, and every later request fails with
    /// [`RequestError::Closed`]: the daemon that refused is the party
    /// that can fix it, not a retry.
    Refused {
        /// The daemon's own crate version, when it named one. `None` from a
        /// daemon built before the refusal carried it.
        daemon_version: Option<String>,
        /// The daemon's refusal message, verbatim.
        message: String,
    },
}

/// A [`Client`] that re-establishes its own connection when the daemon it
/// was talking to is replaced.
///
/// Built for dogs, which outlive the shepherd they connected to: a dog's
/// process crosses a daemon handover as an ordinary child, so it is still
/// running when its socket dies. The CLI uses a bare [`Client`] instead;
/// see this module's own docs for why.
///
/// # Example
///
/// ```no_run
/// use shep_client::{ReconnectingClient, shep_core::protocol::Request};
///
/// # async fn dog(socket: &std::path::Path) -> Result<(), Box<dyn core::error::Error>> {
/// // `name` is what the daemon put in `$SHEP_DOG_NAME` when it spawned
/// // this process; it lets the daemon act on a refusal (G8).
/// let client = ReconnectingClient::connect_as_dog(socket, "metrics").await?;
/// // Survives the daemon being replaced underneath it; a request that was
/// // in flight at the moment it happened still fails rather than retrying.
/// let _flock = client.request(Request::ListFlock).await?;
/// # Ok(())
/// # }
/// # let _ = dog;
/// ```
pub struct ReconnectingClient {
    shared: Arc<Shared>,
    supervisor: JoinHandle<()>,
}

/// Manual, not derived: the socket path and the [`HelloAck`] carry no
/// secret, and neither does [`LinkState`], whose one payload is the
/// daemon's own refusal sentence. Manual because [`RwLock`] and
/// [`JoinHandle`] derive into noise, and a derived impl would print the
/// inner [`Client`] where the link state is the useful fact.
impl fmt::Debug for ReconnectingClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReconnectingClient")
            .field("socket", &self.shared.socket)
            .field("dog_name", &self.shared.dog_name)
            .field("link", &self.link())
            .field("ack", &self.daemon())
            .finish_non_exhaustive()
    }
}

/// Everything the handle and its supervisor both reach: the address to
/// reconnect to, the budget to do it under, and the current generation.
struct Shared {
    socket: PathBuf,
    handshake_timeout: Duration,
    /// The name this client announces itself as a dog under, re-sent on
    /// every reconnect. See [`ReconnectingClient::connect_as_dog`].
    dog_name: Option<String>,
    state: RwLock<State>,
}

/// The generation of the connection in force right now, and what the
/// supervisor last reported about it.
struct State {
    client: Arc<Client>,
    link: LinkState,
}

impl Shared {
    /// A read guard, treating a poisoned lock as ordinary data.
    ///
    /// Nothing inside a critical section here can panic: every one is a
    /// clone or an assignment of a plain struct, so poisoning cannot
    /// signal a torn value.
    fn read(&self) -> RwLockReadGuard<'_, State> {
        self.state.read().unwrap_or_else(PoisonError::into_inner)
    }

    /// A write guard. See [`Self::read`] for the poisoning argument.
    fn write(&self) -> RwLockWriteGuard<'_, State> {
        self.state.write().unwrap_or_else(PoisonError::into_inner)
    }

    /// The current generation, cloned out so the guard is dropped before
    /// any caller awaits on it: no lock is ever held across an `await`.
    fn client(&self) -> Arc<Client> {
        Arc::clone(&self.read().client)
    }

    fn set_link(&self, link: LinkState) {
        self.write().link = link;
    }

    /// Swaps in a freshly handshaken generation, dropping the dead one.
    fn install(&self, client: Client) {
        let mut state = self.write();
        state.client = Arc::new(client);
        state.link = LinkState::Connected;
    }
}

impl ReconnectingClient {
    /// Connects to `socket`, performs the version handshake bounded by
    /// [`HANDSHAKE_TIMEOUT`], and starts the supervisor that will
    /// re-establish this connection whenever it dies.
    ///
    /// The FIRST connection is not supervised: a socket nobody is listening
    /// on is a caller's error, not a handover, so this reports it rather
    /// than retrying behind the caller's back.
    ///
    /// # Errors
    ///
    /// See [`Self::connect_with_timeout`].
    pub async fn connect(socket: &Path) -> Result<Self, ConnectError> {
        Self::connect_with_timeout(socket, HANDSHAKE_TIMEOUT).await
    }

    /// As [`Self::connect`], but with a caller-supplied handshake timeout,
    /// used for the first connection and for every reconnect after it.
    ///
    /// # Errors
    ///
    /// - [`ConnectError::Connect`]: the initial `connect(2)` call failed.
    /// - [`ConnectError::Wire`]: `Hello` failed to encode, or the reply failed to decode.
    /// - [`ConnectError::Io`]: a framed read or write failed after connect.
    /// - [`ConnectError::HandshakeClosed`]: the peer closed before a `HelloReply`.
    /// - [`ConnectError::HandshakeTimeout`]: no `HelloReply` arrived within `timeout`.
    /// - [`ConnectError::ProtocolMismatch`]: the daemon refused on protocol-version skew.
    pub async fn connect_with_timeout(
        socket: &Path,
        timeout: Duration,
    ) -> Result<Self, ConnectError> {
        Self::connect_inner(socket, timeout, None).await
    }

    /// As [`Self::connect`], but announcing this client as the dog
    /// registered under `name`, the value the daemon put in
    /// `$SHEP_DOG_NAME` when it spawned this process.
    ///
    /// A dog should use this rather than [`Self::connect`]: the name makes
    /// a refused handshake actionable, since a refusal never reaches a
    /// request. Without it, the daemon still stops rather than spins on a
    /// refusal, but cannot say which dog went stale or restart it.
    ///
    /// # Errors
    ///
    /// See [`Self::connect_with_timeout`].
    pub async fn connect_as_dog(socket: &Path, name: &str) -> Result<Self, ConnectError> {
        Self::connect_as_dog_with_timeout(socket, HANDSHAKE_TIMEOUT, name).await
    }

    /// As [`Self::connect_as_dog`], but with a caller-supplied handshake
    /// timeout, used for the first connection and for every reconnect after
    /// it.
    ///
    /// # Errors
    ///
    /// See [`Self::connect_with_timeout`].
    pub async fn connect_as_dog_with_timeout(
        socket: &Path,
        timeout: Duration,
        name: &str,
    ) -> Result<Self, ConnectError> {
        Self::connect_inner(socket, timeout, Some(name)).await
    }

    async fn connect_inner(
        socket: &Path,
        timeout: Duration,
        dog_name: Option<&str>,
    ) -> Result<Self, ConnectError> {
        let client = Client::connect_as(socket, timeout, dog_name).await?;
        let shared = Arc::new(Shared {
            socket: socket.to_path_buf(),
            handshake_timeout: timeout,
            dog_name: dog_name.map(str::to_owned),
            state: RwLock::new(State {
                client: Arc::new(client),
                link: LinkState::Connected,
            }),
        });
        let supervisor = tokio::spawn(supervise(Arc::clone(&shared)));
        Ok(Self { shared, supervisor })
    }

    /// The name this client announces itself as a dog under, or `None` for
    /// a caller that is not one.
    #[must_use]
    pub fn dog_name(&self) -> Option<&str> {
        self.shared.dog_name.as_deref()
    }

    /// The handshake acknowledgement of the daemon this client is talking
    /// to right now.
    ///
    /// Owned rather than borrowed, unlike [`Client::daemon`]: the ack
    /// belongs to a generation a reconnect can replace at any moment, so a
    /// reference would either pin a stale one or need a guard in the
    /// caller's hands. A cached ack would describe the predecessor, which
    /// a dog publishing `daemon_version` must not do.
    #[must_use]
    pub fn daemon(&self) -> HelloAck {
        self.shared.read().client.daemon().clone()
    }

    /// The path this client connects (and reconnects) through.
    #[must_use]
    pub fn socket(&self) -> &Path {
        &self.shared.socket
    }

    /// What the supervisor is doing right now.
    #[must_use]
    pub fn link(&self) -> LinkState {
        self.shared.read().link.clone()
    }

    /// Sends `body` with [`DEFAULT_DEADLINE`](crate::DEFAULT_DEADLINE) on
    /// the current generation of the connection.
    ///
    /// # Errors
    ///
    /// See [`Self::request_with_deadline`].
    pub async fn request(&self, body: Request) -> Result<Response, RequestError> {
        self.request_with_deadline(body, None).await
    }

    /// Sends `body` with `deadline` on the current generation.
    ///
    /// Never retried: a request on the wire when the daemon was replaced
    /// fails, and so does one issued while reconnecting. Only the
    /// connection re-establishes; the caller decides whether resending is safe.
    ///
    /// # Errors
    ///
    /// - [`RequestError::Rpc`]: the daemon answered with a structured error.
    /// - [`RequestError::Timeout`]: no reply within the request's budget.
    /// - [`RequestError::Closed`]: the connection closed before a reply arrived, or already had.
    /// - [`RequestError::Wire`]: `body` failed to encode.
    pub async fn request_with_deadline(
        &self,
        body: Request,
        deadline: Option<Duration>,
    ) -> Result<Response, RequestError> {
        self.shared
            .client()
            .request_with_deadline(body, deadline)
            .await
    }

    /// Subscribes the current generation of the connection to `topics`.
    ///
    /// The returned stream belongs to one generation and is not re-armed
    /// across a reconnect: it ends when that connection dies, like
    /// [`Client::subscribe`]'s would. A consumer that wants events past a
    /// handover subscribes again; re-arming here would silently swallow
    /// the gap between the old connection dying and a new `Subscribe`.
    ///
    /// # Errors
    ///
    /// Same as [`Self::request`].
    pub async fn subscribe(&self, topics: Vec<String>) -> Result<EventStream, RequestError> {
        self.shared.client().subscribe(topics).await
    }
}

/// Stops the supervisor when the handle goes away.
///
/// Without this, a dropped `ReconnectingClient` whose daemon is gone leaves
/// a task reconnecting forever against an address nobody will answer,
/// holding the last generation's socket open with it. `abort` rather than a
/// cooperative signal because the supervisor spends its life parked on
/// either a connection's death or a connect attempt, neither of which would
/// notice a flag until it woke.
impl Drop for ReconnectingClient {
    fn drop(&mut self) {
        self.supervisor.abort();
    }
}

/// The supervisor loop: wait for the current generation to die, then
/// re-establish it, forever.
///
/// Ends only on a protocol refusal (see [`LinkState::Refused`]) or when the
/// handle is dropped and this task is aborted.
async fn supervise(shared: Arc<Shared>) {
    loop {
        // held only for the death await, dropped before reconnecting so the
        // dead socket goes away as soon as `install` replaces it
        let generation = shared.client();
        generation.closed().await;
        drop(generation);
        shared.set_link(LinkState::Reconnecting);

        let mut delay = RECONNECT_MIN_DELAY;
        loop {
            match Client::connect_as(
                &shared.socket,
                shared.handshake_timeout,
                shared.dog_name.as_deref(),
            )
            .await
            {
                Ok(fresh) => {
                    shared.install(fresh);
                    break;
                }
                Err(ConnectError::ProtocolMismatch {
                    daemon_version,
                    message,
                    ..
                }) => {
                    shared.set_link(LinkState::Refused {
                        daemon_version,
                        message,
                    });
                    return;
                }
                // Everything else is a daemon that is not ready yet: the
                // successor has not started accepting, or the socket is
                // momentarily unbound. Those resolve on their own, so the
                // only question is how often to ask.
                Err(_transient) => {
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(RECONNECT_MAX_DELAY);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use shep_core::protocol::{PROTOCOL_VERSION, RpcError, RpcErrorCode};

    use super::*;
    use crate::testing::{Handovers, Handshake, control_address, fake_daemon_across_handovers};

    /// Every bounded wait in this module uses one budget: generous against
    /// a loaded CI runner, small enough that a genuinely stuck test fails
    /// rather than hanging the suite.
    const BOUND: Duration = Duration::from_secs(5);

    /// The window a test watches for something that must not happen.
    ///
    /// Eight times [`RECONNECT_MIN_DELAY`], so a supervisor still looping
    /// would have made several further attempts inside it, the first after
    /// 50ms. Stated against the delay it has to outrun, not a round number.
    const NEGATIVE_WINDOW: Duration = Duration::from_millis(400);

    /// An ack distinguishable per generation, so a test can tell which
    /// daemon answered rather than only that one did.
    fn ack_from(pid: u32) -> HelloAck {
        HelloAck {
            daemon_version: format!("0.0.{pid}"),
            protocol: PROTOCOL_VERSION,
            pid,
        }
    }

    /// Waits until the fake has accepted `accepts` connections and the
    /// client has installed the newest of them, or fails inside [`BOUND`].
    ///
    /// Both halves are needed: the accept count alone rises before the
    /// handshake completes, and the link alone still reads `Connected` in
    /// the instant after a cut. The supervisor sets `Reconnecting` before
    /// it dials, so together they are unambiguous.
    ///
    /// Does not observe [`ReconnectingClient::daemon`]: some tests assert
    /// on the ack, and a helper that waited on it would make those fail
    /// for an unrelated reason.
    async fn await_reconnect(client: &ReconnectingClient, shepherds: &Handovers, accepts: u32) {
        let seen = tokio::time::timeout(BOUND, async {
            loop {
                if shepherds.accepted() >= accepts && client.link() == LinkState::Connected {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(
            seen.is_ok(),
            "no reconnect within {BOUND:?}: {} accepts, link {:?}",
            shepherds.accepted(),
            client.link()
        );
    }

    /// Waits until `client`'s link reaches a refusal, or fails inside
    /// [`BOUND`].
    async fn await_refusal(client: &ReconnectingClient) -> LinkState {
        let seen = tokio::time::timeout(BOUND, async {
            loop {
                let link = client.link();
                if matches!(link, LinkState::Refused { .. }) {
                    return link;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
        seen.unwrap_or_else(|_| panic!("the link never reached a refusal within {BOUND:?}"))
    }

    /// fails if a dog is left holding a dead socket after its daemon is
    /// replaced: only the listening socket crosses the exec, and an
    /// accepted one dies with the image.
    #[tokio::test]
    async fn a_dropped_connection_is_re_established_and_the_next_request_is_served() {
        let dir = tempfile::tempdir().unwrap();
        let path = control_address(dir.path());
        let shepherds = fake_daemon_across_handovers(
            &path,
            vec![
                Handshake::Accept(ack_from(11)),
                Handshake::Accept(ack_from(22)),
            ],
        );
        let client = ReconnectingClient::connect(&path).await.unwrap();

        let before = tokio::time::timeout(BOUND, client.request(Request::Ping))
            .await
            .expect("the first request must not hang");
        assert!(before.is_ok(), "before the handover: {before:?}");

        // The handover, exactly: the accepted connection dies under the
        // client while the listener stays bound.
        shepherds.cut().await;
        await_reconnect(&client, &shepherds, 2).await;

        let after = tokio::time::timeout(BOUND, client.request(Request::Ping))
            .await
            .expect("the request after a handover must not hang");
        assert!(
            after.is_ok(),
            "a request after the handover must be served, got {after:?}"
        );
    }

    /// fails if the ack keeps describing the predecessor after a
    /// reconnect, which would make a dog publish `daemon_version` for a
    /// daemon no longer running.
    #[tokio::test]
    async fn the_ack_follows_the_successor_rather_than_the_predecessor() {
        let dir = tempfile::tempdir().unwrap();
        let path = control_address(dir.path());
        let shepherds = fake_daemon_across_handovers(
            &path,
            vec![
                Handshake::Accept(ack_from(11)),
                Handshake::Accept(ack_from(22)),
            ],
        );
        let client = ReconnectingClient::connect(&path).await.unwrap();
        assert_eq!(client.daemon().pid, 11);
        assert_eq!(client.daemon().daemon_version, "0.0.11");

        shepherds.cut().await;
        await_reconnect(&client, &shepherds, 2).await;

        assert_eq!(
            client.daemon().daemon_version,
            "0.0.22",
            "the version must come from the daemon now answering"
        );
    }

    /// fails if a dog's name reaches the FIRST daemon and not its
    /// successor. The refusal is the one that matters and it is the second
    /// one here: a dog that named itself at boot and then reconnected
    /// anonymously would leave the successor unable to say which dog it
    /// just refused, which is precisely the case G8 exists for. A daemon's
    /// predecessor is not around to be asked.
    #[tokio::test]
    async fn a_dogs_name_rides_every_handshake_including_the_refused_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = control_address(dir.path());
        let shepherds = fake_daemon_across_handovers(
            &path,
            vec![
                Handshake::Accept(ack_from(11)),
                Handshake::Refuse(RpcError {
                    code: RpcErrorCode::ProtocolMismatch,
                    message: "daemon speaks protocol 3, client sent 2".to_string(),
                    daemon_version: Some("0.2.0".to_string()),
                }),
            ],
        );
        let client = ReconnectingClient::connect_as_dog(&path, "metrics")
            .await
            .unwrap();
        assert_eq!(client.dog_name(), Some("metrics"));

        shepherds.cut().await;
        await_refusal(&client).await;

        let named: Vec<Option<String>> = shepherds
            .hellos()
            .into_iter()
            .map(|hello| hello.dog_name)
            .collect();
        assert_eq!(
            named,
            vec![Some("metrics".to_string()), Some("metrics".to_string())],
            "every generation must be told which dog is talking to it"
        );
    }

    /// fails if a client that is not a dog claims to be one. The name is
    /// what lets a daemon restart a dog on a refused handshake, so a
    /// `ReconnectingClient` built without one must stay anonymous rather
    /// than inventing a name from its environment or its path.
    #[tokio::test]
    async fn a_client_that_is_not_a_dog_names_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = control_address(dir.path());
        let shepherds = fake_daemon_across_handovers(&path, vec![Handshake::Accept(ack_from(11))]);
        let client = ReconnectingClient::connect(&path).await.unwrap();
        assert_eq!(client.dog_name(), None);

        shepherds.cut().await;
        await_reconnect(&client, &shepherds, 2).await;

        assert!(
            shepherds
                .hellos()
                .iter()
                .all(|hello| hello.dog_name.is_none()),
            "an unnamed client must stay unnamed across a reconnect: {:?}",
            shepherds.hellos()
        );
    }

    /// fails if a request that was in flight when the daemon was replaced
    /// is re-sent to the successor: a re-issued `Stop` could stop a sheep
    /// twice, and the client cannot tell a request the daemon never saw
    /// from one it already acted on.
    ///
    /// The proof is positional: the successor's first envelope must be the
    /// request issued after the reconnect, not the abandoned `Ping`.
    #[tokio::test]
    async fn an_in_flight_request_fails_and_is_never_re_sent_to_the_successor() {
        let dir = tempfile::tempdir().unwrap();
        let path = control_address(dir.path());
        let shepherds = fake_daemon_across_handovers(
            &path,
            vec![
                Handshake::Accept(ack_from(11)),
                Handshake::Accept(ack_from(22)),
            ],
        );
        let client = ReconnectingClient::connect(&path).await.unwrap();

        // the predecessor reads this envelope and dies without answering it
        shepherds.cut_on_next_request();
        let lost = tokio::time::timeout(BOUND, client.request(Request::Ping))
            .await
            .expect("an abandoned request must fail, not hang");
        assert_eq!(
            lost,
            Err(RequestError::Closed),
            "an in-flight request must fail when the daemon is replaced"
        );

        await_reconnect(&client, &shepherds, 2).await;
        let served = tokio::time::timeout(BOUND, client.request(Request::ListFlock))
            .await
            .expect("the request after a handover must not hang");
        assert!(served.is_ok(), "after the handover: {served:?}");

        let successor: Vec<Request> = shepherds
            .envelopes()
            .into_iter()
            .filter(|(generation, _)| *generation == 2)
            .map(|(_, envelope)| envelope.body)
            .collect();
        assert_eq!(
            successor,
            vec![Request::ListFlock],
            "the successor must see only the request issued after the reconnect"
        );
    }

    /// fails if a refused reconnect is retried. A successor that refuses on
    /// protocol skew has said something no retry can change; the design's
    /// G8 puts the fix on the daemon (restart that dog once, from disk) and
    /// forbids the client spinning against it in the meantime.
    #[tokio::test]
    async fn a_refused_reconnect_stops_rather_than_spinning() {
        let dir = tempfile::tempdir().unwrap();
        let path = control_address(dir.path());
        let shepherds = fake_daemon_across_handovers(
            &path,
            vec![
                Handshake::Accept(ack_from(11)),
                Handshake::Refuse(RpcError {
                    code: RpcErrorCode::ProtocolMismatch,
                    message: "daemon speaks protocol 3, client speaks 2".into(),
                    daemon_version: Some("0.9.9".into()),
                }),
            ],
        );
        let client = ReconnectingClient::connect(&path).await.unwrap();

        shepherds.cut().await;
        let link = await_refusal(&client).await;

        let LinkState::Refused {
            daemon_version,
            message,
        } = link
        else {
            unreachable!("await_refusal only returns a refusal");
        };
        assert_eq!(daemon_version.as_deref(), Some("0.9.9"));
        assert!(message.contains("protocol 3"), "{message}");
        assert_eq!(
            shepherds.accepted(),
            2,
            "one initial connection plus exactly one refused reconnect"
        );
        // And it must STAY at two. Reaching `Refused` only proves the
        // supervisor got there; a supervisor that recorded the refusal and
        // then went round again would satisfy every assertion above and
        // still be the spin G8 forbids, so the window is the assertion.
        let spun = tokio::time::timeout(NEGATIVE_WINDOW, async {
            while shepherds.accepted() < 3 {
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(
            spun.is_err(),
            "the supervisor retried a refusal: {} accepts",
            shepherds.accepted()
        );
    }

    /// fails if a reconnect gives up on a daemon that is merely not ready
    /// yet. Across a real handover the listening socket stays bound while
    /// the successor replays its blob, so a connect that completes and a
    /// handshake that is not yet answered is the ordinary case, not a
    /// failure.
    #[tokio::test]
    async fn a_reconnect_retries_past_a_successor_that_is_not_accepting_yet() {
        let dir = tempfile::tempdir().unwrap();
        let path = control_address(dir.path());
        let shepherds = fake_daemon_across_handovers(
            &path,
            vec![
                Handshake::Accept(ack_from(11)),
                Handshake::Drop,
                Handshake::Drop,
                Handshake::Accept(ack_from(44)),
            ],
        );
        let client = ReconnectingClient::connect(&path).await.unwrap();

        shepherds.cut().await;
        await_reconnect(&client, &shepherds, 4).await;

        assert_eq!(
            shepherds.accepted(),
            4,
            "two unanswered handshakes must be retried past, not given up on"
        );
        let served = tokio::time::timeout(BOUND, client.request(Request::Ping))
            .await
            .expect("the request after the retries must not hang");
        assert!(served.is_ok(), "after the retries: {served:?}");
    }

    /// fails if dropping the handle leaves the supervisor reconnecting
    /// forever against an address nobody will answer.
    ///
    /// Asserts a negative: the bound is the assertion, and the accept
    /// count must stay flat within it.
    #[tokio::test]
    async fn dropping_the_handle_stops_the_supervisor() {
        let dir = tempfile::tempdir().unwrap();
        let path = control_address(dir.path());
        let shepherds = fake_daemon_across_handovers(
            &path,
            vec![Handshake::Accept(ack_from(11)), Handshake::Drop],
        );
        let client = ReconnectingClient::connect(&path).await.unwrap();

        shepherds.cut().await;
        // Let the supervisor get as far as its first (dropped) reconnect,
        // so it is provably still running at the moment the handle goes.
        let reached = tokio::time::timeout(BOUND, async {
            while shepherds.accepted() < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(reached.is_ok(), "the supervisor never retried at all");

        drop(client);

        let grew = tokio::time::timeout(NEGATIVE_WINDOW, async {
            while shepherds.accepted() < 3 {
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(
            grew.is_err(),
            "the supervisor kept reconnecting after its handle was dropped: {} accepts",
            shepherds.accepted()
        );
    }
}
