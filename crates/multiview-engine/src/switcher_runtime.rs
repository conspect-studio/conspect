//! First-slice dual-bus tick facade. This composes only after the caller's
//! admission check has budgeted TWO full composites at the configured cadence;
//! a monitor-only deployment should instead use a best-effort off-clock tap.

use crate::clock::Tick;
use crate::error::{Error, Result};
use crate::switcher_bus::{BusFrames, DualBusDrive};
use crate::switcher_control::{ProviderStatus, SwitcherTickDriver};
use std::sync::Arc;

/// Owns the M/E state and two independently pooled scene drives. It is never
/// shared with a REST task; a single clock invokes `tick` exactly once per tick.
pub struct SwitcherRuntime {
    driver: SwitcherTickDriver,
    buses: DualBusDrive,
    scene_version: u64,
}

impl SwitcherRuntime {
    /// Assemble a run only after checking full-rate dual-bus composite capacity.
    #[must_use]
    pub fn new(driver: SwitcherTickDriver, buses: DualBusDrive, scene_version: u64) -> Self {
        Self {
            driver,
            buses,
            scene_version,
        }
    }

    /// Apply at most sixteen queued commands at the *same exact tick* used for
    /// both canvases, then return immutable bus frames for output routing.
    /// Failed composition revokes provider readiness; never substitute PGM for
    /// PVW. A cold preview shows its honest slate while a Cut is held on prime.
    ///
    /// # Errors
    /// Propagates the compositor error; caller emits typed output slate at the
    /// same cadence and must not silently switch to a different bus.
    pub fn tick(&mut self, tick: Tick) -> Result<BusFrames> {
        let projection = self.driver.drain_at_tick(tick);
        let (program, preview) = self.driver.selections();
        let program = program.clone();
        let preview = preview.clone();
        let pair = self
            .driver
            .scene_pair()
            .ok_or_else(|| Error::InvalidLayout("no M/E scene provider".into()))?;
        if pair.version != self.scene_version {
            self.driver.revoke_provider_status();
            return Err(Error::InvalidLayout(
                "switcher scene version changed before output admission".into(),
            ));
        }
        let rendered = self
            .buses
            .compose_selected(tick, &program, &preview, projection);
        match rendered {
            Ok(frames) => {
                let canvas = &pair.program.canvas;
                self.driver.publish_provider_status(ProviderStatus {
                    scene_version: pair.version,
                    last_tick: tick.index,
                    width: canvas.width,
                    height: canvas.height,
                    fps_num: canvas.fps_num,
                    fps_den: canvas.fps_den,
                });
                Ok(frames)
            }
            Err(error) => {
                self.driver.revoke_provider_status();
                Err(error)
            }
        }
    }

    /// Borrow the current bus labels after a real frame was produced.
    /// `Arc` references are immutable; a consumer may hold one without retaining
    /// an input store or changing the M/E machine.
    #[must_use]
    pub fn source_snapshot(&self) -> Option<Arc<crate::switcher_control::ScenePair>> {
        self.driver.scene_pair()
    }
}
