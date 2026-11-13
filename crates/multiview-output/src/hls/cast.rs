//! The Cast packager: HLS with CMAF fMP4 segments, written to a directory.
//!
//! [`CastPackager`] takes the encoder's H.264 access units and AAC frames and
//! publishes, in one directory, a `master.m3u8`, a `video.m3u8` and (with
//! audio) an `audio.m3u8` whose segments are single-track CMAF files built by
//! [`super::cmaf`]. Video and audio are separate renditions with the same
//! media-sequence numbers for the same time ranges: audio segment `n` holds
//! the AAC frames that start inside video segment `n`.
//!
//! The packager is synchronous and single-threaded: one output sink thread
//! drives it and it performs filesystem I/O (never call it from the engine
//! clock). Every buffer is bounded; when a bound is exceeded a `push_*` call
//! returns an error and the caller starts a new packager (a new generation).
//!
//! ## Publishing
//!
//! A video segment is cut at the first IDR at or after `target - half a
//! frame` from the segment start, and always at an IDR that changes the
//! parameter sets. With audio configured, a closed video segment is published
//! together with its audio segment as soon as an audio frame at or after the
//! video segment's end has arrived, so both playlists always list exactly the
//! same sequence numbers and the same discontinuities.
//!
//! ## Gaps
//!
//! Lost input ([`CastPackager::note_gap`], or a timestamp step detected in
//! `push_video` / `push_audio`) closes the open segment pair with what it
//! holds, drops input up to the next IDR and marks the next pair
//! `EXT-X-DISCONTINUITY`. Timestamps are never rebased, sequence numbers and
//! file names never restart inside one packager. A closed video segment for
//! which no audio frame exists is dropped rather than paired with an empty
//! audio segment.
use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use multiview_core::time::{rescale, Rational};

use super::cmaf::{self, Sample};
use super::{AudioRendition, MasterPlaylist, MediaPlaylist, Segment, SegmentType, VariantStream};
use crate::epoch::SharedEpoch;
use crate::error::{Error, Result};

/// Samples per AAC frame.
const AAC_FRAME_SAMPLES: i64 = 1024;
/// Video units skipped while no parameter sets are known before
/// `push_video` reports the stream as unusable.
const MAX_UNITS_WITHOUT_PARAMETER_SETS: u32 = 300;
/// Audio buffered while video does not catch up before `push_audio` reports
/// an error: 10 s.
const MAX_AUDIO_AHEAD_SECONDS: i64 = 10;
/// Closed video segments that may wait for their audio.
const MAX_VIDEO_SEGMENTS_AWAITING_AUDIO: usize = 3;
/// An open video segment longer than this many target durations (no IDR
/// arrived) is an error.
const MAX_OPEN_SEGMENT_TARGETS: i128 = 10;
/// Samples an open video segment may hold.
const MAX_OPEN_SEGMENT_SAMPLES: usize = 4096;
/// Forward deviation (samples) of an audio frame's pts from the 1024-sample
/// grid that is still one continuous run (a step of up to 1.5 frames); a
/// larger step is a gap.
const MAX_AUDIO_DRIFT_SAMPLES: i64 = 512;
/// Consecutive segment pairs dropped for want of audio before the packager
/// reports an error.
const MAX_CONSECUTIVE_DROPPED_PAIRS: u32 = 8;
/// Decimal places of `EXTINF` (sample-exact durations such as
/// `94 * 1024 / 48000` need more than milliseconds).
const EXTINF_DECIMALS: usize = 6;
const NS_PER_SECOND: i64 = 1_000_000_000;

const VIDEO_PLAYLIST: &str = "video.m3u8";
const AUDIO_PLAYLIST: &str = "audio.m3u8";
const MASTER_PLAYLIST: &str = "master.m3u8";
const AUDIO_GROUP: &str = "aud";

/// The video rendition of a [`CastPackager`].
#[derive(Debug, Clone)]
pub struct CastVideoConfig {
    /// Seconds per `pts` / `dts` unit, e.g. `1001/30000`.
    pub time_base: Rational,
    /// Coded width in pixels (`RESOLUTION`, `tkhd`, `avc1`).
    pub width: u32,
    /// Coded height in pixels.
    pub height: u32,
    /// Encoder bit rate in bits per second; `None` measures it from the
    /// segments.
    pub bit_rate: Option<u64>,
    /// The encoder's out-of-band codec configuration: SPS + PPS as Annex B
    /// NAL units or as an `avcC` record. In-band parameter sets on a keyframe
    /// always take precedence.
    pub extradata: Option<Vec<u8>>,
}

/// The audio rendition of a [`CastPackager`] (AAC, 1024 samples per frame).
#[derive(Debug, Clone)]
pub struct CastAudioConfig {
    /// Seconds per `pts` unit, normally `1/sample_rate`.
    pub time_base: Rational,
    /// Sample rate in Hz; the audio track timescale.
    pub sample_rate: u32,
    /// Channel count (`CHANNELS`, `mp4a`).
    pub channels: u16,
    /// Encoder bit rate in bits per second; `None` measures it.
    pub bit_rate: Option<u64>,
    /// The encoder's `AudioSpecificConfig`. When `None` it is taken from the
    /// first ADTS header if the frames are ADTS framed, otherwise built for
    /// AAC-LC from `sample_rate` and `channels`.
    pub audio_specific_config: Option<Vec<u8>>,
}

/// Configuration of a [`CastPackager`].
#[derive(Debug, Clone)]
pub struct CastPackagerConfig {
    /// Output directory (created when missing). Files are written directly
    /// in it.
    pub dir: PathBuf,
    /// Unique per activation; part of every segment and init file name.
    pub generation: u64,
    /// Segments listed per playlist (10).
    pub window: usize,
    /// Target segment duration in nanoseconds (`2_000_000_000`).
    pub target_segment_ns: i64,
    /// The video rendition.
    pub video: CastVideoConfig,
    /// The audio rendition; `None` packages video only.
    pub audio: Option<CastAudioConfig>,
    /// The `EXT-X-PROGRAM-DATE-TIME` anchor source; `None` (or a still-empty
    /// cell) falls back to [`Self::wall_anchor`].
    pub epoch: Option<SharedEpoch>,
    /// `(pts in nanoseconds, Unix time in nanoseconds)`: the wall clock of
    /// one media instant, used when `epoch` gives none. A caller that feeds
    /// packets it buffered earlier (a pre-roll) passes the arrival time of a
    /// buffered packet here, so old packets are not stamped "now". `None`
    /// anchors on the system clock at the first video segment.
    pub wall_anchor: Option<(i64, i64)>,
}

/// One H.264 access unit, Annex B or AVCC framed.
#[derive(Debug, Clone, Copy)]
pub struct VideoUnit<'a> {
    /// Presentation timestamp in the video time base.
    pub pts: i64,
    /// Decode timestamp in the video time base.
    pub dts: i64,
    /// The encoder's keyframe flag. Informational: a segment starts only at
    /// a unit that holds an IDR slice, whatever this flag says.
    pub keyframe: bool,
    /// The packet source reports a decoder-configuration change at this unit
    /// (a routing take): the segment is cut at this unit when it is an IDR,
    /// otherwise at the next IDR.
    pub config_changed: bool,
    /// The access unit bytes.
    pub data: &'a [u8],
}

/// One AAC frame of 1024 samples, raw or ADTS framed.
#[derive(Debug, Clone, Copy)]
pub struct AudioUnit<'a> {
    /// Presentation timestamp in the audio time base.
    pub pts: i64,
    /// The frame bytes.
    pub data: &'a [u8],
}

/// What a [`CastPackager`] has published so far.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CastProgress {
    /// Video segments published (listed at some point).
    pub video_segments: u64,
    /// Audio segments published.
    pub audio_segments: u64,
    /// Whether `master.m3u8` has been written.
    pub master: bool,
}

/// `a` (in `tb_a` units) is at or after `b` (in `tb_b` units), exactly.
/// Both time bases have positive numerators and denominators.
fn time_ge(a: i64, tb_a: Rational, b: i64, tb_b: Rational) -> bool {
    let left = i128::from(a)
        .saturating_mul(i128::from(tb_a.num))
        .saturating_mul(i128::from(tb_b.den));
    let right = i128::from(b)
        .saturating_mul(i128::from(tb_b.num))
        .saturating_mul(i128::from(tb_a.den));
    left >= right
}

/// A timestamp in `tb` units as integer nanoseconds (round to nearest).
fn to_ns(value: i64, tb: Rational) -> i64 {
    rescale(value, tb, Rational::new(1, NS_PER_SECOND))
}

/// The seconds value of `ticks / timescale` for the playlist text layer.
fn seconds(ticks: u64, timescale: u32) -> f64 {
    Rational::new(
        i64::try_from(ticks).unwrap_or(i64::MAX),
        i64::from(timescale.max(1)),
    )
    .as_f64()
}

/// `ticks / timescale` seconds rounded to the nearest integer, at least 1
/// (an `EXT-X-TARGETDURATION` candidate).
fn rounded_seconds(ticks: u64, timescale: u32) -> u64 {
    let scale = u128::from(timescale.max(1));
    let rounded = (u128::from(ticks) * 2 + scale) / (scale * 2);
    u64::try_from(rounded).unwrap_or(u64::MAX).max(1)
}

fn io_error(what: &str, path: &Path, error: &std::io::Error) -> Error {
    Error::Output(format!("cast packager: {what} {}: {error}", path.display()))
}

/// Write `bytes` as `<path>.tmp`, then rename it into place (no fsync: a
/// media segment is immutable and only referenced after this returns).
fn write_renamed(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut name = path.as_os_str().to_owned();
    name.push(".tmp");
    let tmp = PathBuf::from(name);
    std::fs::write(&tmp, bytes).map_err(|e| io_error("writing", &tmp, &e))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        remove_quietly(&tmp);
        io_error("renaming into", path, &e)
    })
}

/// Best-effort unlink: a missing file or a transient error is not a failure.
fn remove_quietly(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) | Err(_) => {}
    }
}

/// Whether `text` is `<digits>-<digits>` (the `<generation>-<index>` part of
/// a packager file name).
fn is_generation_index(text: &str) -> bool {
    let digits = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
    text.split_once('-')
        .is_some_and(|(generation, index)| digits(generation) && digits(index))
}

/// Whether `name` is a file name a [`CastPackager`] writes: `master.m3u8`,
/// `video.m3u8`, `audio.m3u8`, `seg-v-<gen>-<n>.m4s`, `seg-a-<gen>-<n>.m4s`,
/// `init-v-<gen>-<k>.mp4`, `init-a-<gen>-<k>.mp4`, or one of those with
/// `.tmp` appended (the write-then-rename temporary).
#[must_use]
pub fn is_packager_file_name(name: &str) -> bool {
    let name = name.strip_suffix(".tmp").unwrap_or(name);
    if matches!(name, MASTER_PLAYLIST | VIDEO_PLAYLIST | AUDIO_PLAYLIST) {
        return true;
    }
    let segment = name.strip_suffix(".m4s").and_then(|stem| {
        stem.strip_prefix("seg-v-")
            .or_else(|| stem.strip_prefix("seg-a-"))
    });
    let init = name.strip_suffix(".mp4").and_then(|stem| {
        stem.strip_prefix("init-v-")
            .or_else(|| stem.strip_prefix("init-a-"))
    });
    segment.or(init).is_some_and(is_generation_index)
}

/// Delete the files a [`CastPackager`] wrote directly in `dir` (best effort):
/// only names [`is_packager_file_name`] accepts, only regular files (never a
/// symlink, never a sub-directory). Every other file is left alone, so a
/// directory that also holds an operator's files loses none of them.
pub fn purge_dir(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let ours = entry
            .file_name()
            .to_str()
            .is_some_and(is_packager_file_name);
        // `DirEntry::file_type` does not follow symlinks: a symlink with a
        // packager name is not a regular file and is left alone.
        if ours && entry.file_type().is_ok_and(|kind| kind.is_file()) {
            remove_quietly(&entry.path());
        }
    }
}

/// Maps video timestamps (time-base units) onto the track timescale.
#[derive(Debug, Clone, Copy)]
struct VideoClock {
    /// The reduced time base (seconds per timestamp unit).
    tb: Rational,
    /// Track timescale: the time-base denominator, or 90 kHz when that does
    /// not fit 32 bits.
    timescale: u32,
    /// Whether ticks are the exact product `value * tb.num`.
    exact: bool,
}

impl VideoClock {
    fn new(time_base: Rational) -> Self {
        let tb = time_base.reduce();
        match u32::try_from(tb.den) {
            Ok(timescale) => Self {
                tb,
                timescale,
                exact: true,
            },
            Err(_) => Self {
                tb,
                timescale: 90_000,
                exact: false,
            },
        }
    }

    /// A timestamp or a timestamp delta in track-timescale ticks.
    fn ticks(&self, value: i64) -> i64 {
        if self.exact {
            value.saturating_mul(self.tb.num)
        } else {
            rescale(value, self.tb, Rational::new(1, 90_000))
        }
    }
}

/// One buffered video sample of the open segment.
#[derive(Debug)]
struct PendingSample {
    /// Decode time in the video time base.
    dts: i64,
    /// Presentation time in the video time base.
    pts: i64,
    sync: bool,
    data: Vec<u8>,
}

/// The video segment being filled.
#[derive(Debug)]
struct OpenVideo {
    samples: Vec<PendingSample>,
    /// The segment follows a gap.
    after_gap: bool,
    sps: Vec<u8>,
    pps: Vec<u8>,
}

/// A closed video segment waiting to be published with its audio.
#[derive(Debug)]
struct ClosedVideo {
    /// First sample pts, video time base.
    start_pts: i64,
    /// The pts the next segment starts at (exclusive end), video time base.
    end_pts: i64,
    /// `baseMediaDecodeTime` in the track timescale.
    tfdt: u64,
    duration_ticks: u64,
    samples: Vec<Sample>,
    after_gap: bool,
    sps: Vec<u8>,
    pps: Vec<u8>,
}

#[derive(Debug)]
struct VideoState {
    config: CastVideoConfig,
    clock: VideoClock,
    /// The parameter sets in force (in-band, else from extradata).
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
    /// The parameter sets of the newest init file and that file's name.
    init: Option<(Vec<u8>, Vec<u8>, String)>,
    next_init_index: u64,
    open: Option<OpenVideo>,
    last_dts: Option<i64>,
    /// Nominal frame duration: the smallest positive dts delta seen.
    nominal: Option<i64>,
    /// Units dropped while no parameter sets were known.
    skipped_without_sets: u32,
    playlist: MediaPlaylist,
    peak_bps: u64,
    bytes: u64,
    ticks: u64,
}

#[derive(Debug)]
struct AudioFrame {
    /// Presentation time in samples (the audio track timescale).
    pts: i64,
    data: Vec<u8>,
}

#[derive(Debug)]
struct AudioState {
    config: CastAudioConfig,
    /// `1 / sample_rate`: the unit of every audio time held here.
    tb: Rational,
    asc: Option<Vec<u8>>,
    init_written: bool,
    /// Buffered frames: contiguous, 1024 samples apart.
    buffer: VecDeque<AudioFrame>,
    /// The pts (samples) the next frame has when the run is continuous: the
    /// previous frame's pts + 1024. Frames within the drift tolerance are
    /// snapped onto this grid.
    expected: Option<i64>,
    /// After an audio gap: the pts (samples) of the first frame after it.
    /// An IDR more than one frame earlier has no audio at its start and
    /// cannot start a run.
    resume_floor: Option<i64>,
    playlist: MediaPlaylist,
    peak_bps: u64,
    bytes: u64,
    ticks: u64,
}

/// The files of one published sequence number.
#[derive(Debug)]
struct PublishedFiles {
    video: String,
    audio: Option<String>,
    video_init: String,
}

/// The synchronous CMAF packager behind a Cast output. See the module docs.
#[derive(Debug)]
pub struct CastPackager {
    dir: PathBuf,
    generation: u64,
    window: usize,
    target_segment_ns: i64,
    epoch: Option<SharedEpoch>,
    wall_anchor: Option<(i64, i64)>,
    video: VideoState,
    audio: Option<AudioState>,
    /// Closed video segments not yet published (bounded).
    pending: VecDeque<ClosedVideo>,
    /// The start pts (video time base) of the current continuous run; `None`
    /// while input is dropped up to the next usable keyframe.
    run_start: Option<i64>,
    /// The caller reported lost input; handled by the next `push_*` call.
    gap_noted: bool,
    /// The next opened video segment follows a gap.
    gap_pending: bool,
    /// A unit reported `config_changed`; the next IDR cuts the segment.
    config_change_pending: bool,
    /// Segment pairs dropped in a row for want of audio.
    dropped_pairs: u32,
    /// `(wall ns, media ns)`: the single `EXT-X-PROGRAM-DATE-TIME` anchor.
    anchor: Option<(i64, i64)>,
    next_sequence: u64,
    /// `tfdt` + duration of the last published video / audio segment.
    video_end: Option<u64>,
    audio_end: Option<u64>,
    target_duration: u64,
    /// Listed and retained segment files, oldest first.
    on_disk: VecDeque<PublishedFiles>,
    /// Video init files on disk.
    inits: Vec<String>,
    /// `CODECS` and `BANDWIDTH` of the published master playlist.
    master_state: Option<(String, u64)>,
    progress: CastProgress,
}

fn new_playlist(window: usize, target_duration: u64) -> MediaPlaylist {
    let mut playlist = MediaPlaylist::new(SegmentType::Fmp4);
    playlist.set_window(window);
    playlist.set_target_duration(target_duration);
    playlist.set_extinf_decimals(EXTINF_DECIMALS);
    playlist
}

fn positive(tb: Rational) -> bool {
    tb.num > 0 && tb.den > 0
}

impl CastPackager {
    /// Create a packager writing into `config.dir` (created when missing).
    /// Nothing is written until the first usable keyframe arrives.
    ///
    /// # Errors
    /// Returns [`Error::Output`] when the configuration is unusable (a
    /// non-positive time base, zero dimensions, window, target or sample
    /// rate, or extradata that holds no SPS + PPS) or the directory cannot
    /// be created.
    pub fn new(config: CastPackagerConfig) -> Result<Self> {
        let bad = |what: &str| Error::Output(format!("cast packager: {what}"));
        if !positive(config.video.time_base) {
            return Err(bad("the video time base must be positive"));
        }
        if config.video.width == 0 || config.video.height == 0 {
            return Err(bad("the video dimensions must be non-zero"));
        }
        if config.window == 0 || config.target_segment_ns <= 0 {
            return Err(bad("window and target segment duration must be positive"));
        }
        let (sps, pps) = match config.video.extradata.as_deref() {
            None | Some([]) => (None, None),
            Some(extradata) => {
                let (sps, pps) = cmaf::avcc_or_annexb_parameter_sets(extradata)
                    .ok_or_else(|| bad("the video extradata holds no SPS and PPS"))?;
                (Some(sps), Some(pps))
            }
        };
        let target_duration = rounded_seconds(
            u64::try_from(config.target_segment_ns).unwrap_or(0),
            1_000_000_000,
        );
        let audio = match config.audio {
            None => None,
            Some(audio) => {
                if !positive(audio.time_base) || audio.sample_rate == 0 || audio.channels == 0 {
                    return Err(bad(
                        "the audio time base, sample rate and channels must be positive",
                    ));
                }
                Some(AudioState {
                    tb: Rational::new(1, i64::from(audio.sample_rate)),
                    asc: audio
                        .audio_specific_config
                        .clone()
                        .filter(|asc| !asc.is_empty()),
                    config: audio,
                    init_written: false,
                    buffer: VecDeque::new(),
                    expected: None,
                    resume_floor: None,
                    playlist: new_playlist(config.window, target_duration),
                    peak_bps: 0,
                    bytes: 0,
                    ticks: 0,
                })
            }
        };
        std::fs::create_dir_all(&config.dir)
            .map_err(|e| io_error("creating directory", &config.dir, &e))?;
        Ok(Self {
            dir: config.dir,
            generation: config.generation,
            window: config.window,
            target_segment_ns: config.target_segment_ns,
            epoch: config.epoch,
            wall_anchor: config.wall_anchor,
            video: VideoState {
                clock: VideoClock::new(config.video.time_base),
                config: config.video,
                sps,
                pps,
                init: None,
                next_init_index: 0,
                open: None,
                last_dts: None,
                nominal: None,
                skipped_without_sets: 0,
                playlist: new_playlist(config.window, target_duration),
                peak_bps: 0,
                bytes: 0,
                ticks: 0,
            },
            audio,
            pending: VecDeque::new(),
            run_start: None,
            gap_noted: false,
            gap_pending: false,
            config_change_pending: false,
            dropped_pairs: 0,
            anchor: None,
            next_sequence: 0,
            video_end: None,
            audio_end: None,
            target_duration,
            on_disk: VecDeque::new(),
            inits: Vec::new(),
            master_state: None,
            progress: CastProgress::default(),
        })
    }

    /// What has been published so far.
    #[must_use]
    pub const fn published(&self) -> CastProgress {
        self.progress
    }

    /// Whether a player can start: the master playlist is published and each
    /// media playlist lists at least three segments.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.progress.master
            && self.video.playlist.segment_count() >= 3
            && self
                .audio
                .as_ref()
                .is_none_or(|audio| audio.playlist.segment_count() >= 3)
    }

    /// Report that input was lost upstream (a full queue dropped packets).
    /// The open segments are closed and a discontinuity is declared by the
    /// next `push_video` / `push_audio` call (see the module docs).
    pub fn note_gap(&mut self) {
        self.gap_noted = true;
    }
}

/// What the audio buffer holds for one closed video segment.
enum AudioTake {
    /// Later frames may still belong to the segment.
    Wait,
    /// No frame starts at the segment start: the pair cannot be published.
    Missing,
    /// The frames of the segment, in order.
    Frames(Vec<AudioFrame>),
}

impl AudioState {
    /// Take the frames with pts in `[start, end)` (video time base `vtb`).
    /// The range is complete once a frame at or after `end` is buffered;
    /// `force` takes what is there.
    fn take_segment(&mut self, start: i64, end: i64, vtb: Rational, force: bool) -> AudioTake {
        self.drop_before(start, vtb);
        let mut count = 0usize;
        let mut complete = false;
        for frame in &self.buffer {
            if time_ge(frame.pts, self.tb, end, vtb) {
                complete = true;
                break;
            }
            count = count.saturating_add(1);
        }
        if !complete && !force {
            return AudioTake::Wait;
        }
        // The first frame must start within one frame of the video start.
        let aligned = self.buffer.front().is_some_and(|frame| {
            !time_ge(
                frame.pts.saturating_sub(AAC_FRAME_SAMPLES),
                self.tb,
                start,
                vtb,
            )
        });
        if count == 0 || !aligned {
            return AudioTake::Missing;
        }
        AudioTake::Frames(self.buffer.drain(..count).collect())
    }

    /// Drop the buffered frames that start before `start` (time base `vtb`).
    fn drop_before(&mut self, start: i64, vtb: Rational) {
        while let Some(front) = self.buffer.front() {
            if time_ge(front.pts, self.tb, start, vtb) {
                break;
            }
            self.buffer.pop_front();
        }
    }

    /// Samples between the oldest and the newest buffered frame.
    fn buffered_samples(&self) -> i64 {
        match (self.buffer.front(), self.buffer.back()) {
            (Some(front), Some(back)) => back.pts.saturating_sub(front.pts),
            _ => 0,
        }
    }
}

impl CastPackager {
    /// Feed one H.264 access unit (decode order).
    ///
    /// # Errors
    /// Returns [`Error::Output`] when the unit cannot be parsed, a timestamp
    /// is negative, no parameter sets are known after 300 units ("no H.264
    /// parameter sets in-band or in extradata"), an open segment exceeds its
    /// bounds (no IDR arrives), audio falls more than three segments behind
    /// or is missing for more than eight segments in a row, or a file cannot
    /// be written. After an error the packager must be
    /// replaced by a new one (a new generation).
    pub fn push_video(&mut self, unit: VideoUnit<'_>) -> Result<()> {
        if self.gap_noted {
            self.handle_gap()?;
        }
        if let Some(previous) = self.video.last_dts {
            let delta = unit.dts.saturating_sub(previous);
            let nominal = self.video.nominal.unwrap_or(delta);
            if delta <= 0 || i128::from(delta) * 2 > i128::from(nominal) * 3 {
                self.handle_gap()?;
            } else {
                self.video.nominal = Some(nominal.min(delta));
            }
        }
        self.video.last_dts = Some(unit.dts);

        let access_unit = cmaf::h264_access_unit(unit.data, None)?;
        // In-band parameter sets always win over the ones in force (they
        // take effect at the next IDR, which then cuts the segment).
        if let Some(sps) = access_unit.sps {
            self.video.sps = Some(sps);
        }
        if let Some(pps) = access_unit.pps {
            self.video.pps = Some(pps);
        }
        if access_unit.data.is_empty() {
            return Ok(());
        }
        let sample = PendingSample {
            dts: unit.dts,
            pts: unit.pts,
            sync: access_unit.idr,
            data: access_unit.data,
        };
        self.config_change_pending |= unit.config_changed;
        if self.run_start.is_none() {
            return self.try_start_run(sample);
        }
        if sample.sync && self.should_cut(&unit) {
            self.close_open_segment(Some((sample.dts, sample.pts)))?;
            self.open_segment(sample);
        } else if let Some(open) = self.video.open.as_mut() {
            open.samples.push(sample);
            self.check_open_bounds()?;
        }
        self.publish_ready(false)
    }

    /// While no run is active: start one at an IDR with known parameter
    /// sets, drop everything else.
    fn try_start_run(&mut self, sample: PendingSample) -> Result<()> {
        let startable = sample.sync && sample.dts >= 0 && sample.pts >= 0;
        if !startable || self.video.sps.is_none() || self.video.pps.is_none() {
            // Audio older than this unit can never be part of the stream.
            if let Some(audio) = self.audio.as_mut() {
                audio.drop_before(sample.pts, self.video.clock.tb);
            }
        }
        if self.video.sps.is_none() || self.video.pps.is_none() {
            self.video.skipped_without_sets = self.video.skipped_without_sets.saturating_add(1);
            if self.video.skipped_without_sets >= MAX_UNITS_WITHOUT_PARAMETER_SETS {
                return Err(Error::Output(
                    "cast packager: no H.264 parameter sets in-band or in extradata".to_owned(),
                ));
            }
            return Ok(());
        }
        // A run starts at an IDR whose timestamps fit an unsigned `tfdt`.
        if !startable {
            return Ok(());
        }
        if self.anchor.is_none() {
            let media_ns = to_ns(sample.pts, self.video.clock.tb);
            let from_epoch = self
                .epoch
                .as_ref()
                .and_then(SharedEpoch::get)
                .map(|epoch| epoch.wall_at(media_ns));
            let from_caller = self
                .wall_anchor
                .map(|(pts_ns, unix_ns)| unix_ns.saturating_add(media_ns.saturating_sub(pts_ns)));
            let wall_ns = from_epoch.or(from_caller).unwrap_or_else(system_time_ns);
            self.anchor = Some((wall_ns, media_ns));
        }
        if let Some(audio) = self.audio.as_mut() {
            let vtb = self.video.clock.tb;
            let uncovered = audio.resume_floor.is_some_and(|floor| {
                time_ge(
                    floor.saturating_sub(AAC_FRAME_SAMPLES),
                    audio.tb,
                    sample.pts,
                    vtb,
                )
            });
            if uncovered {
                return Ok(());
            }
            audio.resume_floor = None;
            audio.drop_before(sample.pts, vtb);
        }
        self.run_start = Some(sample.pts);
        self.open_segment(sample);
        self.publish_ready(false)
    }

    /// Open a new video segment with its first (IDR) sample.
    fn open_segment(&mut self, first: PendingSample) {
        let after_gap = std::mem::take(&mut self.gap_pending);
        self.config_change_pending = false;
        self.video.open = Some(OpenVideo {
            samples: vec![first],
            after_gap,
            sps: self.video.sps.clone().unwrap_or_default(),
            pps: self.video.pps.clone().unwrap_or_default(),
        });
    }

    /// Whether the IDR `unit` closes the open segment: the parameter sets
    /// changed, the source reports a configuration change, or the segment
    /// reached the target duration minus half a frame.
    fn should_cut(&self, unit: &VideoUnit<'_>) -> bool {
        let Some(open) = self.video.open.as_ref() else {
            return false;
        };
        let sets_changed = self.video.sps.as_deref() != Some(open.sps.as_slice())
            || self.video.pps.as_deref() != Some(open.pps.as_slice());
        if sets_changed || self.config_change_pending {
            return true;
        }
        let Some(start) = open.samples.first().map(|sample| sample.pts) else {
            return false;
        };
        let frame = open
            .samples
            .last()
            .map_or(0, |last| unit.dts.saturating_sub(last.dts).max(0));
        // (pts - start) * tb >= target - frame * tb / 2, in exact integers.
        let tb = self.video.clock.tb;
        let elapsed = i128::from(unit.pts.saturating_sub(start)) * 2 + i128::from(frame);
        elapsed
            .saturating_mul(i128::from(tb.num))
            .saturating_mul(i128::from(NS_PER_SECOND))
            >= (i128::from(self.target_segment_ns) * 2).saturating_mul(i128::from(tb.den))
    }

    /// An open segment that never sees an IDR must not grow without bound.
    fn check_open_bounds(&self) -> Result<()> {
        let Some(open) = self.video.open.as_ref() else {
            return Ok(());
        };
        let (Some(first), Some(last)) = (open.samples.first(), open.samples.last()) else {
            return Ok(());
        };
        let tb = self.video.clock.tb;
        let span = i128::from(last.dts.saturating_sub(first.dts))
            .saturating_mul(i128::from(tb.num))
            .saturating_mul(i128::from(NS_PER_SECOND));
        let limit = (i128::from(self.target_segment_ns) * MAX_OPEN_SEGMENT_TARGETS)
            .saturating_mul(i128::from(tb.den));
        if open.samples.len() > MAX_OPEN_SEGMENT_SAMPLES || span > limit {
            return Err(Error::Output(format!(
                "cast packager: no IDR for {} samples; the open video segment exceeds its bound",
                open.samples.len()
            )));
        }
        Ok(())
    }
}

/// The system clock as integer nanoseconds past the Unix epoch.
fn system_time_ns() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_nanos()).unwrap_or(i64::MAX)
        })
}

impl CastPackager {
    /// Close the open video segment. `next` is the `(dts, pts)` of the IDR
    /// that starts the following segment; `None` (a gap) ends the segment
    /// one nominal frame after its last sample.
    fn close_open_segment(&mut self, next: Option<(i64, i64)>) -> Result<()> {
        let Some(open) = self.video.open.take() else {
            return Ok(());
        };
        let (Some(first), Some(last)) = (open.samples.first(), open.samples.last()) else {
            return Ok(());
        };
        // A gap right after the first sample of the stream: the frame
        // duration is not known yet, so the lone sample is not published.
        let Some(nominal) = self.video.nominal.or(next.map(|_| 1)) else {
            return Ok(());
        };
        let last_pts = open.samples.iter().map(|s| s.pts).max().unwrap_or(last.pts);
        let (next_decode, end_pts) = next.unwrap_or((
            last.dts.saturating_add(nominal),
            last_pts.saturating_add(nominal),
        ));
        let clock = self.video.clock;
        let bad_time = || {
            Error::Output("cast packager: video timestamps do not fit the CMAF track".to_owned())
        };
        let start_pts = first.pts;
        let tfdt = u64::try_from(clock.ticks(first.dts)).map_err(|_| bad_time())?;
        let next_dts: Vec<i64> = open
            .samples
            .iter()
            .skip(1)
            .map(|s| s.dts)
            .chain(std::iter::once(next_decode))
            .collect();
        let mut samples = Vec::with_capacity(open.samples.len());
        let mut duration_ticks = 0u64;
        for (sample, following) in open.samples.into_iter().zip(next_dts) {
            let at = clock.ticks(sample.dts);
            let duration = u32::try_from(clock.ticks(following).saturating_sub(at))
                .ok()
                .filter(|d| *d > 0)
                .ok_or_else(bad_time)?;
            let offset = i32::try_from(clock.ticks(sample.pts).saturating_sub(at))
                .map_err(|_| bad_time())?;
            duration_ticks = duration_ticks.saturating_add(u64::from(duration));
            samples.push(Sample {
                duration,
                flags: if sample.sync {
                    cmaf::SAMPLE_FLAGS_SYNC
                } else {
                    cmaf::SAMPLE_FLAGS_NON_SYNC
                },
                composition_offset: offset,
                data: sample.data,
            });
        }
        self.pending.push_back(ClosedVideo {
            start_pts,
            end_pts,
            tfdt,
            duration_ticks,
            samples,
            after_gap: open.after_gap,
            sps: open.sps,
            pps: open.pps,
        });
        Ok(())
    }

    /// Input was lost: publish what the open segment pair holds, then drop
    /// input until the next usable IDR. The pair after it is discontinuous.
    fn handle_gap(&mut self) -> Result<()> {
        self.gap_noted = false;
        self.video.last_dts = None;
        if self.run_start.is_none() {
            return Ok(());
        }
        self.close_open_segment(None)?;
        self.publish_ready(true)?;
        self.pending.clear();
        self.run_start = None;
        self.gap_pending = true;
        Ok(())
    }

    /// Publish every closed video segment whose audio is complete. With
    /// `force` the audio received so far is taken as complete.
    fn publish_ready(&mut self, force: bool) -> Result<()> {
        let vtb = self.video.clock.tb;
        while let Some(front) = self.pending.front() {
            let frames = match self.audio.as_mut() {
                None => None,
                Some(audio) => {
                    match audio.take_segment(front.start_pts, front.end_pts, vtb, force) {
                        AudioTake::Wait => break,
                        AudioTake::Missing => {
                            // No audio starts with this video segment: drop
                            // the pair (the next published one is marked
                            // discontinuous) rather than list an empty
                            // audio segment.
                            self.pending.pop_front();
                            self.dropped_pairs = self.dropped_pairs.saturating_add(1);
                            if !force && self.dropped_pairs > MAX_CONSECUTIVE_DROPPED_PAIRS {
                                return Err(Error::Output(format!(
                                    "cast packager: no audio for {} video segments in a row",
                                    self.dropped_pairs
                                )));
                            }
                            continue;
                        }
                        AudioTake::Frames(frames) => Some(frames),
                    }
                }
            };
            let Some(closed) = self.pending.pop_front() else {
                break;
            };
            self.publish_pair(&closed, frames)?;
        }
        if !force && self.pending.len() > MAX_VIDEO_SEGMENTS_AWAITING_AUDIO {
            return Err(Error::Output(format!(
                "cast packager: audio is more than {MAX_VIDEO_SEGMENTS_AWAITING_AUDIO} segments \
                 behind video"
            )));
        }
        Ok(())
    }
}

/// Bits per second of `bytes` spread over `ticks / timescale` seconds.
fn bits_per_second(bytes: u64, ticks: u64, timescale: u32) -> u64 {
    if ticks == 0 {
        return 0;
    }
    let bits = u128::from(bytes) * 8 * u128::from(timescale);
    u64::try_from(bits / u128::from(ticks)).unwrap_or(u64::MAX)
}

impl CastPackager {
    fn segment_pdt(&self, media_ns: i64) -> Option<i64> {
        self.anchor
            .map(|(wall_ns, anchor_ns)| wall_ns.saturating_add(media_ns.saturating_sub(anchor_ns)))
    }

    /// Make sure the init file for `sps` / `pps` exists; returns its name
    /// and whether it replaced an earlier init.
    fn ensure_video_init(&mut self, sps: &[u8], pps: &[u8]) -> Result<(String, bool)> {
        if let Some((cur_sps, cur_pps, name)) = self.video.init.as_ref() {
            if cur_sps == sps && cur_pps == pps {
                return Ok((name.clone(), false));
            }
        }
        let name = format!(
            "init-v-{}-{}.mp4",
            self.generation, self.video.next_init_index
        );
        let bytes = cmaf::video_init_segment(
            self.video.clock.timescale,
            self.video.config.width,
            self.video.config.height,
            sps,
            pps,
        );
        write_renamed(&self.dir.join(&name), &bytes)?;
        self.video.next_init_index = self.video.next_init_index.saturating_add(1);
        let replaced = self.video.init.is_some();
        self.video.init = Some((sps.to_vec(), pps.to_vec(), name.clone()));
        self.inits.push(name.clone());
        Ok((name, replaced))
    }

    fn audio_init_name(&self) -> String {
        format!("init-a-{}-0.mp4", self.generation)
    }

    /// Write the audio init file once, with the `AudioSpecificConfig` from
    /// the configuration, else from the first ADTS header, else built for
    /// AAC-LC from the configured rate and channels.
    fn ensure_audio_init(&mut self) -> Result<()> {
        let path = self.dir.join(self.audio_init_name());
        let Some(audio) = self.audio.as_mut() else {
            return Ok(());
        };
        if audio.init_written {
            return Ok(());
        }
        let asc = match audio.asc.clone() {
            Some(asc) => asc,
            None => {
                cmaf::aac_lc_audio_specific_config(audio.config.sample_rate, audio.config.channels)
                    .ok_or_else(|| {
                        Error::Output(format!(
                            "cast packager: no AudioSpecificConfig for {} Hz, {} channels",
                            audio.config.sample_rate, audio.config.channels
                        ))
                    })?
            }
        };
        let bit_rate = audio
            .config
            .bit_rate
            .map_or(0, |rate| u32::try_from(rate).unwrap_or(u32::MAX));
        let bytes = cmaf::audio_init_segment(
            audio.config.sample_rate,
            audio.config.channels,
            &asc,
            bit_rate,
            bit_rate,
        );
        write_renamed(&path, &bytes)?;
        audio.asc = Some(asc);
        audio.init_written = true;
        Ok(())
    }

    /// Publish one sequence number: the video segment, its audio segment,
    /// both playlists and (first time, or on a codec change) the master.
    fn publish_pair(
        &mut self,
        closed: &ClosedVideo,
        frames: Option<Vec<AudioFrame>>,
    ) -> Result<()> {
        let sequence = self.next_sequence;
        let mfhd = u32::try_from(sequence.saturating_add(1)).unwrap_or(u32::MAX);
        let (init_name, init_replaced) = self.ensure_video_init(&closed.sps, &closed.pps)?;
        self.ensure_audio_init()?;
        let audio_start = frames
            .as_ref()
            .and_then(|f| f.first())
            .and_then(|frame| u64::try_from(frame.pts).ok());
        let continuous = self.video_end == Some(closed.tfdt)
            && (frames.is_none() || self.audio_end == audio_start);
        let discontinuity = self.progress.video_segments > 0
            && (closed.after_gap || init_replaced || self.dropped_pairs > 0 || !continuous);
        self.target_duration = self.target_duration.max(rounded_seconds(
            closed.duration_ticks,
            self.video.clock.timescale,
        ));

        // Audio first: the video playlist never lists a sequence number the
        // audio playlist lacks.
        let mut audio_name = None;
        if let (Some(frames), Some(tfdt)) = (frames, audio_start) {
            audio_name = Some(self.publish_audio(sequence, mfhd, tfdt, frames, discontinuity)?);
        }

        let video_name = format!("seg-v-{}-{sequence}.m4s", self.generation);
        let bytes = cmaf::media_segment(mfhd, closed.tfdt, &closed.samples)?;
        write_renamed(&self.dir.join(&video_name), &bytes)?;
        let video = &mut self.video;
        let size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        video.bytes = video.bytes.saturating_add(size);
        video.ticks = video.ticks.saturating_add(closed.duration_ticks);
        video.peak_bps = video.peak_bps.max(bits_per_second(
            size,
            closed.duration_ticks,
            video.clock.timescale,
        ));
        let mut segment = Segment::new(
            video_name.clone(),
            seconds(closed.duration_ticks, video.clock.timescale),
        )
        .with_map_uri(init_name.clone());
        segment.discontinuity = discontinuity;
        segment.program_date_time_ns =
            self.segment_pdt(to_ns(closed.start_pts, self.video.clock.tb));
        self.video.playlist.push_segment(segment);
        self.video
            .playlist
            .set_target_duration(self.target_duration);
        if let Some(audio) = self.audio.as_mut() {
            audio.playlist.set_target_duration(self.target_duration);
            super::live::atomic_write(
                &self.dir.join(AUDIO_PLAYLIST),
                audio.playlist.render().as_bytes(),
            )?;
            self.progress.audio_segments = self.progress.audio_segments.saturating_add(1);
        }
        super::live::atomic_write(
            &self.dir.join(VIDEO_PLAYLIST),
            self.video.playlist.render().as_bytes(),
        )?;
        self.progress.video_segments = self.progress.video_segments.saturating_add(1);
        self.video_end = Some(closed.tfdt.saturating_add(closed.duration_ticks));
        self.next_sequence = sequence.saturating_add(1);
        self.dropped_pairs = 0;
        self.on_disk.push_back(PublishedFiles {
            video: video_name,
            audio: audio_name,
            video_init: init_name,
        });
        self.write_master(&closed.sps)?;
        self.retire_old_files();
        Ok(())
    }
}

impl CastPackager {
    /// Write audio segment `sequence` and list it; returns the file name.
    fn publish_audio(
        &mut self,
        sequence: u64,
        mfhd: u32,
        tfdt: u64,
        frames: Vec<AudioFrame>,
        discontinuity: bool,
    ) -> Result<String> {
        let name = format!("seg-a-{}-{sequence}.m4s", self.generation);
        let init_name = self.audio_init_name();
        let frame_ticks = u32::try_from(AAC_FRAME_SAMPLES).unwrap_or(1024);
        let samples: Vec<Sample> = frames
            .into_iter()
            .map(|frame| Sample {
                duration: frame_ticks,
                flags: cmaf::SAMPLE_FLAGS_SYNC,
                composition_offset: 0,
                data: frame.data,
            })
            .collect();
        let ticks =
            u64::from(frame_ticks).saturating_mul(u64::try_from(samples.len()).unwrap_or(u64::MAX));
        let bytes = cmaf::media_segment(mfhd, tfdt, &samples)?;
        write_renamed(&self.dir.join(&name), &bytes)?;
        let start_ns = i64::try_from(tfdt).map_or(i64::MAX, |pts| {
            to_ns(
                pts,
                self.audio.as_ref().map_or(Rational::new(1, 1), |a| a.tb),
            )
        });
        let pdt = self.segment_pdt(start_ns);
        let Some(audio) = self.audio.as_mut() else {
            return Ok(name);
        };
        let rate = audio.config.sample_rate;
        let size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        audio.bytes = audio.bytes.saturating_add(size);
        audio.ticks = audio.ticks.saturating_add(ticks);
        audio.peak_bps = audio.peak_bps.max(bits_per_second(size, ticks, rate));
        self.target_duration = self.target_duration.max(rounded_seconds(ticks, rate));
        let mut segment = Segment::new(name.clone(), seconds(ticks, rate)).with_map_uri(init_name);
        segment.discontinuity = discontinuity;
        segment.program_date_time_ns = pdt;
        audio.playlist.push_segment(segment);
        self.audio_end = Some(tfdt.saturating_add(ticks));
        Ok(name)
    }

    /// Write `master.m3u8` the first time, and again when `CODECS` changes or
    /// a measured peak bit rate exceeds the advertised `BANDWIDTH`.
    fn write_master(&mut self, sps: &[u8]) -> Result<()> {
        let mut codecs = cmaf::avc1_codec_string(sps);
        let mut peak = self
            .video
            .config
            .bit_rate
            .unwrap_or_else(|| self.video.peak_bps.saturating_add(self.video.peak_bps / 10));
        let mut average = self.video.config.bit_rate.unwrap_or_else(|| {
            bits_per_second(
                self.video.bytes,
                self.video.ticks,
                self.video.clock.timescale,
            )
        });
        if let Some(audio) = self.audio.as_ref() {
            codecs.push(',');
            codecs.push_str(&cmaf::mp4a_codec_string(
                audio.asc.as_deref().unwrap_or_default(),
            ));
            peak = peak.saturating_add(
                audio
                    .config
                    .bit_rate
                    .unwrap_or_else(|| audio.peak_bps.saturating_add(audio.peak_bps / 10)),
            );
            average = average.saturating_add(audio.config.bit_rate.unwrap_or_else(|| {
                bits_per_second(audio.bytes, audio.ticks, audio.config.sample_rate)
            }));
        }
        let peak = peak.max(average).max(1);
        let unchanged = self
            .master_state
            .as_ref()
            .is_some_and(|(listed, bandwidth)| *listed == codecs && peak <= *bandwidth);
        if unchanged {
            return Ok(());
        }
        // Frames per second: one over (time base x nominal dts step).
        let tb = self.video.clock.tb;
        let step = self.video.nominal.unwrap_or(1).max(1);
        let frame_rate = Rational::new(tb.den, tb.num.saturating_mul(step)).as_f64();
        let variant = VariantStream::new(VIDEO_PLAYLIST, peak)
            .with_average_bandwidth(average.clamp(1, peak))
            .with_codecs(codecs.clone())
            .with_resolution(self.video.config.width, self.video.config.height)
            .with_frame_rate(frame_rate);
        let mut master = MasterPlaylist::new();
        master.push_variant(variant);
        if let Some(audio) = self.audio.as_ref() {
            master.set_audio_group_id(AUDIO_GROUP);
            master.push_audio_with_channels(
                AudioRendition {
                    name: "Program".to_owned(),
                    uri: AUDIO_PLAYLIST.to_owned(),
                    language: None,
                    default: true,
                },
                audio.config.channels.to_string(),
            );
        }
        super::live::atomic_write(&self.dir.join(MASTER_PLAYLIST), master.render().as_bytes())?;
        self.master_state = Some((codecs, peak));
        self.progress.master = true;
        Ok(())
    }

    /// Unlink the segment files that left the retention window (the listed
    /// window plus one more window) and the init files nothing refers to.
    fn retire_old_files(&mut self) {
        let keep = self.window.saturating_mul(2);
        while self.on_disk.len() > keep {
            let Some(old) = self.on_disk.pop_front() else {
                break;
            };
            remove_quietly(&self.dir.join(&old.video));
            if let Some(audio) = old.audio {
                remove_quietly(&self.dir.join(audio));
            }
        }
        let current = self.video.init.as_ref().map(|(_, _, name)| name.clone());
        let (on_disk, dir) = (&self.on_disk, &self.dir);
        self.inits.retain(|name| {
            let used = Some(name) == current.as_ref()
                || on_disk.iter().any(|files| &files.video_init == name);
            if !used {
                remove_quietly(&dir.join(name));
            }
            used
        });
    }

    /// Feed one AAC frame (1024 samples, raw or ADTS framed). Ignored when
    /// no audio rendition is configured.
    ///
    /// # Errors
    /// Returns [`Error::Output`] when more than 10 s of audio is buffered
    /// waiting for video, an ADTS header contradicts the configured sample
    /// rate, or a file cannot be written. After an error the packager must
    /// be replaced by a new one.
    pub fn push_audio(&mut self, unit: AudioUnit<'_>) -> Result<()> {
        let Some(audio) = self.audio.as_ref() else {
            return Ok(());
        };
        let pts = rescale(unit.pts, audio.config.time_base, audio.tb);
        // A step of more than 1.5 frames forward, or a step that does not
        // advance, is a gap. Anything closer is the same continuous run and
        // is snapped onto the 1024-sample grid.
        let on_grid = audio.expected.is_none_or(|expected| {
            let deviation = pts.saturating_sub(expected);
            deviation <= MAX_AUDIO_DRIFT_SAMPLES && deviation > -AAC_FRAME_SAMPLES
        });
        if self.gap_noted || !on_grid {
            self.handle_gap()?;
        }
        if !on_grid {
            if let Some(audio) = self.audio.as_mut() {
                audio.buffer.clear();
                audio.expected = None;
                audio.resume_floor = Some(pts);
            }
        }
        let limit =
            i64::from(audio_rate(self.audio.as_ref())).saturating_mul(MAX_AUDIO_AHEAD_SECONDS);
        let Some(audio) = self.audio.as_mut() else {
            return Ok(());
        };
        let pts = audio.expected.unwrap_or(pts);
        if pts < 0 {
            audio.expected = None;
            return Ok(());
        }
        let (payload, header) = cmaf::strip_adts(unit.data);
        if audio.asc.is_none() {
            if let Some(adts) = header {
                // The track timescale is the configured sample rate: a
                // stream at another rate would play at the wrong speed.
                if adts.sample_rate() != Some(audio.config.sample_rate) {
                    return Err(Error::Output(format!(
                        "cast packager: the ADTS header declares {:?} Hz but the audio rendition \
                         is configured for {} Hz",
                        adts.sample_rate(),
                        audio.config.sample_rate
                    )));
                }
                audio.asc = Some(adts.audio_specific_config());
            }
        }
        audio.buffer.push_back(AudioFrame {
            pts,
            data: payload.to_vec(),
        });
        audio.expected = Some(pts.saturating_add(AAC_FRAME_SAMPLES));
        if audio.buffered_samples() > limit {
            return Err(Error::Output(format!(
                "cast packager: more than {MAX_AUDIO_AHEAD_SECONDS} s of audio is buffered \
                 waiting for video"
            )));
        }
        self.publish_ready(false)
    }

    /// Delete every file this packager wrote (best effort). The playlists
    /// carry no generation in their names: they are left alone when
    /// `video.m3u8` already lists another generation's segments (a newer
    /// packager took over the directory).
    pub fn remove_files(self) {
        let mut names: Vec<String> = vec![self.audio_init_name()];
        let own_segments = format!("seg-v-{}-", self.generation);
        let taken_over = std::fs::read_to_string(self.dir.join(VIDEO_PLAYLIST))
            .is_ok_and(|text| !text.contains(&own_segments));
        if !taken_over {
            names.extend([MASTER_PLAYLIST, VIDEO_PLAYLIST, AUDIO_PLAYLIST].map(str::to_owned));
        }
        names.extend(self.inits.iter().cloned());
        for files in &self.on_disk {
            names.push(files.video.clone());
            names.extend(files.audio.clone());
        }
        // A write that failed half-way may have left the next sequence
        // number's files (or their temporaries) behind.
        names.push(format!(
            "seg-v-{}-{}.m4s",
            self.generation, self.next_sequence
        ));
        names.push(format!(
            "seg-a-{}-{}.m4s",
            self.generation, self.next_sequence
        ));
        names.push(format!(
            "init-v-{}-{}.mp4",
            self.generation, self.video.next_init_index
        ));
        for name in names {
            remove_quietly(&self.dir.join(format!("{name}.tmp")));
            remove_quietly(&self.dir.join(name));
        }
    }
}

fn audio_rate(audio: Option<&AudioState>) -> u32 {
    audio.map_or(0, |state| state.config.sample_rate)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_in_different_time_bases_compare_exactly() {
        let video = Rational::new(1001, 30_000);
        let audio = Rational::new(1, 48_000);
        // Frame 60 starts at 2.002 s = 96096 samples exactly.
        assert!(time_ge(96_096, audio, 60, video));
        assert!(!time_ge(96_095, audio, 60, video));
        assert!(time_ge(60, video, 96_096, audio));
        assert!(!time_ge(60, video, 96_097, audio));
        assert_eq!(to_ns(60, video), 2_002_000_000);
    }

    #[test]
    fn target_duration_candidates_round_to_nearest_and_never_reach_zero() {
        assert_eq!(rounded_seconds(60_060, 30_000), 2);
        assert_eq!(rounded_seconds(75_000, 30_000), 3, "2.5 s rounds up");
        assert_eq!(rounded_seconds(74_999, 30_000), 2);
        assert_eq!(
            rounded_seconds(3_003, 30_000),
            1,
            "a short segment still needs 1"
        );
        assert_eq!(rounded_seconds(96_256, 48_000), 2);
        assert_eq!(bits_per_second(250_250, 60_060, 30_000), 1_000_000);
        assert_eq!(bits_per_second(1, 0, 30_000), 0);
    }

    #[test]
    fn the_video_clock_uses_the_reduced_time_base_or_90_khz() {
        let clock = VideoClock::new(Rational::new(2002, 60_000));
        assert_eq!((clock.timescale, clock.exact), (30_000, true));
        assert_eq!(clock.ticks(60), 60_060);
        let nanos = VideoClock::new(Rational::new(1, 10_000_000_000));
        assert_eq!((nanos.timescale, nanos.exact), (90_000, false));
        assert_eq!(nanos.ticks(20_000_000_000), 180_000);
    }
}
