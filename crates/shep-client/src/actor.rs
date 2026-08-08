//! The connection actor: one task owning the framed transport, demuxing
//! [`ServerFrame::Reply`] to the request that asked for it and
//! [`ServerFrame::Event`] to subscribers.
//!
//! [`spawn`] is the only thing this module exposes outside itself: a
//! [`Client`](crate::client::Client) talks to its actor over the returned
//! command channel and never touches the transport directly. That
//! indirection is what lets one [`Client`](crate::client::Client) be shared
//! across concurrent callers (`&self`, not `&mut self`) despite owning a
//! single, non-cloneable framed transport underneath: two callers racing
//! `request()` both send a [`Command`] and await their own
//! [`oneshot`](tokio::sync::oneshot) reply, and only the actor ever touches
//! the socket.

use std::collections::HashMap;

use futures_util::{SinkExt, StreamExt};
use tokio::sync::{broadcast, mpsc, oneshot};

use shep_core::protocol::{
    BusEvent, Envelope, Reply, Request, Response, ServerFrame, decode_frame, encode_frame,
};

use crate::client::RequestError;
use crate::connection::Frames;

/// Capacity of the broadcast channel the actor uses to fan out
/// [`BusEvent`]s to subscribers.
///
/// Mirrors the daemon's own per-connection outbound queue (`CONN_QUEUE =
/// 64`, `shep-daemon/src/server.rs:39`): a client-side buffer smaller than
/// what the daemon itself is willing to queue before it starts dropping
/// would lag behind the daemon's own backpressure for no reason (IR-26).
///
/// Public because it is the number behind
/// [`Lagged`](crate::events::Lagged): a subscriber that falls this many
/// events behind starts losing them, so a caller sizing its own drain loop —
/// or a test proving the lag path — needs the figure rather than a guess.
pub const EVENT_CHANNEL_CAPACITY: usize = 64;

/// Depth of the actor's own command queue.
///
/// Generous rather than tuned: a full queue only makes a caller's `send`
/// await briefly for room, it never drops or fails a request, so this
/// trades a little memory for one fewer value to benchmark. Matches
/// [`EVENT_CHANNEL_CAPACITY`]'s own precedent for the same reason (IR-26).
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
    /// Routed through the actor rather than handing
    /// [`Client`](crate::client::Client) its own clone of the sender to
    /// call `.subscribe()` on directly: `broadcast` closes a channel by
    /// sender count reaching zero, never by receiver activity. A
    /// `Client`-held clone would sit alive for as long as the `Client`
    /// itself does, so the channel could never close — and every
    /// [`crate::EventStream`] with it — even after this task's own loop
    /// ends and drops the one clone it holds, which is exactly the moment a
    /// `Client` that outlives its connection needs it to. Routing through
    /// here keeps that clone the only one that ever exists outside `run`.
    Subscribe {
        /// Resolved exactly once, by the actor.
        reply_to: oneshot::Sender<broadcast::Receiver<BusEvent>>,
    },
}

/// Spawns the actor task that owns `frames`, returning the command channel
/// a [`Client`](crate::client::Client) sends [`Command`]s through.
///
/// The spawned task keeps the only [`broadcast::Sender`] this connection
/// will ever have; see [`Command::Subscribe`] for why nothing outside this
/// module ever holds a clone of it.
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
                        // A request whose caller already stopped waiting (timed
                        // out, or dropped the future) leaves a closed
                        // `oneshot::Sender` behind; swept here so an abandoned
                        // request doesn't linger in `pending` until the whole
                        // connection closes.
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

/// Assigns the next request id, encodes and writes the envelope, and — once
/// the write succeeds — records `reply_to` under that id so a later
/// [`route_frame`] call can resolve it.
///
/// Returns `false` when the write itself failed, the caller's signal to
/// stop the actor loop: the connection is no longer usable, so nothing
/// sent after this point could succeed either.
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
/// pending request with the matching id — by id, never by arrival order,
/// because the daemon may legitimately emit a [`BusEvent`] for a request
/// before it emits that same request's own reply — and a [`BusEvent`] is
/// broadcast to subscribers.
///
/// A [`Reply`] whose id has no entry in `pending` (its caller's future was
/// already cancelled) is dropped silently — there is nobody left to tell.
/// A frame that fails to decode is dropped silently too: the daemon and
/// this client share the same codec, so a bad frame here can only mean the
/// codec itself drifted, which the version handshake already guards
/// against, and there is no better recovery available at this layer than
/// to keep reading.
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
        // `ServerFrame` is `#[non_exhaustive]`: a future frame kind a newer
        // daemon sends is additive by that type's own evolution rule, and
        // an older client that doesn't understand it yet must not treat it
        // as fatal — ignored, silently, same as an unknown `BusEvent` variant.
        _ => {}
    }
}
