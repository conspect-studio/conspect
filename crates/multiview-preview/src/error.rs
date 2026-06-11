//! Error taxonomy for `multiview-preview`.
//!
//! Preview lives in the best-effort control-plane tier (invariant #10): its
//! errors describe configuration / token / framing failures and are surfaced to
//! the operator, but they can never affect the protected output path. Each
//! subsystem has its own focused error type; [`enum@Error`] is the crate-level
//! union the public surface returns.
use thiserror::Error;

pub use crate::framing::JpegError;
pub use crate::token::TokenError;

/// Convenient result alias for the crate's public surface.
pub type Result<T> = core::result::Result<T, Error>;

/// The crate-level error union.
///
/// `#[non_exhaustive]` so new variants can be added without a breaking change;
/// downstream `match` statements must carry a wildcard arm.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// A signed-access-token operation failed (see [`TokenError`]).
    #[error(transparent)]
    Token(#[from] TokenError),

    /// A JPEG/framing operation failed (see [`JpegError`]).
    #[error(transparent)]
    Jpeg(#[from] JpegError),
}
