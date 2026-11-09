//! Opt-in outgoing relay publisher for ONE canonical already-admitted family.
//! No track creation, encoders, viewer feedback or resource provisioning lives here.
//! Owner must keep a single instance per canonical family and feed only its admitted
//! prefix to its existing SharedPublication. Connect/observe run off the output clock.
use crate::{
    delivery::{Admission, Controller},
    ladder::SharedLadder,
    publication::SharedPublication,
};
use bytes::Bytes;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayError {
    InvalidBudget,
    Connect,
    WrongFamily,
    Publication,
    UnsupportedFeedback,
}
/// A relay handle never accepts snapshots from callers/viewers: only its own native
/// outgoing session's group counters are sampled. Drop releases the connection owner.
pub struct RelayPublisher {
    connection: moq_tokio::Connection,
    counters: moq_net::delivery::Counters,
    controller: Controller,
    identity: Bytes,
    applied: usize,
}
impl RelayPublisher {
    /// Build the client here so its per-connection send-buffer and stream bounds cannot
    /// be silently omitted by a caller. `send_window` is the transport's unacknowledged
    /// buffer bound, NOT a live allocator counter; callers still budget encoded cache
    /// and shared production separately. No auto reconnect or alternate viewer session.
    pub async fn connect(
        mut config: moq_tokio::connect::Config,
        endpoint: url::Url,
        origin: moq_net::origin::Consumer,
        ladder: &SharedLadder,
        network_overhead_bps: u64,
        send_window_bytes: u64,
    ) -> Result<Self, RelayError> {
        if send_window_bytes == 0 || send_window_bytes > 16 * 1024 * 1024 {
            return Err(RelayError::InvalidBudget);
        }
        let controller = Controller::new(
            &ladder.level_costs_bps(),
            ladder.reserved_audio_bps(),
            network_overhead_bps,
        )
        .ok_or(RelayError::InvalidBudget)?;
        config.once = Some(true);
        let mut quic = config.quic.take().unwrap_or_default();
        quic.send_window = Some(send_window_bytes);
        quic.max_streams = Some(64);
        config.quic = Some(quic);
        let counters = moq_net::delivery::Counters::default();
        let client = config
            .init(Default::default())
            .map_err(|_| RelayError::Connect)?
            .with_publisher(origin)
            .with_stats(moq_net::stats::Session::default().with_delivery(counters.clone()));
        let connection = tokio::time::timeout(
            Duration::from_secs(20),
            client.connect(endpoint).established(),
        )
        .await
        .map_err(|_| RelayError::Connect)?
        .map_err(|_| RelayError::Connect)?;
        Ok(Self {
            connection,
            counters,
            controller,
            identity: ladder.descriptor_json(),
            applied: 0,
        })
    }
    pub fn sample(&mut self, elapsed: Duration) -> Result<Admission, RelayError> {
        let snapshot = self.counters.snapshot();
        if snapshot.unsupported {
            self.controller.observe(None, elapsed);
            return Err(RelayError::UnsupportedFeedback);
        }
        Ok(self.controller.observe(Some(snapshot), elapsed))
    }
    pub fn snapshot(&self) -> moq_net::delivery::Snapshot {
        self.counters.snapshot()
    }
    /// Apply only to the original admitted family (descriptor includes generation and
    /// exact codec/dimensions/cadence/bitrate). Audio is untouched; removed video groups
    /// are canceled on their own tracks, never by aborting the healthy relay connection.
    pub fn apply_prefix(
        &mut self,
        publication: &mut SharedPublication,
        admission: Admission,
    ) -> Result<(), RelayError> {
        if publication.allocation_budget().is_none() {
            return Err(RelayError::InvalidBudget);
        }
        validate_prefix(
            &self.identity,
            publication,
            admission,
            self.controller.admission(),
        )?;
        let levels = publication.ladder().level_costs_bps().len();
        for level in 1..levels {
            publication
                .enable_trial_cancellation(level)
                .map_err(|_| RelayError::Publication)?;
        }
        if admission.level < self.applied {
            for level in admission.level + 1..=self.applied {
                publication
                    .cancel_trial(level)
                    .map_err(|_| RelayError::Publication)?;
            }
        }
        publication
            .set_stalled(&(admission.level + 1..levels).collect::<Vec<_>>())
            .map_err(|_| RelayError::Publication)?;
        self.applied = admission.level;
        Ok(())
    }
    pub async fn closed(&self) {
        let _ = self.connection.closed().await;
    }
    /// Explicit owner shutdown, not an ABR downshift operation.
    pub fn abort(&self) {
        self.connection.abort(moq_net::Error::Cancel);
    }
}

fn validate_prefix(
    identity: &Bytes,
    publication: &SharedPublication,
    admission: Admission,
    expected: Admission,
) -> Result<(), RelayError> {
    if publication.ladder().descriptor_json() != *identity {
        return Err(RelayError::WrongFamily);
    }
    if admission.level >= publication.ladder().level_costs_bps().len() || admission != expected {
        return Err(RelayError::InvalidBudget);
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::ladder::{AudioSpec, RenditionSpec};
    fn ladder(generation: u64) -> SharedLadder {
        SharedLadder::admit(
            generation,
            vec![
                RenditionSpec::new("low", "vp8", 320, 180, 30, 1, 100_000).unwrap(),
                RenditionSpec::new("mid", "vp8", 640, 360, 30, 1, 500_000).unwrap(),
            ],
            vec![AudioSpec::new("audio", "opus", 48_000, 2, 64_000).unwrap()],
            123,
        )
        .unwrap()
    }
    #[test]
    fn wrong_family_and_unapproved_prefix_are_rejected_before_publication_mutation() {
        let origin = moq_net::broadcast::Producer::new(Default::default());
        let mut publication = SharedPublication::create(ladder(1), &origin).unwrap();
        let current = Admission {
            level: 0,
            trial: false,
        };
        assert_eq!(
            validate_prefix(&ladder(2).descriptor_json(), &publication, current, current),
            Err(RelayError::WrongFamily)
        );
        assert_eq!(
            validate_prefix(
                &ladder(1).descriptor_json(),
                &publication,
                Admission {
                    level: 1,
                    trial: true
                },
                current
            ),
            Err(RelayError::InvalidBudget)
        );
        assert!(publication.stalled().is_empty());
        assert_eq!(publication.published_windows(), &[0, 0]);
        assert!(publication.enable_trial_cancellation(0).is_err());
        assert!(publication.cancel_trial(0).is_err());
        assert!(publication.audio_track("audio").is_some());
    }
    #[test]
    fn family_budget_failure_publishes_no_partial_audio_and_tracks_live_owner_bytes() {
        let origin = moq_net::broadcast::Producer::new(Default::default());
        let budget = crate::allocation::AllocationBudget::new(20).unwrap();
        let mut publication =
            SharedPublication::create_with_budget(ladder(1), &origin, budget.clone()).unwrap();
        let packets = vec![
            crate::Packet {
                payload: Bytes::from_static(&[1; 8]),
                pts_us: 1,
                verified_rap: true,
            },
            crate::Packet {
                payload: Bytes::from_static(&[2; 8]),
                pts_us: 2,
                verified_rap: true,
            },
        ];
        assert!(publication.publish_audio("audio", &packets).is_err());
        assert_eq!(
            budget.retained_bytes(),
            0,
            "failed preflight refunds every reservation"
        );
        publication.publish_audio("audio", &packets[..1]).unwrap();
        assert!(budget.retained_bytes() > 0);
        let before = budget.retained_bytes();
        publication.enable_trial_cancellation(1).unwrap();
        publication.cancel_trial(1).unwrap();
        assert_eq!(
            budget.retained_bytes(),
            before,
            "video cancel never releases independent audio"
        );
        assert!(
            publication.publish_audio("audio", &packets[..1]).is_err(),
            "audio published once per PTS"
        );
    }
    #[test]
    fn fresh_outgoing_meter_is_not_capacity_evidence() {
        let counters = moq_net::delivery::Counters::default();
        let mut controller = Controller::new(&[100_000, 500_000], 64_000, 16_000).unwrap();
        for _ in 0..60 {
            controller.observe(Some(counters.snapshot()), Duration::from_secs(1));
        }
        assert_eq!(
            controller.admission(),
            Admission {
                level: 0,
                trial: false
            }
        );
    }
}
