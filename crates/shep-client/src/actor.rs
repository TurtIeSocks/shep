//! The connection actor: one task owning the framed transport, demuxing
//! [`ServerFrame::Reply`] to the request that asked for it and
//! [`ServerFrame::Event`] to subscribers.
//!
//! Only the actor touches the transport. That is what lets one
//! [`Client`](crate::client::Client) be shared across concurrent callers
//! (`&self`, not `&mut self`) despite owning a single, non-cloneable
//! [`Frames`](crate::connection::Frames): each caller sends a [`Command`]
//! and awaits its own [`oneshot`](tokio::sync::oneshot) reply.

use std::collections::HashMap;

use futures_util::{SinkExt, StreamExt};
use tokio::sync::{broadcast, mpsc, oneshot};

use shep_core::protocol::{
    BusEvent, Envelope, Reply, Request, Response, ServerFrame, decode_frame, encode_frame,
};

use crate::client::RequestError;
use crate::connection::Frames;

/// Capacity of the broadcast channel the actor fans [`BusEvent`]s out over.
///
/// Mirrors the daemon's own per-connection outbound queue (`CONN_QUEUE =
/// 64`, `shep-daemon/src/server.rs`): a smaller client-side buffer would
/// lag behind the daemon's backpressure for no reason.
pub(crate) const EVENT_CHANNEL_CAPACITY: usize = 64;

/// Depth of the actor's own command queue.
///
/// A full queue only makes a caller's `send` await for room; it never
/// drops or fails a request. Matches [`EVENT_CHANNEL_CAPACITY`].
const COMMAND_CHANNEL_CAPACITY: usize = 64;

/// One command a [`Client`](crate::client::Client) sends to its actor task.
pub(crate) enum Command {
    /// Send `body` with `deadline_ms`; resolve `reply_to` with the eventual
    /// outcome once the matching [`Reply`] arrives, or with
    /// [`RequestError::Closed`] if the connection dies first.
    Request {
        /// The request payload.
        body: Request,
        /// The deadline to put on the wire, verbatim.
        deadline_ms: Option<u64>,
        /// Resolved exactly once, by the actor.
        reply_to: oneshot::Sender<Result<Response, RequestError>>,
    },
    /// Hand back a fresh [`broadcast::Receiver`] over the actor's own
    /// [`broadcast::Sender`].
    ///
    /// `broadcast` closes a channel by sender count reaching zero, never by
    /// receiver activity, so the actor task holds the only clone: a
    /// `Client`-held one would keep every [`crate::EventStream`] open for
    /// as long as the `Client` lives.
    Subscribe {
        /// Resolved exactly once, by the actor.
        reply_to: oneshot::Sender<broadcast::Receiver<BusEvent>>,
    },
}

/// Spawns the actor task that owns `frames`, returning the command channel
/// a [`Client`](crate::client::Client) sends [`Command`]s through.
///
/// The spawned task keeps the only [`broadcast::Sender`] this connection
/// will ever have.
pub(crate) fn spawn(frames: Frames) -> mpsc::Sender<Command> {
    let (commands_tx, commands_rx) = mpsc::channel(COMMAND_CHANNEL_CAPACITY);
    let (events_tx, _events_rx) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
    tokio::spawn(run(frames, commands_rx, events_tx));
    commands_tx
}

/// The actor's own loop: `select!` between new commands and incoming
/// frames until either side ends, then fail every still-pending request
/// with [`RequestError::Closed`] rather than leave a caller parked on
/// `.await` forever.
async fn run(
    mut frames: Frames,
    mut commands: mpsc::Receiver<Command>,
    events: broadcast::Sender<BusEvent>,
) {
    let mut next_id: u64 = 1;
    let mut pending: HashMap<u64, oneshot::Sender<Result<Response, RequestError>>> = HashMap::new();

    loop {
        tokio::select! {
            command = commands.recv() => {
                match command {
                    Some(Command::Request { body, deadline_ms, reply_to }) => {
                        let alive = send_request(&mut frames, &mut next_id, &mut pending, body, deadline_ms, reply_to).await;
                        // drops entries whose caller already stopped waiting
                        pending.retain(|_, tx| !tx.is_closed());
                        if !alive {
                            break; // the write failed; the connection is dead
                        }
                    }
                    Some(Command::Subscribe { reply_to }) => {
                        // No wire traffic, no failure mode: the caller may
                        // already have stopped waiting, in which case this
                        // receiver is simply dropped unused.
                        let _ = reply_to.send(events.subscribe());
                    }
                    None => break, // every `Client` handle (and its command sender) is gone
                }
            }
            frame = frames.next() => {
                match frame {
                    Some(Ok(bytes)) => route_frame(&bytes, &mut pending, &events),
                    Some(Err(_)) | None => break, // a read failed, or the peer closed the connection
                }
            }
        }
    }

    for (_id, reply_to) in pending.drain() {
        let _ = reply_to.send(Err(RequestError::Closed)); // the caller may already have stopped listening; that's fine
    }
}

/// Assigns the next request id, encodes and writes the envelope, then
/// records `reply_to` under that id for [`route_frame`] to resolve.
///
/// Returns `false` if the write failed: the connection is dead and the
/// actor loop should stop.
async fn send_request(
    frames: &mut Frames,
    next_id: &mut u64,
    pending: &mut HashMap<u64, oneshot::Sender<Result<Response, RequestError>>>,
    body: Request,
    deadline_ms: Option<u64>,
    reply_to: oneshot::Sender<Result<Response, RequestError>>,
) -> bool {
    let id = *next_id;
    *next_id += 1;
    let envelope = Envelope {
        id,
        deadline_ms,
        body,
    };
    let payload = match encode_frame(&envelope) {
        Ok(payload) => payload,
        Err(err) => {
            let _ = reply_to.send(Err(RequestError::Wire(err)));
            return true; // this one request failed to encode; the connection itself is still fine
        }
    };
    if frames.send(payload).await.is_err() {
        let _ = reply_to.send(Err(RequestError::Closed));
        return false;
    }
    pending.insert(id, reply_to);
    true
}

/// Decodes one frame off the wire and routes it: a [`Reply`] resolves the
/// pending request with the matching id, and a [`BusEvent`] is broadcast
/// to subscribers. Matched by id rather than arrival order, since the
/// daemon may emit a [`BusEvent`] for a request before that request's own
/// reply.
///
/// A [`Reply`] whose id has no entry in `pending`, or a frame that fails
/// to decode, is dropped silently: there is nobody left to tell, and no
/// better recovery is available at this layer than to keep reading.
fn route_frame(
    bytes: &[u8],
    pending: &mut HashMap<u64, oneshot::Sender<Result<Response, RequestError>>>,
    events: &broadcast::Sender<BusEvent>,
) {
    let Ok(frame) = decode_frame::<ServerFrame>(bytes) else {
        return;
    };
    match frame {
        ServerFrame::Reply(Reply { id, result }) => {
            if let Some(reply_to) = pending.remove(&id) {
                let _ = reply_to.send(result.map_err(RequestError::Rpc));
            }
        }
        ServerFrame::Event(event) => {
            let _ = events.send(event); // Err means no subscribers yet; the event is simply dropped
        }
        // `ServerFrame` is `#[non_exhaustive]`: an unknown variant is ignored, not fatal.
        _ => {}
    }
}
