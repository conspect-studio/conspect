//! The crate-local error taxonomy for the control plane.
//!
//! Every fallible control-plane operation surfaces one of these variants; the
//! HTTP boundary maps each to an [RFC 9457](https://www.rfc-editor.org/rfc/rfc9457)
//! `application/problem+json` response via [`crate::problem::Problem`]
//! ([`ControlError::into_problem`]). Domain code never panics — it returns this
//! error and lets the boundary translate it.
use crate::problem::Problem;

/// A control-plane error.
///
/// The variants partition the failure space the HTTP layer must distinguish:
/// missing resources (`404`), optimistic-concurrency conflicts (`412`),
/// authentication/authorization failures (`401`/`403`), validation problems
/// (`400`/`422`), engine-overload back-pressure rejection (`503`), and
/// persistence faults (`500`). Each maps to a stable problem `type` slug.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ControlError {
    /// The addressed resource does not exist.
    #[error("resource {kind} {id:?} not found")]
    NotFound {
        /// The resource collection (e.g. `layout`).
        kind: &'static str,
        /// The requested id.
        id: String,
    },

    /// An `If-Match` precondition did not match the current resource version.
    #[error("version conflict on {kind} {id:?}: expected {expected:?}, found {actual:?}")]
    VersionConflict {
        /// The resource collection.
        kind: &'static str,
        /// The requested id.
        id: String,
        /// The `If-Match` value the client presented (the version it expected).
        expected: String,
        /// The version the resource currently holds.
        actual: String,
    },

    /// A required precondition header (`If-Match`) was absent on a mutating
    /// request that requires it.
    #[error("missing required If-Match precondition on {kind}")]
    PreconditionRequired {
        /// The resource collection.
        kind: &'static str,
    },

    /// The request body or parameters failed validation.
    #[error("validation failed: {0}")]
    Validation(String),

    /// The request conflicts with the resource's current state in a way an
    /// `If-Match` cannot express — e.g. deleting a device while a Source/Output
    /// still references it via `device_ref` (ADR-M009). The detail names the
    /// blocking references so the operator knows what to unbind.
    #[error("conflict: {0}")]
    Conflict(String),

    /// Authentication failed: no credential, or an unrecognized API key.
    #[error("authentication required")]
    Unauthenticated,

    /// The authenticated principal lacks the role/permission for this action,
    /// or is denied access to this specific object (BOLA defense).
    #[error("forbidden: {0}")]
    Forbidden(String),

    /// The engine command bus is saturated; the request is shed rather than
    /// blocking the engine (invariant #10). The client should retry later.
    #[error("engine command bus at capacity; request shed")]
    EngineBusy,

    /// A persistence/repository fault.
    #[error("repository error: {0}")]
    Repository(String),
}

impl ControlError {
    /// Map this error onto its RFC 9457 problem document.
    ///
    /// The mapping is total and stable: the same variant always yields the same
    /// `status` and `type` slug so clients can branch on them.
    #[must_use]
    pub fn into_problem(self) -> Problem {
        match self {
            Self::NotFound { kind, id } => Problem::new(404, "not-found", "Resource not found")
                .with_detail(format!("{kind} {id:?} does not exist")),
            Self::VersionConflict {
                kind,
                id,
                expected,
                actual,
            } => Problem::new(412, "version-conflict", "Precondition failed").with_detail(format!(
                "{kind} {id:?} was modified: expected version {expected}, current is {actual}"
            )),
            Self::PreconditionRequired { kind } => {
                Problem::new(428, "precondition-required", "Precondition required")
                    .with_detail(format!("an If-Match header is required to modify a {kind}"))
            }
            Self::Validation(msg) => {
                Problem::new(422, "validation", "Request validation failed").with_detail(msg)
            }
            Self::Conflict(msg) => {
                Problem::new(409, "conflict", "Conflict with current state").with_detail(msg)
            }
            Self::Unauthenticated => {
                Problem::new(401, "unauthenticated", "Authentication required")
                    .with_detail("a valid Bearer API key is required")
            }
            Self::Forbidden(msg) => {
                Problem::new(403, "forbidden", "Access denied").with_detail(msg)
            }
            Self::EngineBusy => Problem::new(503, "engine-busy", "Engine command bus at capacity")
                .with_detail("the control command queue is full; retry shortly"),
            Self::Repository(msg) => {
                Problem::new(500, "repository", "Internal repository error").with_detail(msg)
            }
        }
    }
}

impl axum::response::IntoResponse for ControlError {
    fn into_response(self) -> axum::response::Response {
        self.into_problem().into_response()
    }
}

/// A control-plane result.
pub type ControlResult<T> = Result<T, ControlError>;
