//! Build-capability gating for `[timing].ptp_phc` (DEV-C1 / ADR-M010).
//!
//! Sampling a PTP Hardware Clock requires the `ptp` build of the `multiview`
//! binary (the engine's `rustix` PHC reader). A build **without** that
//! feature must FAIL a run whose config names a PHC device — clearly, at
//! startup — never silently downgrade the epoch to the system clock (the
//! DEV-B1 display-output precedent in [`crate::outputs`]: a configured
//! capability the binary cannot provide is an error, not a warning). This
//! module is always compiled, so the default CI build exercises the rejection
//! path and a `ptp` build exercises the acceptance path.

use multiview_config::TimingConfig;

/// Ensure a configured `[timing].ptp_phc` is runnable in this build.
///
/// No `[timing]` block, or one without `ptp_phc`, always passes. In a `ptp`
/// build a configured PHC passes (it is opened at run time, with its own
/// honest degrade-on-open-failure path). In any other build it returns a
/// clear, actionable error naming the configured device and the required
/// feature.
///
/// # Errors
///
/// A human-readable message naming the configured PHC device and the `ptp`
/// build requirement, when this binary was built without the feature.
pub fn ensure_ptp_phc_supported(timing: Option<&TimingConfig>) -> Result<(), String> {
    let Some(device) = timing.and_then(|t| t.ptp_phc.as_deref()) else {
        return Ok(());
    };
    if cfg!(feature = "ptp") {
        return Ok(());
    }
    Err(format!(
        "[timing].ptp_phc requires a ptp build: this binary was built without the \
         `ptp` feature, so the configured PTP hardware clock `{device}` cannot be \
         sampled (rebuild with `--features ptp`, or remove `ptp_phc` to ride the \
         chrony/NTP-disciplined system clock)"
    ))
}
