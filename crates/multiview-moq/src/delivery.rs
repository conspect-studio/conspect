//! Experimental relay-uplink controller. Not connected to production yet.
//! Consumes unique application payload ACK completions, never bytes_sent/cwnd/RTT.
//! One controller per publisher-to-relay connection; never attach viewer connections.
use moq_net::delivery::Snapshot;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Admission {
    pub level: usize,
    pub trial: bool,
}
#[derive(Debug)]
pub struct Controller {
    costs: Vec<u64>,
    level: usize,
    trial: bool,
    healthy: u32,
    baseline: Option<Snapshot>,
    warmup: Duration,
    elapsed: Duration,
    bytes: u64,
    pending: u64,
}
impl Controller {
    /// Costs are SUM(prefix video) + audio + explicit network overhead.
    /// Compute costs stay separately reserved by the encoder owner.
    pub fn new(video: &[u64], audio: u64, overhead: u64) -> Option<Self> {
        if video.is_empty()
            || video.len() > 8
            || video.contains(&0)
            || video.windows(2).any(|x| x[0] >= x[1])
        {
            return None;
        }
        let mut total = audio.checked_add(overhead)?;
        let mut costs = Vec::new();
        for rate in video {
            total = total.checked_add(*rate)?;
            costs.push(total);
        }
        Some(Self {
            costs,
            level: 0,
            trial: false,
            healthy: 0,
            baseline: None,
            warmup: Duration::ZERO,
            elapsed: Duration::ZERO,
            bytes: 0,
            pending: 0,
        })
    }
    pub fn admission(&self) -> Admission {
        Admission {
            level: self.level,
            trial: self.trial,
        }
    }
    fn rollback(&mut self) {
        if self.trial {
            self.level = self.level.saturating_sub(1);
            self.trial = false;
        }
        self.healthy = 0;
        self.warmup = Duration::ZERO;
        self.elapsed = Duration::ZERO;
        self.bytes = 0;
    }
    /// <=2s contiguous samples. Four-second windows amortize finite group ACK delay.
    /// Trial is actual candidate-prefix media, never padding. Caller MUST enforce
    /// trial deadline/memory bounds, reset canceled streams and keep audio reserved.
    /// Unknown or idle evidence cannot prove capacity or admit an upshift.
    pub fn observe(&mut self, sample: Option<Snapshot>, elapsed: Duration) -> Admission {
        let Some(s) = sample.filter(|s| {
            !s.invalid && !s.unsupported && !elapsed.is_zero() && elapsed <= Duration::from_secs(2)
        }) else {
            self.rollback();
            self.baseline = None;
            return self.admission();
        };
        let Some(previous) = self.baseline.replace(s) else {
            self.pending = s.pending;
            return self.admission();
        };
        if s.acknowledged < previous.acknowledged || s.abandoned < previous.abandoned {
            self.rollback();
            self.pending = s.pending;
            return self.admission();
        }
        if s.abandoned > previous.abandoned {
            if !self.trial {
                self.level = self.level.saturating_sub(1);
            }
            self.rollback();
            self.pending = s.pending;
            return self.admission();
        }
        if !self.warmup.is_zero() {
            self.warmup = self.warmup.saturating_sub(elapsed);
            self.pending = s.pending;
            return self.admission();
        }
        self.bytes = self
            .bytes
            .saturating_add(s.acknowledged - previous.acknowledged);
        self.elapsed += elapsed;
        if self.elapsed < Duration::from_secs(4) {
            return self.admission();
        }
        let bps = u128::from(self.bytes) * 8 * 1_000_000 / self.elapsed.as_micros();
        // Finite group ACK batching may leave one nominal second of payload pending
        // at either window edge. This is an explicit phase allowance, not ACK credit.
        let phase_allowance = self.costs[self.level] / 8;
        let draining = bps * 100 >= u128::from(self.costs[self.level]) * 90
            && s.pending <= self.pending.saturating_add(phase_allowance);
        if draining {
            if self.trial {
                self.trial = false;
                self.healthy = 0;
            } else {
                self.healthy += 1;
            }
            if self.healthy >= 3 && self.level + 1 < self.costs.len() {
                self.level += 1;
                self.trial = true;
                self.warmup = Duration::from_secs(2);
                self.healthy = 0;
            }
        } else {
            self.level = self.level.saturating_sub(1);
            self.trial = false;
            self.healthy = 0;
        }
        self.elapsed = Duration::ZERO;
        self.bytes = 0;
        self.pending = s.pending;
        self.admission()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn trials_use_real_delivery_and_shaped_failure_rolls_back() {
        let mut c = Controller::new(&[800, 1600], 80, 120).unwrap();
        let mut s = Snapshot::default();
        c.observe(Some(s), Duration::from_secs(1));
        for _ in 0..12 {
            s.acknowledged += 125;
            c.observe(Some(s), Duration::from_secs(1));
        }
        assert_eq!(
            c.admission(),
            Admission {
                level: 1,
                trial: true
            }
        );
        for _ in 0..6 {
            s.acknowledged += 125;
            s.pending += 100;
            c.observe(Some(s), Duration::from_secs(1));
        }
        assert_eq!(
            c.admission(),
            Admission {
                level: 0,
                trial: false
            }
        );
        for _ in 0..12 {
            s.acknowledged += 400;
            s.pending = 0;
            c.observe(Some(s), Duration::from_secs(1));
        }
        assert!(c.admission().trial);
        for _ in 0..6 {
            s.acknowledged += 400;
            c.observe(Some(s), Duration::from_secs(1));
        }
        assert_eq!(
            c.admission(),
            Admission {
                level: 1,
                trial: false
            }
        );
    }
    #[test]
    fn unsupported_snapshot_rolls_back_active_trial() {
        let mut controller = Controller::new(&[800, 1600], 80, 120).unwrap();
        let mut sample = Snapshot::default();
        controller.observe(Some(sample), Duration::from_secs(1));
        for _ in 0..12 {
            sample.acknowledged += 125;
            controller.observe(Some(sample), Duration::from_secs(1));
        }
        assert!(controller.admission().trial);
        sample.unsupported = true;
        controller.observe(Some(sample), Duration::from_secs(1));
        assert_eq!(
            controller.admission(),
            Admission {
                level: 0,
                trial: false
            }
        );
    }
    #[test]
    fn gaps_and_idle_cannot_raise() {
        let mut c = Controller::new(&[800, 1600], 80, 120).unwrap();
        for _ in 0..100 {
            c.observe(Some(Snapshot::default()), Duration::from_secs(1));
        }
        assert_eq!(
            c.admission(),
            Admission {
                level: 0,
                trial: false
            }
        );
        c.observe(None, Duration::from_secs(1));
        assert_eq!(c.admission().level, 0);
        assert!(Controller::new(&[u64::MAX], 1, 1).is_none());
    }
}
