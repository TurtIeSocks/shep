//! The control transport: one address, two implementations
//!
//! shep's control plane is a length-delimited framed byte stream between a
//! client and the shepherd. [`protocol`](crate::protocol) owns what travels
//! over it; this module owns what carries it, and it is the only place in
//! the workspace that names an OS transport type.
//!
//! On unix that carrier is an `AF_UNIX` socket. On Windows it is a named
//! pipe. Both are byte streams with the same framing, so everything above
//! this module — the codec, the handshake, the actor, every RPC verb — is
//! identical on both platforms and carries no `cfg` at all. That is the
//! whole point of the seam: the platform difference is spent once, here.
//!
//! # The address
//!
//! Both halves take a [`Path`], which [`ShepPaths::socket`](crate::paths::ShepPaths::socket)
//! resolves per-platform: a real socket file under `$SHEP_HOME/run` on unix,
//! and the `\\.\pipe\shep-<home>` name on Windows. A pipe name is
//! path-*shaped* but is not a filesystem path — nothing may take its
//! `parent()` or ask whether it `exists()`.
//!
//! # What differs, and why it is not hidden
//!
//! A `UnixListener` accepts repeatedly from one descriptor. A named pipe
//! server instance is *consumed* by the client that connects to it, so the
//! server must create a fresh instance for the next caller. [`Listener`]
//! absorbs that difference behind one `accept()`, which is why it takes
//! `&mut self` where a bare `UnixListener::accept` needs only `&self`.
//!
//! # Security
//!
//! The unix tier's primary access control is the `0700` on `$SHEP_HOME/run`
//! — a peer that cannot traverse the directory cannot reach the socket at
//! all — with the same-uid `peer_cred` check behind it as a second layer.
//! `shep-daemon`'s `RpcServer` carries the canonical writeup.
//!
//! **Windows answers the same question with the pipe's own ACL, and the
//! shape of the answer is different enough to state plainly.** A named pipe
//! created with a default security descriptor grants full control to the
//! creating user, `LocalSystem` and `Administrators`, and grants *read*
//! access to `Everyone`. It does not grant write access to `Everyone`, and
//! that asymmetry is what makes the posture defensible: a client must open
//! the pipe for **both** read and write to speak this protocol at all,
//! because the daemon sends nothing before it has received a `Hello`. A
//! foreign local user's open for write is refused by the OS at `CreateFile`
//! time — fail-closed, before a single byte reaches shep's own code — and
//! an open for read alone yields a connection the daemon never writes to.
//!
//! Two consequences are worth naming rather than leaving to be discovered:
//!
//! - There is **no post-accept peer check on Windows**, and none is needed
//!   for the same-user question, because the OS already answered it at open
//!   time. The unix tier's two layers collapse into one that is enforced
//!   earlier. Establishing the *identity* of an already-admitted peer would
//!   need `ImpersonateNamedPipeClient` and a token-SID comparison, which is
//!   raw FFI this crate's `#![forbid(unsafe_code)]` does not permit; it is
//!   not built because it would be a second answer to a settled question,
//!   not because it was overlooked.
//! - Administrators can reach the pipe. So can they reach a `0700`
//!   directory, and shep's unix writeup already lists root as an explicit
//!   non-goal, so this is parity rather than a regression.
//!
//! [`Listener::bind`] additionally sets `reject_remote_clients`, so the pipe
//! is unreachable over SMB from another machine. Tokio defaults it on; it is
//! set explicitly here because it is load-bearing and a default that matters
//! should be visible at the call site.

use std::io;
use std::path::Path;

/// How long a contended connect waits before retrying.
///
/// Windows only, and it is not a timeout — the caller's own budget bounds
/// the loop (see [`connect`]). Short enough that a server between instances
/// is not noticeably slower to reach than one already waiting.
#[cfg(windows)]
const PIPE_BUSY_RETRY: std::time::Duration = std::time::Duration::from_millis(20);

/// Windows' `ERROR_PIPE_BUSY`: the pipe exists but every server instance is
/// already spoken for. Transient by definition — the server creates the next
/// instance immediately after accepting — so it is retried, never surfaced.
///
/// Hardcoded rather than pulled from `windows-sys`: this crate has no
/// Windows-only dependency, and one stable, well-known error code does not
/// earn it one. The same reasoning [`kv`](crate::kv) applies to
/// `ERROR_SHARING_VIOLATION`.
#[cfg(windows)]
const ERROR_PIPE_BUSY: i32 = 231;

/// The connected stream a **client** holds.
///
/// A concrete type alias rather than a `Box<dyn AsyncRead + AsyncWrite>`:
/// there is exactly one implementation per platform, chosen at compile time,
/// so a trait object would cost a vtable and an allocation to express a
/// choice that was already made.
#[cfg(unix)]
pub type ClientStream = tokio::net::UnixStream;
/// The connected stream a **client** holds.
#[cfg(windows)]
pub type ClientStream = tokio::net::windows::named_pipe::NamedPipeClient;

/// The connected stream the **daemon** holds for one accepted peer.
///
/// The same type as [`ClientStream`] on unix, where a socketpair's two ends
/// are indistinguishable; a distinct type on Windows, where the server end
/// of a pipe is its own type. Keeping them separately named means neither
/// side's code has to know which case it is in.
#[cfg(unix)]
pub type ServerStream = tokio::net::UnixStream;
/// The connected stream the **daemon** holds for one accepted peer.
#[cfg(windows)]
pub type ServerStream = tokio::net::windows::named_pipe::NamedPipeServer;

/// The reading half of a [`ServerStream`], after [`split`].
pub type ServerReadHalf = tokio::io::ReadHalf<ServerStream>;
/// The writing half of a [`ServerStream`], after [`split`].
pub type ServerWriteHalf = tokio::io::WriteHalf<ServerStream>;

/// Splits an accepted connection into halves that can be owned by two tasks.
///
/// The daemon reads requests on one task and writes replies (and pushed bus
/// events) on another, so the two halves must be separately owned and
/// `'static`.
///
/// [`tokio::io::split`] rather than `UnixStream::into_split`, which exists
/// only on unix. The generic split coordinates the two halves through an
/// internal lock where `into_split` needs none, so this trades a small,
/// uncontended synchronisation cost on every frame for one code path on both
/// platforms. That is the right trade here and would not be everywhere: this
/// is a control plane carrying operator RPC — a handful of frames per
/// command — not a data path. Do not copy the reasoning to one.
#[must_use]
pub fn split(stream: ServerStream) -> (ServerReadHalf, ServerWriteHalf) {
    tokio::io::split(stream)
}

/// A connected `(daemon side, client side)` pair over the real platform
/// transport.
///
/// For tests that need a live connection without standing up a daemon. It is
/// a **real** transport on both platforms, not an in-memory duplex: on unix a
/// socketpair, on Windows an actual named pipe with a unique name. That
/// matters, because the thing most worth testing at this layer is behaviour
/// that an in-memory pipe would not reproduce — a peer closing mid-frame, a
/// half-open connection, the exact error a dead peer's write produces.
///
/// The Windows arm picks its own name rather than taking one, so callers
/// need no address and no tempdir; process id plus a monotonic counter keeps
/// concurrent tests off each other in a namespace that is machine-global.
///
/// # Errors
///
/// Whatever the OS says while creating or connecting the pair.
pub async fn connected_pair() -> io::Result<(ServerStream, ClientStream)> {
    #[cfg(unix)]
    {
        tokio::net::UnixStream::pair()
    }
    #[cfg(windows)]
    {
        use core::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);

        let name = std::path::PathBuf::from(format!(
            r"\\.\pipe\shep-pair-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let mut listener = Listener::bind(&name)?;
        // Concurrently, not in sequence: `accept` does not resolve until a
        // client attaches, and `connect` cannot attach until the server is
        // waiting, so awaiting either one alone would deadlock.
        tokio::try_join!(listener.accept(), connect(&name))
    }
}

/// Dials the shepherd at `addr`.
///
/// # Errors
///
/// Whatever the OS says. The one case handled rather than returned is
/// Windows' `ERROR_PIPE_BUSY`, which means the pipe exists but every server
/// instance is currently serving someone: that is transient and is retried.
///
/// **This loop is deliberately unbounded, and is safe only because every
/// caller bounds it.** `shep-client`'s `Connection::open` wraps the whole
/// connect-plus-handshake in one `tokio::time::timeout`, so a pipe that
/// stays busy forever surfaces as that layer's `HandshakeTimeout` — the same
/// error a unix socket that is bound but never accepted from produces, which
/// is the behaviour the two platforms should share. A bound here as well
/// would be a second, quieter deadline competing with it.
pub async fn connect(addr: &Path) -> io::Result<ClientStream> {
    #[cfg(unix)]
    {
        tokio::net::UnixStream::connect(addr).await
    }
    #[cfg(windows)]
    {
        use tokio::net::windows::named_pipe::ClientOptions;
        loop {
            match ClientOptions::new().open(addr) {
                Ok(client) => return Ok(client),
                Err(err) if err.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                    tokio::time::sleep(PIPE_BUSY_RETRY).await;
                }
                Err(err) => return Err(err),
            }
        }
    }
}

/// The listening half of the control transport.
///
/// Holds a bound `UnixListener` on unix, and on Windows the *next* idle
/// named pipe server instance — see [`Self::accept`] for why that is one
/// instance rather than a listener.
#[derive(Debug)]
pub struct Listener {
    #[cfg(unix)]
    listener: tokio::net::UnixListener,
    /// The idle instance the next client will connect to, taken on every
    /// accept and replaced with a fresh one.
    ///
    /// `None` only between an accept handing out its connection and the
    /// replacement being created, and across an accept whose replacement
    /// could not be created at all. [`Self::accept`] makes one when it
    /// finds the slot empty, which is what keeps a single failed `create`
    /// from ending the daemon's ability to serve anyone.
    #[cfg(windows)]
    server: Option<tokio::net::windows::named_pipe::NamedPipeServer>,
    /// The pipe name, kept so each replacement instance can be created on
    /// the same name.
    #[cfg(windows)]
    addr: std::ffi::OsString,
}

impl Listener {
    /// Binds the control transport at `addr`.
    ///
    /// # Errors
    ///
    /// Whatever the OS says: the socket path is unwritable or already bound
    /// on unix, the pipe name is already owned on Windows.
    ///
    /// # Single instance
    ///
    /// The Windows arm passes `first_pipe_instance(true)`, which makes this
    /// call itself the daemon's mutual exclusion: a second shepherd trying
    /// the same `$SHEP_HOME` fails here with `ERROR_ACCESS_DENIED` rather
    /// than quietly creating a second instance of the same pipe and stealing
    /// half the connections. That is a stronger guarantee than the unix
    /// arm's, where binding over a stale socket file is possible and the
    /// pidfile lock is what actually excludes a second daemon — and it is
    /// free, which is why it is used rather than reproducing the pidfile
    /// dance on a platform that does not need it.
    ///
    /// It also removes the stale-socket problem entirely instead of solving
    /// it: a pipe has no directory entry, so a daemon that died leaves
    /// nothing behind to recover from. There is no Windows equivalent of the
    /// "connect, fail, unlink, rebind" sequence, because there is nothing to
    /// unlink.
    pub fn bind(addr: &Path) -> io::Result<Self> {
        #[cfg(unix)]
        {
            Ok(Self {
                listener: tokio::net::UnixListener::bind(addr)?,
            })
        }
        #[cfg(windows)]
        {
            use tokio::net::windows::named_pipe::ServerOptions;
            let server = ServerOptions::new()
                .first_pipe_instance(true)
                .reject_remote_clients(true)
                .create(addr)?;
            Ok(Self {
                server: Some(server),
                addr: addr.as_os_str().to_os_string(),
            })
        }
    }

    /// Wraps a socket this process was handed rather than one it bound.
    ///
    /// The successor's half of a daemon handover. The control socket is one
    /// of the descriptors an outgoing shepherd passes across its `execve`,
    /// so the image that takes over adopts the listener instead of binding
    /// the address again: a rebind would race the predecessor's socket file
    /// and lose whatever connection a client had already made.
    ///
    /// Unix only, and the whole handover is. Windows has no `execve`, and
    /// its arm of [`Self::bind`] makes the bind itself the daemon's mutual
    /// exclusion, so a second image could not create the pipe to hand on in
    /// the first place.
    #[cfg(unix)]
    #[must_use]
    pub fn from_unix_listener(listener: tokio::net::UnixListener) -> Self {
        Self { listener }
    }

    /// Waits for the next peer and returns its connected stream.
    ///
    /// # Errors
    ///
    /// Whatever the OS says. A transient failure is the caller's to log and
    /// continue from — one bad accept must not end a daemon.
    ///
    /// # Cancellation safety
    ///
    /// Safe on both platforms, which the daemon's accept loop depends on: it
    /// `select!`s this against a shutdown watch, and a cancelled accept must
    /// not drop a peer that was mid-connect. `UnixListener::accept` and
    /// `NamedPipeServer::connect` are both documented cancel-safe, and the
    /// Windows arm's instance swap happens only *after* `connect` has
    /// resolved, so a cancellation cannot leave this holding an instance
    /// that a client has already been handed.
    ///
    /// # Why `&mut self`
    ///
    /// A named pipe server instance is consumed by whoever connects to it:
    /// once a client is attached, that instance *is* the connection and can
    /// never accept again. So accepting means handing out the instance we
    /// were holding and creating the next one, which needs exclusive access.
    /// The unix arm needs only `&self` and takes `&mut self` anyway, so both
    /// platforms present one signature.
    pub async fn accept(&mut self) -> io::Result<ServerStream> {
        #[cfg(unix)]
        {
            let (stream, _addr) = self.listener.accept().await?;
            Ok(stream)
        }
        #[cfg(windows)]
        {
            use tokio::net::windows::named_pipe::ServerOptions;
            // The slot is empty only if a previous accept could not create
            // a replacement. Make one now rather than at that failure, so a
            // transient `create` error costs one accept instead of the
            // daemon's whole ability to serve.
            if self.server.is_none() {
                self.server = Some(
                    ServerOptions::new()
                        .reject_remote_clients(true)
                        .create(&self.addr)?,
                );
            }
            // Resolves when a client attaches to the instance we hold.
            let Some(server) = self.server.as_ref() else {
                unreachable!("the slot was just filled")
            };
            server.connect().await?;
            // Out of the slot BEFORE anything that can fail. Once a client
            // has attached, this instance IS that connection and can never
            // accept again, so leaving it in place would mean the next
            // accept calling `connect` on a connected instance: Windows
            // answers that with ERROR_PIPE_CONNECTED or ERROR_NO_DATA
            // rather than waiting, and the daemon's accept loop, which
            // logs an error and carries on, would spin on it forever
            // instead of serving anyone.
            let connected = self
                .server
                .take()
                .unwrap_or_else(|| unreachable!("the slot was just filled"));
            // `first_pipe_instance` is deliberately NOT set here: it is set
            // once, at `bind`, and setting it again would refuse to create
            // the very instance this listener already owns the name for.
            //
            // A failure here leaves the slot empty and the peer connected,
            // which is the right way round. The alternative, `?` before the
            // handoff, drops a peer that has already attached AND leaves a
            // connected instance in the slot for the next accept to trip
            // over. The error is not lost: the next accept re-creates and
            // reports it, having handed this peer over first.
            self.server = ServerOptions::new()
                .reject_remote_clients(true)
                .create(&self.addr)
                .ok();
            Ok(connected)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    /// A control address that is valid on the platform running the test:
    /// a file inside `dir` on unix, a uniquely-named pipe on Windows (where
    /// `dir` is irrelevant, since a pipe name is not a filesystem path).
    ///
    /// `tag` keeps concurrently running tests off each other's address —
    /// the pipe namespace is machine-global, so two tests using one name
    /// would contend for real.
    fn address(dir: &Path, tag: &str) -> std::path::PathBuf {
        #[cfg(unix)]
        {
            let _ = tag;
            dir.join("shep.sock")
        }
        #[cfg(windows)]
        {
            let _ = dir;
            std::path::PathBuf::from(format!(
                r"\\.\pipe\shep-transport-test-{tag}-{}",
                std::process::id()
            ))
        }
    }

    /// fails if a listener rebuilt around an inherited socket cannot serve.
    ///
    /// The successor half of a daemon handover: the control socket is one
    /// descriptor the outgoing shepherd hands on, so its replacement binds
    /// nothing and wraps what it was given. A `bind` here instead would
    /// meet the socket file the predecessor left behind, and the connection
    /// a client had already made to it would be dropped on the floor.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_listener_built_around_an_inherited_socket_still_accepts() {
        let dir = tempfile::tempdir().unwrap();
        let addr = address(dir.path(), "adopted");
        let inherited = tokio::net::UnixListener::bind(&addr).unwrap();

        let mut listener = Listener::from_unix_listener(inherited);
        let client = tokio::spawn(async move {
            let mut stream = connect(&addr).await.unwrap();
            stream.write_all(b"still here\n").await.unwrap();
        });

        let mut served = listener.accept().await.unwrap();
        let mut said = [0_u8; 11];
        served.read_exact(&mut said).await.unwrap();
        assert_eq!(&said, b"still here\n");
        client.await.unwrap();
    }

    /// fails if the transport cannot carry bytes both ways on this platform.
    /// The most basic thing this module claims, and the one that would break
    /// silently if `ClientStream` and `ServerStream` were ever mismatched.
    #[tokio::test]
    async fn a_client_and_the_daemon_exchange_bytes_over_the_platform_transport() {
        let dir = tempfile::tempdir().unwrap();
        let addr = address(dir.path(), "roundtrip");
        let mut listener = Listener::bind(&addr).unwrap();

        let server = tokio::spawn(async move {
            let mut stream = listener.accept().await.unwrap();
            let mut buf = [0u8; 5];
            stream.read_exact(&mut buf).await.unwrap();
            stream.write_all(b"world").await.unwrap();
            stream.flush().await.unwrap();
            buf
        });

        let mut client = connect(&addr).await.unwrap();
        client.write_all(b"hello").await.unwrap();
        client.flush().await.unwrap();
        let mut reply = [0u8; 5];
        client.read_exact(&mut reply).await.unwrap();

        assert_eq!(&server.await.unwrap(), b"hello");
        assert_eq!(&reply, b"world");
    }

    /// fails if a second connection cannot be served after the first.
    ///
    /// This is the whole reason [`Listener::accept`] takes `&mut self`: a
    /// named pipe server instance is consumed by its client, so an
    /// implementation that forgot to create a replacement would serve
    /// exactly one caller and then hang forever. On unix this is trivially
    /// true and the test costs nothing; on Windows it is the load-bearing
    /// assertion of this module.
    #[tokio::test]
    async fn the_listener_serves_more_than_one_connection() {
        let dir = tempfile::tempdir().unwrap();
        let addr = address(dir.path(), "sequential");
        let mut listener = Listener::bind(&addr).unwrap();

        let server = tokio::spawn(async move {
            let mut seen = Vec::new();
            for _ in 0..3 {
                let mut stream = listener.accept().await.unwrap();
                let mut byte = [0u8; 1];
                stream.read_exact(&mut byte).await.unwrap();
                seen.push(byte[0]);
            }
            seen
        });

        for tag in [1u8, 2, 3] {
            let mut client = connect(&addr).await.unwrap();
            client.write_all(&[tag]).await.unwrap();
            client.flush().await.unwrap();
            // Dropped here: the next iteration must get a fresh instance.
        }

        assert_eq!(server.await.unwrap(), vec![1, 2, 3]);
    }

    /// fails if dialing an address nothing is listening on reports success.
    ///
    /// The negative case the connect-retry loop could plausibly swallow: on
    /// Windows a missing pipe is `ERROR_FILE_NOT_FOUND`, which must be
    /// returned, while only `ERROR_PIPE_BUSY` is retried. A loop that
    /// retried both would hang here instead of failing.
    #[tokio::test]
    async fn dialing_an_address_with_no_listener_fails_rather_than_hanging() {
        let dir = tempfile::tempdir().unwrap();
        let addr = address(dir.path(), "absent");

        let result = tokio::time::timeout(std::time::Duration::from_secs(5), connect(&addr))
            .await
            .expect("connect must fail fast, not hang, when nothing is listening");

        assert!(result.is_err(), "no listener must not read as a connection");
    }

    /// fails if two shepherds can own one control address at once.
    ///
    /// On Windows this is `first_pipe_instance(true)` doing the daemon's
    /// mutual exclusion; a second `create` on the same name is refused by
    /// the OS. On unix `bind` refuses an address already bound. Both
    /// platforms must refuse, for the same operator-visible reason: two
    /// daemons on one `$SHEP_HOME` would split the flock between them.
    #[tokio::test]
    async fn a_second_bind_on_the_same_address_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let addr = address(dir.path(), "exclusive");
        let _first = Listener::bind(&addr).unwrap();

        assert!(
            Listener::bind(&addr).is_err(),
            "a second daemon must not be able to bind the same control address"
        );
    }
}
