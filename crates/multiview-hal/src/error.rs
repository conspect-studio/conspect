//! Error taxonomy for the hardware-abstraction layer.
//!
//! Every fallible operation in `multiview-hal` returns [`Result`], whose error arm
//! is the crate-local [`enum@Error`] enum. At the crate boundary these convert into
//! the workspace-wide [`multiview_core::Error`] taxonomy (the dedicated
//! [`multiview_core::Error::Backend`] arm carries the message), so downstream
//! crates see one uniform error surface.
use crate::capability::Stage;
use multiview_core::traits::BackendKind;
use thiserror::Error;

/// Convenient result alias used throughout the crate.
pub type Result<T> = core::result::Result<T, Error>;

/// Errors raised by capability detection, the registry, and the planner.
///
/// Marked `#[non_exhaustive]`: downstream `match` statements must include a
/// wildcard arm so new variants can be added without a breaking change.
#[derive(Debug, Error, Clone, PartialEq)]
#[non_exhaustive]
pub enum Error {
    /// A backend was requested for a `(stage, kind)` pair that is not
    /// registered.
    #[error("no backend registered for stage {stage:?} kind {kind:?}")]
    BackendNotFound {
        /// The pipeline stage that was queried.
        stage: Stage,
        /// The backend kind that was queried.
        kind: BackendKind,
    },

    /// A backend with this `(stage, kind)` pair is already registered and
    /// would be overwritten.
    #[error("backend already registered for stage {stage:?} kind {kind:?}")]
    DuplicateBackend {
        /// The pipeline stage of the conflicting registration.
        stage: Stage,
        /// The backend kind of the conflicting registration.
        kind: BackendKind,
    },

    /// A hardware backend was requested but is unavailable on this host
    /// (feature not compiled in, or the probe found no usable device).
    #[error("backend {kind:?} is unavailable: {reason}")]
    BackendUnavailable {
        /// The backend kind that could not be provided.
        kind: BackendKind,
        /// Human-readable reason (e.g. `"cuda feature not enabled"`).
        reason: &'static str,
    },

    /// A proposed plan cannot be admitted because it exceeds an engine budget.
    #[error(
        "admission denied: {stage:?} load {requested_mpps:.3} Mpix/s exceeds \
         budget {budget_mpps:.3} Mpix/s"
    )]
    BudgetExceeded {
        /// The stage whose budget was exceeded.
        stage: Stage,
        /// The requested load, in megapixels per second.
        requested_mpps: f64,
        /// The available budget, in megapixels per second.
        budget_mpps: f64,
    },

    /// A capability descriptor or cost figure was structurally invalid.
    #[error("invalid capability/cost: {0}")]
    InvalidCapability(&'static str),
}

impl From<Error> for multiview_core::Error {
    /// Fold a HAL fault into the workspace taxonomy. Capability detection,
    /// registry negotiation, and admission against an engine budget are
    /// host-capability failures (the operator's request was structurally valid;
    /// the host simply cannot satisfy it as asked), so they route to the
    /// dedicated [`multiview_core::Error::Backend`] arm rather than
    /// [`multiview_core::Error::Config`]; the detail is preserved.
    fn from(value: Error) -> Self {
        multiview_core::Error::Backend(value.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::Error;
    use crate::capability::Stage;
    use multiview_core::traits::BackendKind;

    /// The crate boundary folds every HAL fault — capability detection, registry
    /// negotiation, and admission against an engine budget — into the
    /// workspace-wide [`multiview_core::Error::Backend`] arm, *not*
    /// [`multiview_core::Error::Config`]: the operator's request was structurally
    /// valid; the host simply cannot satisfy it. Guards the routing and proves
    /// the detail survives.
    #[test]
    fn backend_unavailable_routes_to_core_backend_arm() {
        let err: multiview_core::Error = Error::BackendUnavailable {
            kind: BackendKind::Cuda,
            reason: "cuda feature not enabled",
        }
        .into();
        match err {
            multiview_core::Error::Backend(msg) => {
                assert!(
                    msg.contains("cuda feature not enabled"),
                    "detail must survive the conversion, got: {msg}"
                );
            }
            other => panic!("expected Error::Backend, got {other:?}"),
        }
    }

    /// An admission/budget denial also lands on the `Backend` arm (the planner's
    /// "host cannot satisfy this as asked" failure), confirming the whole `From`
    /// impl is rerouted rather than a single variant.
    #[test]
    fn budget_exceeded_also_routes_to_backend_arm() {
        let err: multiview_core::Error = Error::BudgetExceeded {
            stage: Stage::Decode,
            requested_mpps: 500.0,
            budget_mpps: 250.0,
        }
        .into();
        assert!(
            matches!(err, multiview_core::Error::Backend(_)),
            "expected Error::Backend, got {err:?}"
        );
    }
}
