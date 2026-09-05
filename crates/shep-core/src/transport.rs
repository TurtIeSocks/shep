//! The control transport: one address, two implementations
//!
//! shep's control plane is a length-delimited framed byte stream: an
//! `AF_UNIX` socket on unix, a named pipe on Windows. The only module
//! naming an OS transport type; everything above carries no `cfg`.
//!
//! The address is a [`Path`], but the Windows pipe name is only
//! path-*shaped*: never take its `parent()` or ask `exists()`.
//!
//! Security: unix relies on `0700` on `$SHEP_HOME/run` plus a same-uid
//! `peer_cred` check; Windows relies on the pipe's ACL, which grants
//! `Everyone` read but not write (canonical writeup: shep-daemon's `RpcServer`).

use std::io;
use std::path::Path;

/// How long a contended connect waits before retrying.
///
/// Windows only, and not a timeout: the caller's own budget bounds the
/// loop (see [`connect`]). Short enough that a server between instances is
/// not noticeably slower to reach than one already waiting.
#[cfg(windows)]
const PIPE_BUSY_RETRY: std::time::Duration = std::time::Duration::from_millis(20);

/// Windows' `ERROR_PIPE_BUSY`: the pipe exists but every server instance is
/// already spoken for. Transient by definition, since the server creates
/// the next instance immediately after accepting, so it is retried, never
/// surfaced.
///
/// Hardcoded rather than pulled from `windows-sys`: this crate has no
/// Windows-only dependency, and one stable, well-known error code does not
/// earn it one. The same reasoning [`kv`](crate::kv) applies to
/// `ERROR_SHARING_VIOLATION`.
#[cfg(windows)]
const ERROR_PIPE_BUSY: i32 = 231;

/// The connected stream a client holds.
///
/// A concrete type alias rather than a `Box<dyn AsyncRead + AsyncWrite>`:
/// there is exactly one implementation per platform, chosen at compile time,
/// so a trait object would cost a vtable and an allocation to express a
/// choice that was already made.
#[cfg(unix)]
pub type ClientStream = tokio::net::UnixStream;
/// The connected stream a client holds.
#[cfg(windows)]
pub type ClientStream = tokio::net::windows::named_pipe::NamedPipeClient;

/// The connected stream the daemon holds for one accepted peer.
///
/// The same type as [`ClientStream`] on unix, where a socketpair's two ends
/// are indistinguishable; a distinct type on Windows, where the server end
/// of a pipe is its own type. Keeping them separately named means neither
/// side's code has to know which case it is in.
#[cfg(unix)]
pub type ServerStream = tokio::net::UnixStream;
/// The connected stream the daemon holds for one accepted peer.
#[cfg(windows)]
pub type ServerStream = tokio::net::windows::named_pipe::NamedPipeServer;

/// The reading half of a [`ServerStream`], after [`split`].
pub type ServerReadHalf = tokio::io::ReadHalf<ServerStream>;
/// The writing half of a [`ServerStream`], after [`split`].
pub type ServerWriteHalf = tokio::io::WriteHalf<ServerStream>;

/// Splits an accepted connection into halves that can be owned by two tasks.
///
/// The daemon reads requests on one task and writes replies on another, so
/// the two halves must be separately owned and `'static`.
///
/// [`tokio::io::split`], not `UnixStream::into_split` (unix-only): it costs
/// a small lock per frame in exchange for one code path on both platforms.
/// Fine for a control plane's handful of frames per command; do not copy
/// the tradeoff onto a data path.
#[must_use]
pub fn split(stream: ServerStream) -> (ServerReadHalf, ServerWriteHalf) {
    tokio::io::split(stream)
}

/// A connected `(daemon side, client side)` pair over the real transport.
///
/// For tests that need a live connection without standing up a daemon,
/// real on both platforms rather than an in-memory duplex, so it
/// reproduces behavior a duplex would not: a peer closing mid-frame, a
/// half-open connection, the exact error a dead peer's write produces. The
/// Windows arm picks its own name (process id plus a monotonic counter),
/// so callers need no address and no tempdir.
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
/// Whatever the OS says. Windows' `ERROR_PIPE_BUSY` is retried rather than
/// returned: it means every server instance is busy, which is transient.
///
/// This loop is unbounded and safe only because every caller bounds it:
/// `shep-client`'s `Connection::open` wraps connect-plus-handshake in one
/// `tokio::time::timeout`, so a pipe stuck busy surfaces as a
/// `HandshakeTimeout`, matching a unix socket that is bound but never
/// accepted from.
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
/// named pipe server instance: see [`Self::accept`] for why that is one
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
    /// The Windows arm passes `first_pipe_instance(true)`, making this call
    /// itself the daemon's mutual exclusion: a second shepherd on the same
    /// `$SHEP_HOME` fails here with `ERROR_ACCESS_DENIED`. Unix instead
    /// relies on the pidfile lock, since a stale socket file can be bound
    /// over; a pipe has no directory entry to leave stale.
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
                // Explicit though tokio defaults it on: keeps the pipe
                // unreachable over SMB from another machine.
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

    /// The descriptor this listener is bound on.
    ///
    /// The predecessor's half of a daemon handover, and the counterpart to
    /// [`Self::from_unix_listener`]: an outgoing shepherd has to name this
    /// number in the blob it hands on, since a descriptor number is only
    /// meaningful in the process that owns it and the successor adopts it by
    /// number. Borrowed, never owned: closing it would close the control
    /// socket out from under a daemon that is still serving.
    ///
    /// Unix only, as the whole handover is.
    #[cfg(unix)]
    #[must_use]
    pub fn as_raw_fd(&self) -> std::os::fd::RawFd {
        use std::os::fd::AsRawFd as _;
        self.listener.as_raw_fd()
    }

    /// Waits for the next peer and returns its connected stream.
    ///
    /// # Errors
    ///
    /// Whatever the OS says; a transient failure is the caller's to log
    /// and continue from.
    ///
    /// Cancel-safe: the Windows instance swap happens only after `connect`
    /// resolves, so a cancelled `select!` cannot orphan an already-connected
    /// peer. `&mut self` because a pipe instance is consumed by its client;
    /// unix takes `&mut` too, for one signature on both platforms.
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
            // Out of the slot before anything fallible: leaving a connected
            // instance in place would make the next accept's `connect`
            // spin forever on ERROR_PIPE_CONNECTED/ERROR_NO_DATA.
            let connected = self
                .server
                .take()
                .unwrap_or_else(|| unreachable!("the slot was just filled"));
            // `first_pipe_instance` stays unset here: set once at `bind`,
            // setting it again would refuse to recreate this listener's
            // own instance. A failure here leaves the slot empty (peer
            // already handed over); the next accept retries creation.
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
    /// `tag` keeps concurrently running tests off each other's address:
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

    /// The successor half of a daemon handover: the control socket is one
    /// descriptor the outgoing shepherd hands on, so its replacement binds
    /// nothing and wraps what it was given. A `bind` here instead would
    /// meet the socket file the predecessor left behind, and drop the
    /// connection a client had already made to it.
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

        // Every await bounded: an unbounded hang here would stop the whole
        // test binary rather than failing just this test.
        let bound = std::time::Duration::from_secs(10);
        let mut served = tokio::time::timeout(bound, listener.accept())
            .await
            .expect("an adopted listener must accept")
            .unwrap();
        let mut said = [0_u8; 11];
        tokio::time::timeout(bound, served.read_exact(&mut said))
            .await
            .expect("the bytes the client wrote must arrive")
            .unwrap();
        assert_eq!(&said, b"still here\n");
        tokio::time::timeout(bound, client)
            .await
            .expect("the client task must finish")
            .unwrap();
    }

    /// The most basic thing this module claims, and the one that would
    /// break silently if `ClientStream` and `ServerStream` were mismatched.
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

    /// The whole reason [`Listener::accept`] takes `&mut self`: a named
    /// pipe server instance is consumed by its client, so an implementation
    /// that forgot to create a replacement would serve exactly one caller
    /// and hang forever. Trivially true on unix; the load-bearing assertion
    /// on Windows.
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

    /// The negative case the connect-retry loop could plausibly swallow: on
    /// Windows a missing pipe is `ERROR_FILE_NOT_FOUND`, which must be
    /// returned, while only `ERROR_PIPE_BUSY` is retried.
    #[tokio::test]
    async fn dialing_an_address_with_no_listener_fails_rather_than_hanging() {
        let dir = tempfile::tempdir().unwrap();
        let addr = address(dir.path(), "absent");

        let result = tokio::time::timeout(std::time::Duration::from_secs(5), connect(&addr))
            .await
            .expect("connect must fail fast, not hang, when nothing is listening");

        assert!(result.is_err(), "no listener must not read as a connection");
    }

    /// On Windows this is `first_pipe_instance(true)` refusing a second
    /// `create` on the same name; on unix `bind` refuses an address already
    /// bound. Both must refuse: two daemons on one `$SHEP_HOME` would split
    /// the flock between them.
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
