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

    /// An earlier capture admission survived a crash without a proven receipt.
    #[error("capture outcome is uncertain")]
    CaptureOutcomeUncertain,

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

    /// The bounded idempotency reservation store is full. Existing keys still
    /// replay, but a new key cannot be accepted without risking duplicate work.
    #[error("idempotency reservation store at capacity; new key refused")]
    IdempotencyCapacity,

    /// The atomic rename became visible, but directory durability could not be
    /// confirmed. The caller must read the authoritative record before retrying.
    #[error("desired-state commit status is indeterminate: {0}")]
    CommitIndeterminate(String),

    /// A bounded control-plane queue or per-principal rate is exhausted (the deep
    /// probe's queue/rate limit); the request is shed with `429`, never blocked.
    #[error("too many requests: {0}")]
    TooManyRequests(String),

    /// A persistence/repository fault.
    #[error("repository error: {0}")]
    Repository(String),

    /// The prospective accepted state is one the engine would refuse to start
    /// from (issue #317): the declared switcher M/E cannot carry it (today:
    /// overlays on the Program canvas). Nothing was applied or persisted.
    #[error("{0}")]
    NotAdmissibleWithSwitcher(multiview_config::SwitcherCanvasRefusal),

    /// `apply-layout` without a `target` while a switcher M/E is declared
    /// (#307): the M/E owns the Program canvas. Nothing was reserved,
    /// persisted or sent.
    #[error("the switcher M/E owns the Program canvas; layout {0:?} was not applied")]
    SwitcherOwnsProgram(String),

    /// A cell edit — `SwapSource`, a video→cell route take, or a salvo that
    /// recalls a layout or cell sources — while a switcher M/E is declared
    /// (#307 review M1): it would change neither Program (the M/E bus) nor the
    /// stored layout outputs show, so it is refused rather than accepted as a
    /// silent no-op. The payload names the refused operation. Nothing was
    /// reserved or sent.
    #[error("the switcher M/E owns the Program canvas; {0} was not applied")]
    SwitcherOwnsCells(&'static str),
}

/// The stable problem `type` slug for [`ControlError::NotAdmissibleWithSwitcher`].
pub const LAYOUT_NOT_ADMISSIBLE_WITH_SWITCHER: &str = "layout-not-admissible-with-switcher";

/// The stable problem `type` slug for [`ControlError::SwitcherOwnsProgram`].
pub const SWITCHER_OWNS_PROGRAM: &str = "switcher-owns-program";

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
            Self::CaptureOutcomeUncertain => Problem::new(409, "capture-outcome-uncertain", "Capture outcome is uncertain")
                .with_detail("This request will not run again with the same Idempotency-Key. Do not bypass it with a new key. An administrator must verify the producer generation and artifacts before an audited recovery decision."),
            Self::Unauthenticated => {
                Problem::new(401, "unauthenticated", "Authentication required")
                    .with_detail("a valid Bearer API key is required")
            }
            Self::Forbidden(msg) => {
                Problem::new(403, "forbidden", "Access denied").with_detail(msg)
            }
            Self::EngineBusy => Problem::new(503, "engine-busy", "Engine command bus at capacity")
                .with_detail("the control command queue is full; retry shortly"),
            Self::IdempotencyCapacity => Problem::new(
                503,
                "idempotency-capacity",
                "Idempotency reservation capacity exhausted",
            )
            .with_detail("the process cannot safely reserve another distinct idempotency key; existing keys still replay"),
            Self::CommitIndeterminate(msg) => Problem::new(
                503,
                "commit-indeterminate",
                "Desired-state commit status is indeterminate",
            )
            .with_detail(format!(
                "{msg}; read the current resource and its ETag before a retry"
            )),
            Self::TooManyRequests(msg) => {
                Problem::new(429, "too-many-requests", "Too many requests").with_detail(msg)
            }
            Self::Repository(msg) => {
                Problem::new(500, "repository", "Internal repository error").with_detail(msg)
            }
            // 422, like the switcher routes' refusal of an unsupported take and
            // apply-layout's existing Class-2 refusal: the request is well-formed
            // but not executable in the current configuration.
            Self::NotAdmissibleWithSwitcher(refusal) => Problem::new(
                422,
                LAYOUT_NOT_ADMISSIBLE_WITH_SWITCHER,
                "Not admissible while the switcher is on",
            )
            .with_detail(refusal.operator_message()),
            Self::SwitcherOwnsProgram(layout) => Problem::new(
                422,
                SWITCHER_OWNS_PROGRAM,
                "The switcher owns the Program canvas",
            )
            .with_detail(format!(
                "The switcher is on: the Program canvas is the M/E bus, so layout {layout:?} \
                 cannot replace it. To show the layout on an output, take it there with \
                 POST /api/v1/routing/output/take (feed {{\"layout\":\"{layout}\"}}) or send \
                 apply-layout with target {{\"output\":\"<id>\"}}; to cut it to Program, \
                 select it as a switcher source with \
                 POST /api/v1/switcher/commands/select_preview (layout:{layout}). \
                 Nothing was changed."
            )),
            Self::SwitcherOwnsCells(operation) => Problem::new(
                422,
                SWITCHER_OWNS_PROGRAM,
                "The switcher owns the Program canvas",
            )
            .with_detail(format!(
                "The switcher is on: Program is the M/E bus, so {operation} would change \
                 neither Program nor the stored layout outputs show. To change what a \
                 layout's cells show, edit the stored layout with PUT /api/v1/layouts/{{id}} \
                 (outputs showing it update live); to change Program, use a switcher select \
                 (POST /api/v1/switcher/commands/select_preview or select_program). \
                 Nothing was changed."
            )),
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
