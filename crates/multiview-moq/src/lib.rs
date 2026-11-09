//! Encoded-video adapter seam. Call only from an off-clock bounded consumer task.
//! Admission, global retained-allocation accounting and authorization are external.
/// Family-scoped retained allocation lifetime accounting.
pub mod allocation;
pub mod av;
/// Experimental genuine-delivery controller (not production wired).
pub mod delivery;
/// Shared multi-rendition ladder: exact admitted specs, one catalog, one clock.
pub mod ladder;
pub mod provider;
/// Real multi-track publication of one admitted shared ladder.
pub mod publication;
/// Opt-in outgoing publisher sharing one canonical admitted family.
pub mod relay;
/// Publisher-side uplink budgeting and bounded peer subscription tracking.
pub mod uplink;
use bytes::Bytes;
use std::time::Duration;

/// Immutable shared encoded packet; `verified_rap` must come from the codec parser.
#[derive(Clone, Debug)]
pub struct Packet {
    pub payload: Bytes,
    pub pts_us: u64,
    pub verified_rap: bool,
}

/// One complete, already admitted encoded-time video window.
#[derive(Clone, Debug)]
pub struct Window {
    pub generation: u64,
    pub packets: Vec<Packet>,
}

/// A bounded, already admitted window shape that a subscription fence may carry.
///
/// The fence charges and wraps exactly the packets this visits, in a bounded deterministic
/// order, so one fence — one queue bound, one charge account, one revocation identity —
/// serves both the single-rendition and the shared-ladder shapes. There is no second fence.
pub trait RetainedWindow: Send + 'static {
    /// The configuration generation this window was admitted under.
    fn generation(&self) -> u64;
    /// Visit every retained packet mutably. Bounded by construction, never parallel.
    fn for_each_packet(&mut self, visit: &mut dyn FnMut(&mut Packet));
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reject {
    Revoked,
    Generation,
    Empty,
    Budget,
    Timestamp,
    NeedsRap,
    Transport,
}

/// Video-only hang mapping. It never encodes and never awaits network I/O.
/// A failure poisons the dependency chain until a verified RAP arrives.
/// This is not a production admission queue or an audio/video atomic publisher.
pub struct VideoAdapter {
    track: moq_net::track::Producer,
    group: Option<moq_net::group::Producer>,
    cancel_groups: Option<std::collections::VecDeque<moq_net::group::Producer>>,
    generation: u64,
    allocation: Option<allocation::AllocationBudget>,
    revoked: bool,
    needs_rap: bool,
    last_pts: Option<u64>,
}

impl VideoAdapter {
    pub const MAX_PACKETS: usize = 60;
    pub const MAX_BYTES: usize = 4 * 1024 * 1024;
    pub const MAX_SPAN_US: u64 = 2_000_000;

    pub fn new(track: moq_net::track::Producer, generation: u64) -> Self {
        Self {
            track,
            group: None,
            cancel_groups: None,
            generation,
            allocation: None,
            revoked: false,
            needs_rap: true,
            last_pts: None,
        }
    }

    pub fn with_allocation_budget(mut self, budget: allocation::AllocationBudget) -> Self {
        self.allocation = Some(budget);
        self
    }

    pub fn track_info() -> moq_net::track::Info {
        hang::container::track_info(hang::catalog::PRIORITY.video)
            .with_max_age(Duration::from_secs(2))
    }

    /// Stop publication immediately. The owner must separately close QUIC and
    /// acknowledge its subscription fence before releasing source demand.
    pub fn revoke(&mut self) {
        self.revoked = true;
        self.group.take();
    }

    /// Opt into bounded cancellation ownership for experimental publisher trials.
    pub fn enable_trial_cancellation(&mut self) {
        if self.cancel_groups.is_none() {
            self.cancel_groups = Some(Default::default());
        }
    }

    /// Abort the current incomplete group when a publisher-prefix trial is canceled.
    /// Completed older group streams still follow their transport completion/expiry.
    pub fn abort_group(&mut self) {
        if let Some(groups) = &mut self.cancel_groups {
            for group in groups.drain(..) {
                let _ = group.abort(moq_net::Error::Cancel);
            }
        }
        if let Some(group) = self.group.take() {
            let _ = group.abort(moq_net::Error::Cancel);
        }
        self.needs_rap = true;
    }

    /// Signal loss before accepting the next complete window.
    pub fn discontinuity(&mut self) {
        self.group.take();
        self.needs_rap = true;
    }

    fn validate(&self, window: &Window) -> Result<(), Reject> {
        if self.revoked {
            return Err(Reject::Revoked);
        }
        if window.generation != self.generation {
            return Err(Reject::Generation);
        }
        let first = window.packets.first().ok_or(Reject::Empty)?;
        if window.packets.len() > Self::MAX_PACKETS {
            return Err(Reject::Budget);
        }
        let mut total = 0usize;
        let mut previous = self.last_pts;
        for packet in &window.packets {
            total = total
                .checked_add(packet.payload.len())
                .ok_or(Reject::Budget)?;
            if packet.payload.is_empty() || total > Self::MAX_BYTES {
                return Err(Reject::Budget);
            }
            if previous.is_some_and(|pts| packet.pts_us <= pts) {
                return Err(Reject::Timestamp);
            }
            if packet.pts_us.saturating_sub(first.pts_us) > Self::MAX_SPAN_US {
                return Err(Reject::Budget);
            }
            if moq_net::Timestamp::new(packet.pts_us, moq_net::Timescale::MICRO).is_err() {
                return Err(Reject::Timestamp);
            }
            previous = Some(packet.pts_us);
        }
        if self.needs_rap && !first.verified_rap {
            return Err(Reject::NeedsRap);
        }
        Ok(())
    }

    /// Validate the entire window before publishing any of it. A downstream
    /// write error may publish a prefix, so it always forces discontinuity.
    /// Payload clones retain shared immutable allocations, not encoder copies.
    pub fn publish(&mut self, window: &Window) -> Result<(), Reject> {
        if let Err(error) = self.validate(window) {
            self.discontinuity();
            return Err(error);
        }
        // Reserve/serialize the ENTIRE window before mutating any group or PTS.
        let prepared = window
            .packets
            .iter()
            .map(|packet| provider::serialized_frame_in(packet, self.allocation.as_ref()))
            .collect::<Result<Vec<_>, _>>()?;
        for (packet, serialized) in window.packets.iter().zip(prepared) {
            if let Err(error) = self.publish_packet(packet, serialized) {
                // Transport mutation cannot be rolled back; make this adapter terminal.
                self.abort_group();
                self.revoked = true;
                return Err(error);
            }
            self.last_pts = Some(packet.pts_us);
        }
        Ok(())
    }

    fn publish_packet(&mut self, packet: &Packet, serialized: bytes::Bytes) -> Result<(), Reject> {
        let timestamp = moq_net::Timestamp::new(packet.pts_us, moq_net::Timescale::MICRO)
            .map_err(|_| Reject::Timestamp)?;
        if packet.verified_rap {
            if let Some(previous) = self.group.take() {
                previous.finish().map_err(|_| Reject::Transport)?;
                if let Some(groups) = &mut self.cancel_groups {
                    groups.push_back(previous);
                    if groups.len() > 4 {
                        if let Some(old) = groups.pop_front() {
                            let _ = old.abort(moq_net::Error::Old);
                        }
                    }
                }
            }
            self.group = Some(self.track.append_group().map_err(|_| Reject::Transport)?);
            self.needs_rap = false;
        }
        self.group
            .as_mut()
            .ok_or(Reject::NeedsRap)?
            .write_frame(timestamp, serialized)
            .map_err(|_| Reject::Transport)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn adapter() -> VideoAdapter {
        let broadcast = moq_net::broadcast::Producer::new(Default::default());
        VideoAdapter::new(
            broadcast
                .create_track("video", VideoAdapter::track_info())
                .unwrap(),
            7,
        )
    }
    fn window(pts: u64, rap: bool) -> Window {
        Window {
            generation: 7,
            packets: vec![Packet {
                payload: Bytes::from_static(&[0, 1]),
                pts_us: pts,
                verified_rap: rap,
            }],
        }
    }
    #[test]
    fn second_packet_budget_failure_preserves_video_group_and_pts() {
        let budget = allocation::AllocationBudget::new(12).unwrap();
        let mut adapter = adapter().with_allocation_budget(budget.clone());
        let mut window = window(0, true);
        window.packets.push(Packet {
            payload: Bytes::from_static(&[0, 1]),
            pts_us: 1,
            verified_rap: false,
        });
        assert_eq!(adapter.publish(&window), Err(Reject::Budget));
        assert!(adapter.group.is_none());
        assert_eq!(adapter.last_pts, None);
        assert_eq!(budget.retained_bytes(), 0);
        assert!(!adapter.revoked);
    }
    #[test]
    fn trial_retention_is_bounded_and_opt_in_is_idempotent() {
        let mut adapter = adapter();
        assert!(adapter.cancel_groups.is_none());
        adapter.enable_trial_cancellation();
        for tick in 0..8 {
            adapter.publish(&window(tick * 33_333, true)).unwrap();
        }
        assert_eq!(adapter.cancel_groups.as_ref().unwrap().len(), 4);
        adapter.enable_trial_cancellation();
        assert_eq!(adapter.cancel_groups.as_ref().unwrap().len(), 4);
        adapter.abort_group();
        assert!(adapter.group.is_none());
        assert!(adapter.cancel_groups.as_ref().unwrap().is_empty());
        assert!(adapter.needs_rap);
    }
    #[test]
    fn finished_groups_remain_abortable_for_trial_cancellation() {
        let broadcast = moq_net::broadcast::Producer::new(Default::default());
        let track = broadcast
            .create_track("trial", VideoAdapter::track_info())
            .unwrap();
        let mut group = track.append_group().unwrap();
        group
            .write_frame(
                moq_net::Timestamp::from_millis(0).unwrap(),
                Bytes::from_static(b"test"),
            )
            .unwrap();
        group.finish().unwrap();
        assert!(group.abort(moq_net::Error::Cancel).is_ok());
    }
    #[test]
    fn revocation_and_generation_never_publish() {
        let mut adapter = adapter();
        let mut wrong = window(0, true);
        wrong.generation = 8;
        assert_eq!(adapter.publish(&wrong), Err(Reject::Generation));
        adapter.revoke();
        assert_eq!(adapter.publish(&window(1, true)), Err(Reject::Revoked));
    }
    #[test]
    fn rejection_requires_rap_and_validates_whole_window() {
        let mut adapter = adapter();
        assert_eq!(adapter.validate(&window(0, false)), Err(Reject::NeedsRap));
        let mut invalid = window(0, true);
        invalid.packets.push(Packet {
            payload: Bytes::new(),
            pts_us: 1,
            verified_rap: false,
        });
        assert_eq!(adapter.publish(&invalid), Err(Reject::Budget));
        assert!(adapter.last_pts.is_none());
        assert!(adapter.group.is_none());
    }
    #[test]
    fn packet_and_span_limits_apply_before_writes() {
        let adapter = adapter();
        let mut oversized = window(0, true);
        oversized.packets.resize(61, oversized.packets[0].clone());
        assert_eq!(adapter.validate(&oversized), Err(Reject::Budget));
        let mut span = window(0, true);
        span.packets.push(Packet {
            payload: Bytes::from_static(&[0]),
            pts_us: 2_000_001,
            verified_rap: false,
        });
        assert_eq!(adapter.validate(&span), Err(Reject::Budget));
    }
}
