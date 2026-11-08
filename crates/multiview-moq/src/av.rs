//! Coordinated video/audio publication for an already admitted immutable window.
use crate::{Packet, Reject, VideoAdapter, Window};
use std::collections::BTreeMap;

/// Every configured audio track must appear, even when its window has no packets.
/// All tracks share the video binding generation and timestamp epoch.
pub struct AvWindow {
    pub video: Window,
    pub audio: BTreeMap<String, Vec<Packet>>,
}

struct AudioTrack {
    producer: moq_net::track::Producer,
    last_pts: Option<u64>,
}

/// Off-clock mapping only. Admission retains a whole A/V window upstream.
/// Downstream transport errors can write a prefix and force a common gap epoch.
pub struct AvAdapter {
    video: VideoAdapter,
    audio: BTreeMap<String, AudioTrack>,
}

impl AvAdapter {
    pub fn new(video: VideoAdapter, audio: BTreeMap<String, moq_net::track::Producer>) -> Self {
        Self {
            video,
            audio: audio
                .into_iter()
                .map(|(name, producer)| {
                    (
                        name,
                        AudioTrack {
                            producer,
                            last_pts: None,
                        },
                    )
                })
                .collect(),
        }
    }

    pub fn revoke(&mut self) {
        self.video.revoke();
    }
    pub fn discontinuity(&mut self) {
        self.video.discontinuity();
    }

    fn validate(&self, window: &AvWindow) -> Result<(), Reject> {
        self.video.validate(&window.video)?;
        if !self.audio.keys().eq(window.audio.keys()) {
            return Err(Reject::Generation);
        }
        let start = window.video.packets.first().ok_or(Reject::Empty)?.pts_us;
        let mut bytes: usize = window.video.packets.iter().map(|p| p.payload.len()).sum();
        let mut count = window.video.packets.len();
        for (name, packets) in &window.audio {
            let mut previous = self.audio[name].last_pts;
            for packet in packets {
                count += 1;
                bytes = bytes
                    .checked_add(packet.payload.len())
                    .ok_or(Reject::Budget)?;
                if bytes > VideoAdapter::MAX_BYTES || count > 512 || packet.payload.is_empty() {
                    return Err(Reject::Budget);
                }
                if previous.is_some_and(|pts| packet.pts_us <= pts)
                    || packet.pts_us < start
                    || packet.pts_us - start > VideoAdapter::MAX_SPAN_US
                {
                    return Err(Reject::Timestamp);
                }
                moq_net::Timestamp::new(packet.pts_us, moq_net::Timescale::MICRO)
                    .map_err(|_| Reject::Timestamp)?;
                previous = Some(packet.pts_us);
            }
        }
        Ok(())
    }

    pub fn publish(&mut self, window: &AvWindow) -> Result<(), Reject> {
        if let Err(error) = self.validate(window) {
            self.discontinuity();
            return Err(error);
        }
        self.video.publish(&window.video)?;
        for (name, packets) in &window.audio {
            let track = self.audio.get_mut(name).ok_or(Reject::Generation)?;
            for packet in packets {
                let result = (|| {
                    let mut group = track
                        .producer
                        .append_group()
                        .map_err(|_| Reject::Transport)?;
                    let timestamp =
                        moq_net::Timestamp::new(packet.pts_us, moq_net::Timescale::MICRO)
                            .map_err(|_| Reject::Timestamp)?;
                    let serialized = crate::provider::serialized_frame(packet)?;
                    group
                        .write_frame(timestamp, serialized)
                        .map_err(|_| Reject::Transport)?;
                    group.finish().map_err(|_| Reject::Transport)
                })();
                if let Err(error) = result {
                    self.video.discontinuity();
                    return Err(error);
                }
                track.last_pts = Some(packet.pts_us);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    #[test]
    fn missing_or_invalid_audio_prevents_any_video_publication() {
        let broadcast = moq_net::broadcast::Producer::new(Default::default());
        let video = VideoAdapter::new(
            broadcast
                .create_track("video", VideoAdapter::track_info())
                .unwrap(),
            1,
        );
        let audio = broadcast.create_track("audio", None).unwrap();
        let mut adapter = AvAdapter::new(video, BTreeMap::from([("audio".into(), audio)]));
        let mut window = AvWindow {
            video: Window {
                generation: 1,
                packets: vec![Packet {
                    payload: Bytes::from_static(&[0]),
                    pts_us: 10,
                    verified_rap: true,
                }],
            },
            audio: BTreeMap::new(),
        };
        assert_eq!(adapter.publish(&window), Err(Reject::Generation));
        window.audio.insert(
            "audio".into(),
            vec![Packet {
                payload: Bytes::new(),
                pts_us: 10,
                verified_rap: false,
            }],
        );
        assert_eq!(adapter.publish(&window), Err(Reject::Budget));
        assert!(adapter.video.last_pts.is_none());
        adapter.revoke();
        assert_eq!(adapter.publish(&window), Err(Reject::Revoked));
    }
}
