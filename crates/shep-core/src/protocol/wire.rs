//! Frame encoding: u32 length prefix + JSON payload
//!
//! One codec constructor + encode/decode helpers shared by daemon and
//! client so framing parameters can never drift between the two.

use core::fmt;

use bytes::Bytes;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio_util::codec::LengthDelimitedCodec;

/// Hard ceiling per frame; larger is a protocol violation
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// Builds the shared length-delimited codec (u32 BE prefix, 16 MiB cap)
#[must_use]
pub fn codec() -> LengthDelimitedCodec {
    LengthDelimitedCodec::builder()
        .length_field_type::<u32>()
        .max_frame_length(MAX_FRAME_BYTES)
        .new_codec()
}

/// Serializes one value to a frame payload
///
/// # Errors
///
/// - [`WireError::Json`] — serialization failed (carries serde's message).
/// - [`WireError::FrameTooLarge`] — payload exceeds [`MAX_FRAME_BYTES`].
pub fn encode_frame<T: Serialize>(value: &T) -> Result<Bytes, WireError> {
    let vec = serde_json::to_vec(value).map_err(|e| WireError::Json(e.to_string()))?;
    if vec.len() > MAX_FRAME_BYTES {
        return Err(WireError::FrameTooLarge(vec.len()));
    }
    Ok(Bytes::from(vec)) // zero-copy: Bytes takes the Vec's buffer
}

/// Deserializes one frame payload
///
/// # Errors
///
/// - [`WireError::Json`] — the payload is not valid JSON for `T`.
pub fn decode_frame<T: DeserializeOwned>(frame: &[u8]) -> Result<T, WireError> {
    serde_json::from_slice(frame).map_err(|e| WireError::Json(e.to_string()))
}

/// Error type returned from [`encode_frame`] and [`decode_frame`]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireError {
    /// JSON (de)serialization failed (carries the serde message)
    Json(String),
    /// Encoded payload exceeds [`MAX_FRAME_BYTES`] (carries actual size)
    FrameTooLarge(usize),
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Json(m) => write!(f, "wire frame JSON error: {m}"),
            Self::FrameTooLarge(n) => {
                write!(
                    f,
                    "frame of {n} bytes exceeds the {MAX_FRAME_BYTES}-byte limit"
                )
            }
        }
    }
}

impl core::error::Error for WireError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Envelope, Request};

    #[test]
    fn encode_decode_round_trip() {
        let env = Envelope {
            id: 9,
            deadline_ms: Some(5000),
            body: Request::Ping,
        };
        let bytes = encode_frame(&env).unwrap();
        let back: Envelope = decode_frame(&bytes).unwrap();
        assert_eq!(back, env);
    }

    #[test]
    fn decode_rejects_garbage_with_json_error() {
        assert!(matches!(
            decode_frame::<Envelope>(b"not json"),
            Err(WireError::Json(_))
        ));
    }

    #[test]
    fn codec_uses_u32_prefix_and_max_frame() {
        let c = codec();
        // 16 MiB cap per spec-adjacent sanity: a frame larger than this is a
        // protocol violation, not a legitimate message.
        assert_eq!(c.max_frame_length(), MAX_FRAME_BYTES);
    }

    #[tokio::test]
    async fn framed_stream_round_trip() {
        use futures_util::{SinkExt, StreamExt};
        use tokio_util::codec::{FramedRead, FramedWrite};

        let (client, server) = tokio::io::duplex(64 * 1024);
        let mut writer = FramedWrite::new(client, codec());
        let mut reader = FramedRead::new(server, codec());

        let env = Envelope {
            id: 1,
            deadline_ms: None,
            body: Request::ListFlock,
        };
        writer.send(encode_frame(&env).unwrap()).await.unwrap();

        let frame = reader.next().await.unwrap().unwrap();
        let back: Envelope = decode_frame(&frame).unwrap();
        assert_eq!(back, env);
    }
}
