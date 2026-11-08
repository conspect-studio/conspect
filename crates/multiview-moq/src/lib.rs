//! Encoded-video adapter seam. Call only from an off-clock bounded consumer task.
//! Admission, global retained-allocation accounting and authorization are external.
pub mod av;
pub mod provider;
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
    generation: u64,
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
            generation,
            revoked: false,
            needs_rap: true,
            last_pts: None,
        }
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
        for packet in &window.packets {
            if let Err(error) = self.publish_packet(packet) {
                self.discontinuity();
                return Err(error);
            }
            self.last_pts = Some(packet.pts_us);
        }
        Ok(())
    }

    fn publish_packet(&mut self, packet: &Packet) -> Result<(), Reject> {
        if packet.verified_rap {
            if let Some(previous) = self.group.take() {
                previous.finish().map_err(|_| Reject::Transport)?;
            }
            self.group = Some(self.track.append_group().map_err(|_| Reject::Transport)?);
            self.needs_rap = false;
        }
        let timestamp = moq_net::Timestamp::new(packet.pts_us, moq_net::Timescale::MICRO)
            .map_err(|_| Reject::Timestamp)?;
        let serialized = provider::serialized_frame(packet)?;
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
