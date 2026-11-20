//! Length-prefixed postcard frames: `u32` big-endian length, then the body.

use serde::{Serialize, de::DeserializeOwned};

/// Largest frame we accept (control messages are small; files use blobs).
pub const MAX_FRAME: usize = 1 << 20;

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("frame of {0} bytes exceeds the {MAX_FRAME} byte limit")]
    TooLarge(usize),
    #[error("malformed frame: {0}")]
    Malformed(#[from] postcard::Error),
}

/// Encode `msg` as a complete frame (length prefix included).
pub fn encode<T: Serialize>(msg: &T) -> Result<Vec<u8>, FrameError> {
    let body = postcard::to_stdvec(msg)?;
    if body.len() > MAX_FRAME {
        return Err(FrameError::TooLarge(body.len()));
    }
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

/// Parse a length prefix, rejecting oversized frames before allocating.
pub fn body_len(prefix: [u8; 4]) -> Result<usize, FrameError> {
    let len = u32::from_be_bytes(prefix) as usize;
    if len > MAX_FRAME {
        return Err(FrameError::TooLarge(len));
    }
    Ok(len)
}

/// Decode a frame body (without its length prefix).
pub fn decode<T: DeserializeOwned>(body: &[u8]) -> Result<T, FrameError> {
    Ok(postcard::from_bytes(body)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let msg = (7u32, "hello".to_string());
        let frame = encode(&msg).unwrap();
        let len = body_len(frame[..4].try_into().unwrap()).unwrap();
        assert_eq!(len, frame.len() - 4);
        let back: (u32, String) = decode(&frame[4..]).unwrap();
        assert_eq!(back, msg);
    }

    #[test]
    fn rejects_oversized_prefix() {
        assert!(body_len(((MAX_FRAME + 1) as u32).to_be_bytes()).is_err());
    }

    #[test]
    fn rejects_garbage() {
        assert!(decode::<(u32, String)>(&[0xff, 0xff, 0xff]).is_err());
    }
}
