//! [`ReconnectingClient`]: a connection that re-establishes itself when the
//! daemon on the other end is replaced.
//!
//! # Why this is a separate type and not a mode on [`Client`]
//!
//! A daemon handover carries the listening socket across an `execve` but
//! cannot carry an accepted one, so every connection to the shepherd dies
//! at a reload. A dog's *process* survives that for free — it is a child of
//! a daemon whose pid does not change — so what a carried dog becomes is a
//! live process holding a dead socket. Measured over six real reloads: the
//! metrics dog kept its pid, reported zero restarts, stayed `online`, wrote
//! nothing to stderr, and answered HTTP 503 to every scrape for 6m 22s.
//!
//! The CLI has the opposite requirement. `shep stop` is one-shot: a request
//! that dropped mid-flight must fail, because a silently re-issued `Stop`
//! could stop a sheep twice — the second time after an operator's own
//! `start` put it back. So the reconnect could not go on [`Client`] itself,
//! nor behind a flag on it: a type whose retry behaviour depends on how it
//! was built is a type every CLI call site has to be read carefully to
//! trust. Making it a *distinct type* means the CLI is unaffected by
//! construction rather than by convention — it never names
//! `ReconnectingClient`, so no `shep` verb can acquire this behaviour by
//! accident or by a later edit to a shared constructor.
//!
//! # What it does and does not retry
//!
//! **In-flight requests fail, never retry.** A request already handed to
//! the connection when it died comes back [`RequestError::Closed`], exactly
//! as it does on a bare [`Client`]. The client cannot tell a request the
//! daemon never received from one it received and acted on before the image
//! swapped, so retrying would be a guess about a side effect. What
//! reconnects is the *connection*, so the caller's NEXT request is served.
//!
//! # Why the reconnect is supervised rather than lazy
//!
//! A background task waits on the current connection's death and
//! re-establishes it immediately, rather than the cheaper design of
//! reconnecting on the next use. Two reasons, both about the daemon's side
//! of the reload:
//!
//! - A dog nobody is talking to still has to re-handshake. A metrics dog
//!   scraped once a minute would otherwise spend that minute unreconnected,
//!   and a refusal the daemon needs to act on (the design's G8) would not
//!   surface until something happened to ask.
//! - `daemon reload` reports dog staleness *after* the dogs have
//!   reconnected (G13), which is only immediate if the reconnect is driven
//!   by the disconnection rather than by the next request.
//!
//! A request issued during the reconnect window still fails with
//! [`RequestError::Closed`]: the window is one connect plus one handshake
//! on a local socket, and waiting inside `request` would turn a fast
//! failure into a long hang for no correctness gain.
//!
//! # A refused reconnect stops
//!
//! A successor that refuses the handshake on protocol skew has said
//! something a retry cannot change: this dog's running image speaks a
//! protocol this daemon does not. The supervisor stops, and
//! [`ReconnectingClient::link`] reports [`LinkState::Refused`]. Restarting
//! that dog from disk is the daemon's job, not this client's — retrying
//! here would be the spin the design's G8 exists to forbid.

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
/// The first attempt carries no delay at all, which is the case that
/// matters: across a handover the listening socket never stops being bound,
/// so `connect(2)` succeeds into the backlog and only the handshake waits
/// for the successor to start accepting. 50ms is the pause before a second
/// attempt, short enough that a successor a few milliseconds late costs one
/// of these rather than a visible outage (IR-26: named, with the reason).
pub const RECONNECT_MIN_DELAY: Duration = Duration::from_millis(50);

/// The ceiling [`RECONNECT_MIN_DELAY`] doubles up to.
///
/// Same order as [`HANDSHAKE_TIMEOUT`] deliberately: once a daemon has been
/// unreachable long enough to reach this ceiling, one further attempt costs
/// about as much as the wait between attempts, so the loop is bounded noise
/// rather than a spin. A dog whose daemon is genuinely gone sits here
/// indefinitely, which is correct — the daemon that would have reaped it is
/// the one that vanished.
pub const RECONNECT_MAX_DELAY: Duration = Duration::from_secs(5);

/// What a [`ReconnectingClient`]'s supervisor is currently doing.
///
/// Growth is expected — library-crate public type (IR-20).
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
    /// supervisor has stopped and every later request fails with
    /// [`RequestError::Closed`] — a refusal is not something a retry can
    /// fix, and the daemon that refused is the party that can.
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
/// running when its socket dies. The CLI deliberately uses a bare
/// [`Client`] instead — see this module's own docs for why that is a
/// separate type rather than a flag.
///
/// # Example
///
/// ```no_run
/// use shep_client::{ReconnectingClient, shep_core::protocol::Request};
///
/// # async fn dog(socket: &std::path::Path) -> Result<(), Box<dyn core::error::Error>> {
/// let client = ReconnectingClient::connect(socket).await?;
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

/// Manual, not derived (IR-41). The socket path and the [`HelloAck`] are
/// both already printed by [`Client`]'s own `Debug`, and neither carries a
/// secret — a control-socket path is an operator-visible filename, and an
/// ack is a version, a protocol number and a pid. [`LinkState`] is printed
/// in full for the same reason: its one payload is the daemon's own
/// refusal sentence. Manual only because [`RwLock`] and [`JoinHandle`]
/// derive into noise, and because a derived impl would print the *inner*
/// [`Client`] on every line where the link state is the useful fact.
impl fmt::Debug for ReconnectingClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReconnectingClient")
            .field("socket", &self.shared.socket)
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
    /// Nothing inside a critical section here can panic — every one is a
    /// clone or an assignment of a plain struct — so poisoning cannot
    /// signal a torn value, and `unwrap()` would turn an impossible
    /// condition into a panic in a library.
    fn read(&self) -> RwLockReadGuard<'_, State> {
        self.state.read().unwrap_or_else(PoisonError::into_inner)
    }

    /// A write guard. See [`Self::read`] for the poisoning argument.
    fn write(&self) -> RwLockWriteGuard<'_, State> {
        self.state.write().unwrap_or_else(PoisonError::into_inner)
    }

    /// The current generation, cloned out so the guard is dropped before
    /// any caller awaits on it — no lock is ever held across an `await`.
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
    /// See [`Self::connect_with_timeout`] — every error variant it can
    /// return, this returns unchanged.
    pub async fn connect(socket: &Path) -> Result<Self, ConnectError> {
        Self::connect_with_timeout(socket, HANDSHAKE_TIMEOUT).await
    }

    /// As [`Self::connect`], but with a caller-supplied handshake timeout,
    /// used for the first connection and for every reconnect after it.
    ///
    /// # Errors
    ///
    /// - [`ConnectError::Connect`] — `connect(2)` failed; nothing is
    ///   listening at `socket`.
    /// - [`ConnectError::Wire`] — the `Hello` failed to encode, or the
    ///   reply frame failed to decode.
    /// - [`ConnectError::Io`] — a framed read or write failed after connect.
    /// - [`ConnectError::HandshakeClosed`] — the peer closed the connection
    ///   before sending a `HelloReply`.
    /// - [`ConnectError::HandshakeTimeout`] — connect succeeded but no
    ///   `HelloReply` (or close) arrived within `timeout`.
    /// - [`ConnectError::ProtocolMismatch`] — the daemon refused the
    ///   handshake on protocol-version skew.
    pub async fn connect_with_timeout(
        socket: &Path,
        timeout: Duration,
    ) -> Result<Self, ConnectError> {
        let client = Client::connect_with_timeout(socket, timeout).await?;
        let shared = Arc::new(Shared {
            socket: socket.to_path_buf(),
            handshake_timeout: timeout,
            state: RwLock::new(State {
                client: Arc::new(client),
                link: LinkState::Connected,
            }),
        });
        let supervisor = tokio::spawn(supervise(Arc::clone(&shared)));
        Ok(Self { shared, supervisor })
    }

    /// The handshake acknowledgement of the daemon this client is talking
    /// to **right now**.
    ///
    /// Owned rather than borrowed, unlike [`Client::daemon`]: the ack
    /// belongs to a generation that a reconnect can replace at any moment,
    /// so handing out a reference would either pin a stale one or need a
    /// guard in the caller's hands. Reading it through the current
    /// generation is also the only correct answer — a cached ack would
    /// describe the predecessor, which is exactly what a dog publishing
    /// `daemon_version` must not do.
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

    /// Sends `body` with `deadline` on the current generation of the
    /// connection.
    ///
    /// **Never retried.** A request that was on the wire when the daemon
    /// was replaced fails, and a request issued while the supervisor is
    /// still reconnecting fails too. Only the connection is re-established;
    /// the caller decides what to do about the request, because only the
    /// caller knows whether sending it twice is safe.
    ///
    /// # Errors
    ///
    /// - [`RequestError::Rpc`] — the daemon answered with a structured error.
    /// - [`RequestError::Timeout`] — no reply within the request's budget.
    /// - [`RequestError::Closed`] — the connection closed before a reply
    ///   arrived, or had already closed when this was issued.
    /// - [`RequestError::Wire`] — `body` failed to encode.
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
    /// **The returned stream belongs to one generation and is not re-armed
    /// across a reconnect**: it ends when that connection dies, the same as
    /// [`Client::subscribe`]'s would. A consumer that wants events past a
    /// handover subscribes again after the stream ends. Re-arming it inside
    /// this type would silently swallow the gap between the connection
    /// dying and the successor accepting a new `Subscribe`, and an event
    /// stream that hides a gap is worse than one that ends.
    ///
    /// # Errors
    ///
    /// Same as [`Self::request`]: a daemon-side error, a client-side
    /// timeout, a closed connection, or a wire encode failure.
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
        // Held only for the await on this generation's death — dropped
        // before the reconnect, so the dead client's socket goes away as
        // soon as `install` replaces it rather than being pinned here.
        let generation = shared.client();
        generation.closed().await;
        drop(generation);
        shared.set_link(LinkState::Reconnecting);

        let mut delay = RECONNECT_MIN_DELAY;
        loop {
            match Client::connect_with_timeout(&shared.socket, shared.handshake_timeout).await {
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

    /// Every bounded wait in this module uses one budget. Generous against
    /// a loaded CI runner, and small enough that a genuinely stuck test
    /// fails rather than hanging the suite (IR-46: every `await` here has a
    /// forcing mechanism, and this is it).
    const BOUND: Duration = Duration::from_secs(5);

    /// The window a test watches for something that must NOT happen.
    ///
    /// Eight times [`RECONNECT_MIN_DELAY`], so a supervisor that were still
    /// looping would have made several further attempts inside it — the
    /// first of them after 50ms. A negative assertion is only as good as
    /// its window, so this is stated against the delay it has to outrun
    /// rather than picked as a round number.
    const NEGATIVE_WINDOW: Duration = Duration::from_millis(400);

    /// An ack distinguishable per generation, so a test can tell WHICH
    /// daemon answered rather than only that one did.
    fn ack_from(pid: u32) -> HelloAck {
        HelloAck {
            daemon_version: format!("0.0.{pid}"),
            protocol: PROTOCOL_VERSION,
            pid,
        }
    }

    /// Waits until the fake has accepted `accepts` connections AND the
    /// client has installed the newest of them, or fails inside [`BOUND`].
    /// The reconnect is driven by a background task, so there is no handle
    /// to await; polling against a real condition with a hard ceiling is
    /// the forcing mechanism.
    ///
    /// Both halves are needed and neither alone would do. The accept count
    /// alone rises before the handshake completes, so a request issued on
    /// it could still meet the dead generation; the link alone still reads
    /// `Connected` in the instant after a cut, before the supervisor has
    /// noticed. Together they are unambiguous, because the supervisor sets
    /// `Reconnecting` BEFORE it dials — so an accept count that has moved
    /// and a link back at `Connected` can only mean the fresh generation is
    /// installed.
    ///
    /// Deliberately does NOT observe [`ReconnectingClient::daemon`]: the
    /// ack is what one of these tests is asserting about, and a helper that
    /// waited on it would make every other test fail for that test's
    /// reason.
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
    /// replaced. This is the measured defect: over six real reloads the
    /// metrics dog kept its pid, reported zero restarts and answered 503 to
    /// every scrape, because only the LISTENING socket crosses the exec and
    /// an accepted one dies with the image.
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
    /// reconnect. A dog publishing `daemon_version` from a cached ack —
    /// which the metrics dog does — would report a daemon that is no longer
    /// running.
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

    /// fails if a request that was in flight when the daemon was replaced
    /// is re-sent to the successor. A re-issued `Stop` stops a sheep twice,
    /// and the client cannot tell a request the daemon never saw from one
    /// it received and acted on before the image swapped — so the rule is
    /// the whole of the design's H2: in-flight requests fail, never retry.
    ///
    /// The proof is positional rather than a count: the successor's FIRST
    /// envelope must be the request issued after the reconnect. A retry
    /// would put the abandoned `Ping` there instead.
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

        // The predecessor reads this envelope and dies without answering
        // it — a request the daemon may or may not have acted on.
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
    /// forever. A dog that exits, or a test that finishes, must not leave a
    /// task dialling an address nobody will answer and pinning the last
    /// generation's socket open with it.
    ///
    /// Asserts a NEGATIVE, so the bound is the assertion: the accept count
    /// must NOT grow within it.
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
