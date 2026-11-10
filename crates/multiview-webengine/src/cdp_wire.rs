//! Bounded Chrome `DevTools` pipe framing for management workers.
//!
//! Chromium's debugging pipe uses NUL-terminated UTF-8 JSON messages. This
//! module performs no I/O and never logs protocol payloads: messages can contain
//! credentials, cookies and page content. A framing error poisons the decoder;
//! the owner must close the browser session instead of resynchronizing secrets.

use serde_json::Value;
use thiserror::Error;

/// Maximum size of one encoded message, excluding its terminating NUL.
/// This accommodates a bounded screenshot while preventing unlimited growth.
pub const DEFAULT_MESSAGE_LIMIT: usize = 16 * 1024 * 1024;

/// A fatal protocol framing error. Payload bytes are deliberately excluded.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum WireError {
    /// An encoded message exceeded the configured byte bound.
    #[error("CDP message exceeds its byte limit")]
    TooLarge,
    /// Empty messages are not valid CDP JSON.
    #[error("CDP message is empty")]
    Empty,
    /// Invalid JSON or non-object protocol payload.
    #[error("CDP message is not a JSON object")]
    InvalidMessage,
    /// The caller attempted to use a decoder after a fatal framing error.
    #[error("CDP decoder is poisoned")]
    Poisoned,
    /// EOF arrived in the middle of a message.
    #[error("CDP pipe closed with an incomplete message")]
    Truncated,
}

/// Stateful decoder with an explicit per-message bound.
///
/// Consume one byte at a time with [`Self::push`]. This intentionally avoids
/// accumulating an unbounded collection of events from a large input chunk.
/// The owner must handle each returned message before reading further events.
pub struct Decoder {
    bytes: Vec<u8>,
    limit: usize,
    poisoned: bool,
}

impl Decoder {
    /// Construct a decoder. A zero bound permits no messages.
    #[must_use]
    pub fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
            poisoned: false,
        }
    }

    /// Supply one byte and return a complete protocol object at its delimiter.
    ///
    /// # Errors
    /// Any framing error is fatal and clears retained payload bytes.
    pub fn push(&mut self, byte: u8) -> Result<Option<Value>, WireError> {
        if self.poisoned {
            return Err(WireError::Poisoned);
        }
        if byte != 0 {
            if self.bytes.len() >= self.limit {
                return self.fail(WireError::TooLarge);
            }
            self.bytes.push(byte);
            return Ok(None);
        }
        if self.bytes.is_empty() {
            return self.fail(WireError::Empty);
        }
        let parsed = serde_json::from_slice::<Value>(&self.bytes);
        self.bytes.fill(0);
        self.bytes.clear();
        match parsed {
            Ok(value) if value.is_object() => Ok(Some(value)),
            _ => self.fail(WireError::InvalidMessage),
        }
    }

    /// Validate an EOF boundary.
    ///
    /// # Errors
    /// Rejects incomplete messages or a poisoned session.
    pub fn finish(&mut self) -> Result<(), WireError> {
        if self.poisoned {
            return Err(WireError::Poisoned);
        }
        if !self.bytes.is_empty() {
            return self.fail(WireError::Truncated);
        }
        Ok(())
    }

    fn fail<T>(&mut self, error: WireError) -> Result<T, WireError> {
        self.bytes.fill(0);
        self.bytes.clear();
        self.poisoned = true;
        Err(error)
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // Best-effort clearing only; this is not a cryptographic zeroization
        // guarantee, and parsed JSON values remain the caller's responsibility.
        self.bytes.fill(0);
    }
}

/// Encode an object with its delimiter, without exposing payload in errors.
///
/// # Errors
/// Rejects non-objects and messages over the configured bound.
pub fn encode(message: &Value, limit: usize) -> Result<Vec<u8>, WireError> {
    if !message.is_object() {
        return Err(WireError::InvalidMessage);
    }
    // Count serialization first. This avoids allocating the entire oversized
    // payload just to reject it. The input Value is owned by the caller.
    let mut counter = BoundedCounter { bytes: 0, limit };
    serde_json::to_writer(&mut counter, message).map_err(|_| WireError::TooLarge)?;
    let mut encoded = Vec::with_capacity(counter.bytes.saturating_add(1));
    serde_json::to_writer(&mut encoded, message).map_err(|_| WireError::InvalidMessage)?;
    encoded.push(0);
    Ok(encoded)
}

struct BoundedCounter {
    bytes: usize,
    limit: usize,
}

impl std::io::Write for BoundedCounter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let next = self
            .bytes
            .checked_add(bytes.len())
            .filter(|n| *n <= self.limit)
            .ok_or_else(|| std::io::Error::other("CDP size limit"))?;
        self.bytes = next;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
