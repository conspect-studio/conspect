//! Operational errors from the safe NDI handles (`send`/`recv`/`find`).
//!
//! These are *reported* outcomes — a refused create, a malformed argument, a
//! short host buffer — never panics or blocks (safety rule #3 / inv #1). The
//! handle constructors and methods return [`NdiError`] so the consuming
//! `forbid(unsafe_code)` crates (`multiview-output` / `-input`) can translate it
//! into their own typed sink/source status without ever touching FFI.

use crate::table::TableError;

/// A failure from constructing or driving a safe NDI handle.
#[derive(Debug)]
#[non_exhaustive]
pub enum NdiError {
    /// The loaded runtime table was missing a required function pointer
    /// (resolved once via [`crate::table`]).
    Table(TableError),
    /// A string argument (e.g. the sender/source name) contained an interior NUL
    /// byte and could not be passed to the C API as a NUL-terminated string.
    InvalidCString {
        /// Which argument was malformed (e.g. `"sender name"`).
        field: &'static str,
    },
    /// An SDK create call returned a null instance handle (the runtime refused).
    NullInstance {
        /// Which handle could not be created (e.g. `"NDI sender"`).
        what: &'static str,
    },
    /// The host pixel buffer is shorter than the frame geometry requires; sending
    /// it would be an out-of-bounds read in the SDK, so it is refused.
    ShortBuffer {
        /// Bytes the caller supplied.
        have: usize,
        /// Bytes the declared geometry (`stride * height`) needs.
        need: usize,
    },
    /// A geometry/rate field does not fit the C `int` the SDK ABI uses (e.g. a
    /// width/stride above `i32::MAX`). Refused rather than truncated.
    FieldOutOfRange {
        /// Which field was out of range (e.g. `"width"`).
        field: &'static str,
    },
}

impl std::fmt::Display for NdiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Table(err) => write!(f, "{err}"),
            Self::InvalidCString { field } => {
                write!(f, "the {field} contains an interior NUL byte")
            }
            Self::NullInstance { what } => {
                write!(f, "the NDI runtime returned a null {what} handle")
            }
            Self::ShortBuffer { have, need } => {
                write!(f, "host buffer is {have} bytes, need {need} for the frame")
            }
            Self::FieldOutOfRange { field } => {
                write!(f, "the {field} field does not fit the SDK's C int range")
            }
        }
    }
}

impl std::error::Error for NdiError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Table(err) => Some(err),
            _ => None,
        }
    }
}

impl From<TableError> for NdiError {
    fn from(err: TableError) -> Self {
        Self::Table(err)
    }
}
