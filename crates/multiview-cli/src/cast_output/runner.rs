//! The cast sink: what the output's sink thread does with each packet.
//!
//! [`CastRunner`] is a plain state machine over [`CastInput`] values so it is
//! testable without an encoder; [`run_cast_sink`] is the thread body that
//! pulls the binding's packets and feeds it.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use multiview_ffmpeg::{StreamCodecParameters, StreamKind};

use multiview_core::time::Rational;
use multiview_output::hls::cast::{
    AudioUnit, CastAudioConfig, CastPackager, CastPackagerConfig, CastVideoConfig, VideoUnit,
};
use multiview_output::SharedEpoch;

use super::ring::{Preroll, PrerollRing, RingKind, RingPacket};
use super::{
    claim_cast_dir, next_generation, purge_owned, unix_nanos, CastGate, CAST_TARGET_SEGMENT_NANOS,
    CAST_WINDOW, PREROLL_BYTES, PREROLL_NANOS,
};
use crate::binding_delivery::LeasedPacket;

/// The least time between two packager restarts after an error.
const RESTART_INTERVAL: Duration = Duration::from_secs(1);

/// Which elementary stream a packet handed to the runner belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CastInputKind {
    /// An H.264 access unit of the output's video rendition.
    Video,
    /// An AAC frame of the output's program-audio slot.
    Audio,
}

/// One packet for the cast sink (borrowed from the delivery lease).
#[derive(Debug, Clone, Copy)]
pub(crate) struct CastInput<'a> {
    pub(crate) kind: CastInputKind,
    pub(crate) pts: i64,
    pub(crate) dts: i64,
    /// The access unit is an IDR (video only).
    pub(crate) keyframe: bool,
    /// The delivery reported a decoder-configuration change at this packet
    /// (a routing take moved the output onto another encoder).
    pub(crate) config_changed: bool,
    pub(crate) data: &'a [u8],
}

/// What a cast sink needs besides its packets.
#[derive(Debug, Clone)]
pub(crate) struct CastSetup {
    /// The output's config id.
    pub(crate) id: String,
    /// The directory the stream is written to.
    pub(crate) dir: PathBuf,
    /// The activation gate shared with the control plane.
    pub(crate) gate: Arc<CastGate>,
    /// The pipeline's outbound epoch (`EXT-X-PROGRAM-DATE-TIME`).
    pub(crate) epoch: SharedEpoch,
    /// The video rendition, from the opened encoder.
    pub(crate) video: CastVideoConfig,
    /// The audio rendition, when the run carries program audio.
    pub(crate) audio: Option<CastAudioConfig>,
}

/// See the module docs.
pub(crate) struct CastRunner {
    setup: CastSetup,
    ring: PrerollRing,
    packager: Option<CastPackager>,
    /// The gate generation being served (0 = no stream).
    serving: u64,
    /// The gate's dropped-packet count this runner has accounted for.
    seen_dropped: u64,
    /// After an error: when the next packager may start.
    retry_at: Option<Instant>,
    /// An error of the current activation was logged already.
    error_logged: bool,
    restarts: u64,
}

impl CastRunner {
    pub(crate) fn new(setup: CastSetup) -> Self {
        let audio_tb = setup
            .audio
            .as_ref()
            .map_or(Rational::new(1, 48_000), |audio| audio.time_base);
        let ring = PrerollRing::new(
            setup.video.time_base,
            audio_tb,
            PREROLL_NANOS,
            PREROLL_BYTES,
        );
        let seen_dropped = setup.gate.dropped_packets();
        Self {
            setup,
            ring,
            packager: None,
            serving: 0,
            seen_dropped,
            retry_at: None,
            error_logged: false,
            restarts: 0,
        }
    }

    /// Packager restarts after an error so far.
    pub(crate) const fn restarts(&self) -> u64 {
        self.restarts
    }

    /// Handle one packet. Never waits on anything but this thread's own
    /// disk writes.
    pub(crate) fn handle(&mut self, input: CastInput<'_>, now: Instant) {
        self.handle_at(input, now, unix_nanos());
    }

    /// [`Self::handle`] with the wall clock of the packet's arrival given
    /// (`unix_ns`, Unix nanoseconds).
    pub(crate) fn handle_at(&mut self, input: CastInput<'_>, now: Instant, unix_ns: i64) {
        let gate = Arc::clone(&self.setup.gate);
        if gate.is_disabled() {
            return;
        }
        // Packets this sink did not receive: what is buffered no longer
        // joins what comes next, and an open stream declares the gap.
        let dropped = gate.dropped_packets();
        if dropped != self.seen_dropped {
            self.seen_dropped = dropped;
            self.ring.clear();
            if let Some(packager) = self.packager.as_mut() {
                packager.note_gap();
            }
        }
        let Some((generation, preroll)) = gate.activation() else {
            if self.serving != 0 {
                self.stop_stream();
            }
            self.buffer(input, unix_ns);
            return;
        };
        if generation != self.serving {
            // A new activation (also right after a deactivation this thread
            // has not seen yet): a new stream with new file names.
            self.stop_stream();
            self.serving = generation;
            self.error_logged = false;
            self.retry_at = None;
            gate.set_error(None);
            let buffered = if preroll {
                self.ring.take()
            } else {
                self.ring.clear();
                Preroll::default()
            };
            self.start_packager(generation, buffered.wall_anchor, now);
            for packet in &buffered.packets {
                if !self.feed(ring_input(packet), now) {
                    break;
                }
            }
        } else if self.packager.is_none()
            && input.kind == CastInputKind::Video
            && input.keyframe
            && self.retry_at.is_some_and(|at| now >= at)
        {
            // After an error: a new generation (new file names) at a
            // keyframe, at most once per `RESTART_INTERVAL`.
            self.start_packager(next_generation(), None, now);
        }
        self.feed(input, now);
    }

    /// No packet arrived for a while: honour a deactivation anyway, so a
    /// stopped cast leaves no files behind while the feed is quiet.
    pub(crate) fn poll(&mut self) {
        if self.serving != 0 && self.setup.gate.activation().is_none() {
            self.stop_stream();
        }
    }

    /// End of run: close the stream and remove its files.
    pub(crate) fn finish(&mut self) {
        self.stop_stream();
        purge_owned(&self.setup.dir, &self.setup.id);
        self.setup.gate.end();
    }

    fn buffer(&mut self, input: CastInput<'_>, unix_ns: i64) {
        let packet = RingPacket {
            kind: match input.kind {
                CastInputKind::Video => RingKind::Video,
                CastInputKind::Audio => RingKind::Audio,
            },
            pts: input.pts,
            dts: input.dts,
            keyframe: input.keyframe,
            config_changed: input.config_changed,
            data: input.data.to_vec(),
        };
        match input.kind {
            CastInputKind::Video => self.ring.push_video(packet, unix_ns),
            CastInputKind::Audio => self.ring.push_audio(packet),
        }
    }

    fn stop_stream(&mut self) {
        self.packager = None;
        if self.serving != 0 {
            purge_owned(&self.setup.dir, &self.setup.id);
        }
        self.serving = 0;
    }

    fn start_packager(&mut self, generation: u64, wall_anchor: Option<(i64, i64)>, now: Instant) {
        let gate = Arc::clone(&self.setup.gate);
        gate.begin_generation(self.serving);
        // The directory may have changed since the run was built: nothing is
        // written into a directory that is not (or no longer) this output's.
        if let Err(reason) = claim_cast_dir(&self.setup.dir, &self.setup.id) {
            tracing::warn!(output = %self.setup.id, %reason, "cast output disabled for this run");
            gate.disable(reason);
            self.serving = 0;
            return;
        }
        let config = CastPackagerConfig {
            dir: self.setup.dir.clone(),
            generation,
            window: CAST_WINDOW,
            target_segment_ns: CAST_TARGET_SEGMENT_NANOS,
            video: self.setup.video.clone(),
            audio: self.setup.audio.clone(),
            epoch: Some(self.setup.epoch.clone()),
            wall_anchor,
        };
        match CastPackager::new(config) {
            Ok(packager) => self.packager = Some(packager),
            Err(error) => self.fail(&error.to_string(), now),
        }
    }

    /// Feed one packet to the open packager. Returns `false` when there is
    /// none (or it just failed).
    fn feed(&mut self, input: CastInput<'_>, now: Instant) -> bool {
        let Some(packager) = self.packager.as_mut() else {
            return false;
        };
        #[cfg(test)]
        self.setup.gate.run_test_hook();
        let result = match input.kind {
            CastInputKind::Video => packager.push_video(VideoUnit {
                pts: input.pts,
                dts: input.dts,
                keyframe: input.keyframe,
                config_changed: input.config_changed,
                data: input.data,
            }),
            CastInputKind::Audio => packager.push_audio(AudioUnit {
                pts: input.pts,
                data: input.data,
            }),
        };
        match result {
            Ok(()) => {
                let progress = packager.published();
                self.setup.gate.publish_progress(
                    progress.video_segments,
                    progress.audio_segments,
                    progress.master,
                );
                true
            }
            Err(error) => {
                self.fail(&error.to_string(), now);
                false
            }
        }
    }

    /// The packager failed: drop it and its files; a new generation starts
    /// at a keyframe after [`RESTART_INTERVAL`].
    fn fail(&mut self, reason: &str, now: Instant) {
        if !self.error_logged {
            self.error_logged = true;
            tracing::warn!(
                output = %self.setup.id,
                error = reason,
                "cast stream failed; restarting it at the next keyframe"
            );
        }
        self.packager = None;
        purge_owned(&self.setup.dir, &self.setup.id);
        self.restarts = self.restarts.saturating_add(1);
        self.setup.gate.set_error(Some(reason.to_owned()));
        self.setup.gate.note_restart();
        self.setup.gate.begin_generation(self.serving);
        self.retry_at = Some(now + RESTART_INTERVAL);
    }
}

fn ring_input(packet: &RingPacket) -> CastInput<'_> {
    CastInput {
        kind: match packet.kind {
            RingKind::Video => CastInputKind::Video,
            RingKind::Audio => CastInputKind::Audio,
        },
        pts: packet.pts,
        dts: packet.dts,
        keyframe: packet.keyframe,
        config_changed: packet.config_changed,
        data: &packet.data,
    }
}

/// How long the sink thread waits for a packet before it re-reads the gate.
const IDLE_POLL: Duration = Duration::from_millis(200);

/// What a finished cast sink reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CastSinkReport {
    /// Packets received from the delivery binding.
    pub(crate) packets: usize,
    /// Packager restarts after an error.
    pub(crate) restarts: u64,
    /// The sink panicked and the output was disabled for the run.
    pub(crate) panicked: bool,
}

/// The body of a cast output's sink thread: pull the binding's packets until
/// the run ends and feed the [`CastRunner`].
///
/// This is the only place a cast output touches the disk. A panic in the
/// packager path is caught here: the output is disabled for the run, its
/// packets are discarded from then on, and the thread ends normally, so the
/// run and every other output are unaffected.
pub(crate) fn run_cast_sink(
    setup: CastSetup,
    rx: &Receiver<LeasedPacket>,
    params: &StreamCodecParameters,
) -> CastSinkReport {
    let gate = Arc::clone(&setup.gate);
    let id = setup.id.clone();
    let has_audio = setup.audio.is_some();
    let mut runner = CastRunner::new(setup);
    let mut report = CastSinkReport {
        packets: 0,
        restarts: 0,
        panicked: false,
    };
    // The lease the last packet came from (identity only), to report a
    // decoder-configuration change once per delivered window.
    let mut last_lease = std::sync::Weak::new();
    let mut config_change_pending = false;
    loop {
        let leased = match rx.recv_timeout(IDLE_POLL) {
            Ok(leased) => leased,
            Err(RecvTimeoutError::Timeout) => {
                if !report.panicked {
                    runner.poll();
                }
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => break,
        };
        report.packets = report.packets.saturating_add(1);
        if report.panicked {
            continue;
        }
        let lease = Arc::downgrade(&leased.permit);
        if !last_lease.ptr_eq(&lease) {
            last_lease = lease;
            config_change_pending |= leased.permit.decoder_config_changed();
        }
        let packet = &leased.packet;
        let kind = match packet.kind() {
            StreamKind::Video => CastInputKind::Video,
            StreamKind::Audio if has_audio => CastInputKind::Audio,
            // A cast stream carries the program audio only (no discrete
            // track, no future stream kind).
            _ => continue,
        };
        let (Some(data), Some(pts)) = (packet.payload(), packet.pts()) else {
            continue;
        };
        let video = kind == CastInputKind::Video;
        let input = CastInput {
            kind,
            pts,
            dts: packet.dts().unwrap_or(pts),
            keyframe: video && params.is_decoder_ready_rap(packet),
            config_changed: video && std::mem::take(&mut config_change_pending),
            data,
        };
        let step = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runner.handle(input, Instant::now());
        }));
        if step.is_err() {
            report.panicked = true;
            let reason = format!(
                "cast output {id:?}: its packager stopped on an internal error; restart the \
                 engine to use this output again"
            );
            tracing::error!(output = %id, "cast sink panicked; this output is disabled for the run");
            gate.disable(reason);
        }
    }
    report.restarts = runner.restarts();
    if report.panicked {
        gate.end();
    } else {
        runner.finish();
    }
    report
}
