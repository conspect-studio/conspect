//! Off-clock event journal worker: decode, time and persist raw marker evidence.
//!
//! Producers (the demux reader and the HLS coordinator) copy bytes and make one
//! `try_send`; a full queue drops and counts. All parsing, clock math and disk
//! IO happen on the dedicated `multiview-event-journal` thread. Nothing here
//! feeds a splice boundary, an overlay or automation: the source-event gate
//! keeps its own trust rules, and these times are labelled predictions.
use multiview_control::event_journal::{
    dedupe_key, rfc3339_ns, DedupeKey, JournalClockSource, JournalCounters, JournalLimits,
    JournalOrigin, JournalReader, JournalRecord, JournalStore, JournalTiming, JournalTimingClass,
    JournalTransport,
};
use multiview_control::source_events::{SourceClockDto, SourceMarkerDto};
use multiview_framestore::SwapCell;
use multiview_input::hls_journal::{decode_tag, scan_playlist, JournalTag};
use multiview_input::id3;
use multiview_input::scte::decode::{
    decode_section, pts_add, pts_delta, Command, DecodedSection, PTS_MODULUS,
};
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    path::PathBuf,
    sync::{
        atomic::Ordering,
        mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender, TrySendError},
        Arc, Mutex, OnceLock, PoisonError,
    },
    time::Duration,
};

/// Observations waiting for the worker.
pub const QUEUE_CAPACITY: usize = 512;
/// Largest payload journaled; bigger ones are counted as dropped.
pub const MAX_PAYLOAD_BYTES: usize = 65_536;
const MAX_SEGMENTS: usize = 64;
const MAX_MARKERS: usize = 32;
const MAINTAIN_EVERY: Duration = Duration::from_secs(1);
/// A clock anchor further than this from the marker is treated as stale.
const MAX_ANCHOR_DISTANCE_TICKS: i64 = 3600 * 90_000;
const PDT_UNCERTAINTY_MS: u64 = 100;
/// About one segment: the unknown delivery delay of a receive-time estimate.
const RECEIVE_UNCERTAINTY_MS: u64 = 6000;

/// What a demuxed metadata packet carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PacketKind {
    /// SCTE-35 section.
    Scte35,
    /// Timed ID3 tag.
    Id3,
}

/// One raw observation, as cheap to build as a copy of its bytes.
#[derive(Debug)]
pub enum Observation {
    /// A demuxed metadata packet.
    Packet {
        /// Source identity.
        source: String,
        /// Payload kind.
        kind: PacketKind,
        /// Exact payload bytes.
        bytes: Vec<u8>,
        /// Transport PID.
        pid: Option<u16>,
        /// Packet time on the 90 kHz clock, masked to 33 bits.
        pts_90k: Option<u64>,
        /// Host receive time.
        received_ns: i64,
    },
    /// One media playlist load.
    Playlist {
        /// Source identity.
        source: String,
        /// Playlist text.
        text: Arc<str>,
        /// Host receive time.
        received_ns: i64,
    },
    /// One fetched media segment.
    Segment {
        /// Source identity.
        source: String,
        /// Media sequence.
        media_sequence: u64,
        /// Segment URI, sanitized.
        uri: String,
        /// First PES PTS.
        first_pts: Option<u64>,
    },
    /// Persist pending repeat counters.
    Flush,
}

/// Nanoseconds past the Unix epoch on the host clock.
#[must_use]
pub fn wall_now_ns() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_nanos()).ok())
        .unwrap_or(0)
}

/// Rescale a packet timestamp to the 33-bit 90 kHz clock.
#[must_use]
pub fn to_90k(pts: i64, num: i32, den: i32) -> Option<u64> {
    if num <= 0 || den <= 0 {
        return None;
    }
    let ticks = i128::from(pts) * i128::from(num) * 90_000 / i128::from(den);
    u64::try_from(ticks.rem_euclid(i128::from(PTS_MODULUS))).ok()
}

/// Nonblocking handle held by producers.
#[derive(Clone)]
pub struct JournalFeed {
    tx: SyncSender<Observation>,
    counters: Arc<JournalCounters>,
}
impl JournalFeed {
    /// Offer one observation. Never blocks: a full queue drops and counts.
    pub fn offer(&self, observation: Observation) {
        if let Observation::Packet { bytes, .. } = &observation {
            if bytes.len() > MAX_PAYLOAD_BYTES {
                self.count_drop();
                return;
            }
        }
        if let Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) =
            self.tx.try_send(observation)
        {
            self.count_drop();
        }
    }
    fn count_drop(&self) {
        self.counters.dropped.fetch_add(1, Ordering::Relaxed);
    }
}

/// "Source PTS `raw_pts_90k` reaches the output at `wall_ns`".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OutputAnchor {
    /// Source PTS, 33-bit 90 kHz.
    pub raw_pts_90k: u64,
    /// Host wall-clock of that PTS on the output timeline.
    pub wall_ns: i64,
}
/// Reads a source's current source-to-output mapping, lock-free.
pub type OutputMap = Arc<dyn Fn() -> Option<OutputAnchor> + Send + Sync>;

/// A pairing of a source PTS with a UTC instant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClockAnchor {
    /// Source PTS.
    pub pts: u64,
    /// UTC of that PTS.
    pub utc_ns: i64,
    /// Which clock this came from.
    pub source: JournalClockSource,
    /// Estimated uncertainty.
    pub uncertainty_ms: u64,
}

fn ticks_to_ns(ticks: i64) -> i64 {
    ticks.saturating_mul(100_000) / 9
}

impl ClockAnchor {
    /// UTC of `pts`, or `None` if this anchor is too far away to trust.
    #[must_use]
    pub fn utc_of(&self, pts: u64) -> Option<i64> {
        let delta = pts_delta(pts, self.pts);
        (delta.abs() <= MAX_ANCHOR_DISTANCE_TICKS)
            .then(|| self.utc_ns.saturating_add(ticks_to_ns(delta)))
    }
    /// Source PTS of `utc_ns`.
    #[must_use]
    pub fn pts_of(&self, utc_ns: i64) -> u64 {
        let ticks = i128::from(utc_ns.saturating_sub(self.utc_ns)) * 9 / 100_000;
        let modulus = i128::from(PTS_MODULUS);
        let at = (i128::from(self.pts) + ticks).rem_euclid(modulus);
        u64::try_from(at).unwrap_or(self.pts)
    }
}

/// The clocks available for one marker, best first.
#[derive(Clone, Copy, Debug, Default)]
pub struct Clocks {
    /// `EXT-X-PROGRAM-DATE-TIME` paired with a segment's first PTS.
    pub pdt: Option<ClockAnchor>,
    /// ID3 UTC stamp paired with its PTS.
    pub id3: Option<ClockAnchor>,
    /// Source-to-output mapping.
    pub output: Option<OutputAnchor>,
}
impl Clocks {
    fn best_for(&self, pts: u64) -> Option<(ClockAnchor, i64)> {
        [self.pdt, self.id3]
            .into_iter()
            .flatten()
            .find_map(|anchor| anchor.utc_of(pts).map(|utc| (anchor, utc)))
    }
    fn airs(&self, pts: u64) -> Option<i64> {
        let output = self.output?;
        let delta = pts_delta(pts, output.raw_pts_90k);
        (delta.abs() <= MAX_ANCHOR_DISTANCE_TICKS)
            .then(|| output.wall_ns.saturating_add(ticks_to_ns(delta)))
    }
}

fn apply_end(timing: &mut JournalTiming, start_ns: Option<i64>, airs_ns: Option<i64>) {
    let Some(duration_ms) = timing.duration_ms else {
        return;
    };
    let span = i64::try_from(duration_ms)
        .unwrap_or(i64::MAX)
        .saturating_mul(1_000_000);
    timing.end_at = start_ns.map(|ns| rfc3339_ns(ns.saturating_add(span)));
    timing.airs_end_at = airs_ns.map(|ns| rfc3339_ns(ns.saturating_add(span)));
}

/// Derive every operator time of one SCTE-35 section.
#[must_use]
pub fn section_timing(
    section: &DecodedSection,
    packet_pts: Option<u64>,
    received_ns: i64,
    clocks: &Clocks,
) -> JournalTiming {
    let immediate = section.is_immediate();
    let splice_pts = if immediate {
        packet_pts.map(|pts| pts_add(pts, 0))
    } else {
        section.splice_pts()
    };
    let class = match (immediate, section.splice_pts()) {
        (true, _) => JournalTimingClass::Immediate,
        (false, Some(_)) => JournalTimingClass::Scheduled,
        (false, None) => JournalTimingClass::None,
    };
    let duration_ms = section.duration_ticks().map(|ticks| ticks / 90);
    let mut timing = JournalTiming {
        class,
        splice_pts,
        pre_roll_ms: None,
        duration_ms,
        clock_source: None,
        uncertainty_ms: None,
        start_at: None,
        end_at: None,
        airs_at: None,
        airs_end_at: None,
    };
    if class == JournalTimingClass::None {
        return timing;
    }
    let pre_roll_ticks = splice_pts.zip(packet_pts).map(|(s, p)| pts_delta(s, p));
    timing.pre_roll_ms = pre_roll_ticks.map(|ticks| ticks / 90);
    let start_ns = match splice_pts.and_then(|pts| clocks.best_for(pts)) {
        Some((anchor, utc)) => {
            timing.clock_source = Some(anchor.source);
            timing.uncertainty_ms = Some(anchor.uncertainty_ms);
            Some(utc)
        }
        None => {
            timing.clock_source = Some(JournalClockSource::ReceiveTime);
            timing.uncertainty_ms = Some(RECEIVE_UNCERTAINTY_MS);
            Some(received_ns.saturating_add(ticks_to_ns(pre_roll_ticks.unwrap_or(0))))
        }
    };
    let airs_ns = splice_pts.and_then(|pts| clocks.airs(pts));
    timing.start_at = start_ns.map(rfc3339_ns);
    timing.airs_at = airs_ns.map(rfc3339_ns);
    apply_end(&mut timing, start_ns, airs_ns);
    timing
}

/// Derive the times of a playlist cue from its date, on the PDT clock.
fn dated_timing(start_ns: i64, duration_ms: Option<u64>, clocks: &Clocks) -> JournalTiming {
    let splice_pts = clocks.pdt.map(|anchor| anchor.pts_of(start_ns));
    let airs_ns = splice_pts.and_then(|pts| clocks.airs(pts));
    let mut timing = JournalTiming {
        class: JournalTimingClass::Scheduled,
        splice_pts,
        pre_roll_ms: None,
        duration_ms,
        clock_source: Some(JournalClockSource::Pdt),
        uncertainty_ms: Some(PDT_UNCERTAINTY_MS),
        start_at: Some(rfc3339_ns(start_ns)),
        end_at: None,
        airs_at: airs_ns.map(rfc3339_ns),
        airs_end_at: None,
    };
    apply_end(&mut timing, Some(start_ns), airs_ns);
    timing
}

/// One line describing a decoded section.
#[must_use]
pub fn section_summary(section: &DecodedSection) -> String {
    let mut parts = vec![section.splice_command_name.to_owned()];
    if let Command::SpliceInsert(insert) = &section.splice_command {
        parts.push(format!("splice_event_id={}", insert.splice_event_id));
    }
    if let Some(segmentation) = section.primary_segmentation() {
        parts.push(format!(
            "segmentation_event_id={}",
            segmentation.segmentation_event_id
        ));
        if let Some(upid) = &segmentation.upid {
            parts.push(format!("{}={}", upid.upid_type_name, upid.value));
        }
    }
    if let Some(ticks) = section.duration_ticks() {
        parts.push(format!("duration={}.{:03}s", ticks / 90_000, ticks / 90 % 1000));
    }
    if !section.crc_ok {
        parts.push("CRC FAILED".to_owned());
    }
    parts.join(" ")
}

fn section_error(section: &DecodedSection) -> Option<String> {
    match (&section.parse_error, section.crc_ok) {
        (Some(error), _) => Some(error.clone()),
        (None, false) => Some(format!("CRC_32 mismatch, carried {}", section.crc_32)),
        (None, true) => None,
    }
}

fn section_identity(section: &DecodedSection) -> (Option<String>, Option<String>) {
    let segmentation = section.primary_segmentation();
    let id = segmentation.map(|s| s.segmentation_event_id).or(
        if let Command::SpliceInsert(insert) = &section.splice_command {
            Some(insert.splice_event_id)
        } else {
            None
        },
    );
    let upid = segmentation
        .and_then(|s| s.upid.as_ref())
        .filter(|upid| upid.upid_type != 0)
        .map(|upid| format!("{}: {}", upid.upid_type_name, upid.value));
    (id.map(|id| id.to_string()), upid)
}

struct SegmentContext {
    media_sequence: u64,
    uri: String,
    first_pts: Option<u64>,
}

#[derive(Default)]
struct SourceState {
    pdt_by_sequence: BTreeMap<u64, i64>,
    segments: VecDeque<SegmentContext>,
    pdt: Option<ClockAnchor>,
    id3: Option<ClockAnchor>,
    clock: Option<SourceClockDto>,
    markers: VecDeque<SourceMarkerDto>,
}
impl SourceState {
    fn pair_pdt(&mut self, media_sequence: u64) {
        let pts = self
            .segments
            .iter()
            .find(|s| s.media_sequence == media_sequence)
            .and_then(|s| s.first_pts);
        if let Some((pts, &utc_ns)) = pts.zip(self.pdt_by_sequence.get(&media_sequence)) {
            self.pdt = Some(ClockAnchor {
                pts,
                utc_ns,
                source: JournalClockSource::Pdt,
                uncertainty_ms: PDT_UNCERTAINTY_MS,
            });
        }
    }
    /// The segment carrying `pts`: the latest one starting at or before it.
    fn segment_at(&self, pts: u64) -> Option<&SegmentContext> {
        self.segments.iter().rev().find(|segment| {
            segment
                .first_pts
                .is_some_and(|first| (0..MAX_ANCHOR_DISTANCE_TICKS).contains(&pts_delta(pts, first)))
        })
    }
    fn transport(&self, pid: Option<u16>, pts: Option<u64>) -> JournalTransport {
        let segment = pts.and_then(|pts| self.segment_at(pts));
        JournalTransport {
            pid,
            media_sequence: segment.map(|s| s.media_sequence),
            segment_uri: segment.map(|s| s.uri.clone()),
            program_date_time: segment
                .and_then(|s| self.pdt_by_sequence.get(&s.media_sequence))
                .map(|ns| rfc3339_ns(*ns)),
        }
    }
    fn push_marker(&mut self, marker: SourceMarkerDto) {
        if self.markers.len() == MAX_MARKERS {
            self.markers.pop_back();
        }
        self.markers.push_front(marker);
    }
}

/// What the Events panel shows for one source.
#[derive(Clone, Debug, Default)]
pub struct LiveSummary {
    /// Latest embedded source clock.
    pub clock: Option<SourceClockDto>,
    /// Newest decoded markers first.
    pub markers: Vec<SourceMarkerDto>,
}
type LiveMap = HashMap<String, Arc<LiveSummary>>;
type LiveCell = Arc<SwapCell<Arc<LiveMap>>>;

fn output_registry() -> &'static Mutex<HashMap<String, OutputMap>> {
    static MAPS: OnceLock<Mutex<HashMap<String, OutputMap>>> = OnceLock::new();
    MAPS.get_or_init(Mutex::default)
}

/// Install `source`'s output mapping. Called once per connection, off-clock.
pub fn register_output(source: &str, map: OutputMap) {
    output_registry()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(source.to_owned(), map);
}

fn output_anchor(source: &str) -> Option<OutputAnchor> {
    let map = output_registry()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(source)
        .cloned()?;
    map()
}

/// The worker's state, drivable without a thread.
pub struct JournalWorker {
    store: JournalStore,
    sources: HashMap<String, SourceState>,
    live: LiveCell,
}

struct Draft {
    source: String,
    received_ns: i64,
    origin: JournalOrigin,
    name: String,
    summary: String,
    transport: JournalTransport,
    source_pts: Option<u64>,
    raw_encoding: &'static str,
    raw: String,
    decoded: Option<serde_json::Value>,
    timing: Option<JournalTiming>,
    error: Option<String>,
    clock_reference: bool,
}

impl JournalWorker {
    /// Build a worker over `store`.
    #[must_use]
    pub fn new(store: JournalStore) -> Self {
        Self {
            store,
            sources: HashMap::new(),
            live: Arc::new(SwapCell::new(Arc::new(HashMap::new()))),
        }
    }

    /// Persist repeats and apply retention.
    pub fn maintain(&mut self, now_ns: i64) {
        self.store.maintain(now_ns);
    }

    /// Persist every pending repeat counter.
    pub fn flush(&mut self, now_ns: i64) {
        self.store.flush(now_ns);
    }

    /// The published per-source summary.
    #[must_use]
    pub fn summary(&self, source: &str) -> Option<Arc<LiveSummary>> {
        self.live.load().get(source).cloned()
    }

    fn publish(&self, source: &str) {
        let Some(state) = self.sources.get(source) else {
            return;
        };
        let mut map = LiveMap::clone(&self.live.load());
        map.insert(
            source.to_owned(),
            Arc::new(LiveSummary {
                clock: state.clock.clone(),
                markers: state.markers.iter().cloned().collect(),
            }),
        );
        self.live.store(Arc::new(map));
    }

    fn write(&mut self, draft: Draft, key: DedupeKey) -> Option<String> {
        let mut record = JournalRecord {
            record_id: String::new(),
            source_id: draft.source,
            received_at: rfc3339_ns(draft.received_ns),
            received_ns: draft.received_ns.to_string(),
            origin: draft.origin,
            name: draft.name,
            summary: draft.summary,
            transport: draft.transport,
            source_pts: draft.source_pts,
            raw_encoding: draft.raw_encoding.to_owned(),
            raw: draft.raw,
            decoded: draft.decoded,
            timing: draft.timing,
            error: draft.error,
            clock_reference: draft.clock_reference,
            repeats: 0,
            last_seen_at: None,
        };
        match self.store.append(&mut record, Some(key), draft.received_ns) {
            Ok(()) => Some(record.record_id),
            Err(error) => {
                tracing::warn!(%error, "event journal write failed");
                None
            }
        }
    }

    /// Handle one observation.
    pub fn handle(&mut self, observation: Observation) {
        match observation {
            Observation::Packet {
                source,
                kind: PacketKind::Scte35,
                bytes,
                pid,
                pts_90k,
                received_ns,
            } => self.scte(&source, &bytes, pid, pts_90k, received_ns),
            Observation::Packet {
                source,
                kind: PacketKind::Id3,
                bytes,
                pid,
                pts_90k,
                received_ns,
            } => self.id3(&source, &bytes, pid, pts_90k, received_ns),
            Observation::Playlist {
                source,
                text,
                received_ns,
            } => self.playlist(&source, &text, received_ns),
            Observation::Segment {
                source,
                media_sequence,
                uri,
                first_pts,
            } => {
                let state = self.sources.entry(source).or_default();
                if state.segments.len() == MAX_SEGMENTS {
                    state.segments.pop_front();
                }
                state.segments.push_back(SegmentContext {
                    media_sequence,
                    uri,
                    first_pts,
                });
                state.pair_pdt(media_sequence);
            }
            Observation::Flush => self.flush(wall_now_ns()),
        }
    }

    fn clocks(&self, source: &str) -> Clocks {
        let state = self.sources.get(source);
        Clocks {
            pdt: state.and_then(|s| s.pdt),
            id3: state.and_then(|s| s.id3),
            output: output_anchor(source),
        }
    }

    fn scte(
        &mut self,
        source: &str,
        bytes: &[u8],
        pid: Option<u16>,
        pts: Option<u64>,
        received_ns: i64,
    ) {
        let key = dedupe_key(source, JournalOrigin::Scte35, 0, bytes);
        if self.store.repeat(&key, received_ns) {
            return;
        }
        let transport = self
            .sources
            .entry(source.to_owned())
            .or_default()
            .transport(pid, pts);
        let clocks = self.clocks(source);
        let mut draft = Draft {
            source: source.to_owned(),
            received_ns,
            origin: JournalOrigin::Scte35,
            name: "Malformed SCTE-35".to_owned(),
            summary: String::new(),
            transport,
            source_pts: pts,
            raw_encoding: "base64",
            raw: base64_of(bytes),
            decoded: None,
            timing: None,
            error: None,
            clock_reference: false,
        };
        let section = match decode_section(bytes) {
            Ok(section) => section,
            Err(error) => {
                draft.summary.clone_from(&error);
                draft.error = Some(error);
                self.write(draft, key);
                return;
            }
        };
        let timing = section_timing(&section, pts, received_ns, &clocks);
        draft.name = section.display_name();
        draft.summary = section_summary(&section);
        draft.error = section_error(&section);
        draft.decoded = serde_json::to_value(&section).ok();
        draft.timing = Some(timing.clone());
        let name = draft.name.clone();
        let Some(record_id) = self.write(draft, key) else {
            return;
        };
        // Heartbeats are journal evidence, not something to show as an event.
        if matches!(section.splice_command, Command::SpliceNull) && section.descriptors.is_empty() {
            return;
        }
        let (event_id, upid) = section_identity(&section);
        self.marker(source, record_id, name, received_ns, &timing, event_id, upid);
    }

    #[allow(clippy::too_many_arguments)] // reason: one flat row of independent display fields.
    fn marker(
        &mut self,
        source: &str,
        record_id: String,
        name: String,
        received_ns: i64,
        timing: &JournalTiming,
        event_id: Option<String>,
        upid: Option<String>,
    ) {
        let marker = SourceMarkerDto {
            record_id,
            name,
            received_at: rfc3339_ns(received_ns),
            start_at: timing.start_at.clone(),
            end_at: timing.end_at.clone(),
            airs_at: timing.airs_at.clone(),
            airs_end_at: timing.airs_end_at.clone(),
            duration_ms: timing.duration_ms,
            clock_source: timing.clock_source,
            uncertainty_ms: timing.uncertainty_ms,
            event_id,
            upid,
        };
        self.sources
            .entry(source.to_owned())
            .or_default()
            .push_marker(marker);
        self.publish(source);
    }

    fn id3(
        &mut self,
        source: &str,
        bytes: &[u8],
        pid: Option<u16>,
        pts: Option<u64>,
        received_ns: i64,
    ) {
        let key = dedupe_key(source, JournalOrigin::Id3, 0, bytes);
        if self.store.repeat(&key, received_ns) {
            return;
        }
        let state = self.sources.entry(source.to_owned()).or_default();
        let transport = state.transport(pid, pts);
        let mut draft = Draft {
            source: source.to_owned(),
            received_ns,
            origin: JournalOrigin::Id3,
            name: "Malformed ID3".to_owned(),
            summary: String::new(),
            transport,
            source_pts: pts,
            raw_encoding: "base64",
            raw: base64_of(bytes),
            decoded: None,
            timing: None,
            error: None,
            clock_reference: false,
        };
        match id3::parse(bytes) {
            Err(error) => {
                draft.error = Some(error.to_string());
                draft.summary = error.to_string();
            }
            Ok(tag) => {
                let decoded = id3::decode_tag(&tag);
                draft.clock_reference = id3::is_clock_only(&tag);
                let frames: Vec<&str> = decoded.frames.iter().map(|f| f.frame_id.as_str()).collect();
                draft.summary = frames.join(" ");
                draft.name = format!("ID3 {}", frames.first().copied().unwrap_or("tag"));
                if let Some(clock) = id3::clock_reference(&tag) {
                    draft.name = "Source clock (ID3 UTC)".to_owned();
                    draft.summary = rfc3339_ns(clock.utc_ns);
                    state.clock = Some(SourceClockDto {
                        utc: rfc3339_ns(clock.utc_ns),
                        offset_ms: clock.utc_ns.saturating_sub(received_ns) / 1_000_000,
                        clock_source: JournalClockSource::Id3Utc,
                        observed_at: rfc3339_ns(received_ns),
                    });
                    if let Some(anchor_pts) = clock.pts_90khz.or(pts) {
                        state.id3 = Some(ClockAnchor {
                            pts: anchor_pts,
                            utc_ns: clock.utc_ns,
                            source: JournalClockSource::Id3Utc,
                            uncertainty_ms: clock.resolution_ms,
                        });
                    }
                }
                draft.decoded = serde_json::to_value(&decoded).ok();
            }
        }
        self.write(draft, key);
        self.publish(source);
    }

    fn playlist(&mut self, source: &str, text: &str, received_ns: i64) {
        let scan = scan_playlist(text);
        let state = self.sources.entry(source.to_owned()).or_default();
        for &(sequence, pdt) in &scan.segment_pdt {
            state.pdt_by_sequence.insert(sequence, pdt);
            state.pair_pdt(sequence);
        }
        while state.pdt_by_sequence.len() > MAX_SEGMENTS {
            state.pdt_by_sequence.pop_first();
        }
        if let Some(&(_, pdt)) = scan.segment_pdt.last() {
            if state.id3.is_none() {
                state.clock = Some(SourceClockDto {
                    utc: rfc3339_ns(pdt),
                    offset_ms: pdt.saturating_sub(received_ns) / 1_000_000,
                    clock_source: JournalClockSource::Pdt,
                    observed_at: rfc3339_ns(received_ns),
                });
            }
        }
        for tag in &scan.tags {
            self.hls_tag(source, tag, received_ns);
        }
        if !scan.segment_pdt.is_empty() {
            self.publish(source);
        }
    }

    fn hls_tag(&mut self, source: &str, tag: &JournalTag, received_ns: i64) {
        let position = tag.media_sequence.unwrap_or(0);
        let key = dedupe_key(source, JournalOrigin::Hls, position, tag.line.as_bytes());
        if self.store.repeat(&key, received_ns) {
            return;
        }
        let decoded = decode_tag(&tag.line);
        let clocks = self.clocks(source);
        let start_ns = decoded.start_ns.or(tag.pdt_ns).map(|ns| {
            let elapsed = decoded.elapsed_ms.unwrap_or(0);
            ns.saturating_sub(i64::try_from(elapsed).unwrap_or(0).saturating_mul(1_000_000))
        });
        let timing = start_ns.map(|ns| dated_timing(ns, decoded.duration_ms, &clocks));
        let section = decoded.scte35.iter().find_map(|s| s.section.as_ref());
        let name = section.map_or_else(|| hls_name(&decoded.tag), DecodedSection::display_name);
        let (event_id, upid) = section.map_or((None, None), section_identity);
        let error = decoded.scte35.iter().find_map(|s| s.error.clone());
        let is_event = !matches!(decoded.tag.as_str(), "EXT-X-CUE-OUT-CONT" | "EXT-X-ASSET");
        let draft = Draft {
            source: source.to_owned(),
            received_ns,
            origin: JournalOrigin::Hls,
            name: name.clone(),
            summary: hls_summary(&decoded),
            transport: JournalTransport {
                pid: None,
                media_sequence: tag.media_sequence,
                segment_uri: tag.segment_uri.clone(),
                program_date_time: tag.pdt_ns.map(rfc3339_ns),
            },
            source_pts: None,
            raw_encoding: "text",
            raw: tag.line.clone(),
            decoded: serde_json::to_value(&decoded).ok(),
            timing: timing.clone(),
            error,
            clock_reference: false,
        };
        let Some(record_id) = self.write(draft, key) else {
            return;
        };
        if let Some(timing) = timing.filter(|_| is_event) {
            self.marker(source, record_id, name, received_ns, &timing, event_id, upid);
        }
    }
}

fn hls_name(tag: &str) -> String {
    match tag {
        "EXT-X-CUE-OUT" => "HLS Cue Out".to_owned(),
        "EXT-X-CUE-OUT-CONT" => "HLS Cue Out (continuing)".to_owned(),
        "EXT-X-CUE-IN" => "HLS Cue In".to_owned(),
        "EXT-X-DATERANGE" => "HLS Date Range".to_owned(),
        "EXT-X-ASSET" => "HLS Asset".to_owned(),
        other => other.trim_start_matches("EXT-").to_owned(),
    }
}

fn hls_summary(tag: &multiview_input::hls_journal::DecodedTag) -> String {
    let mut parts = vec![tag.tag.clone()];
    if let Some(ms) = tag.elapsed_ms {
        parts.push(format!("elapsed={}.{:03}s", ms / 1000, ms % 1000));
    }
    if let Some(ms) = tag.duration_ms {
        parts.push(format!("duration={}.{:03}s", ms / 1000, ms % 1000));
    }
    parts.extend(
        tag.scte35
            .iter()
            .filter_map(|s| s.section.as_ref())
            .map(section_summary),
    );
    parts.join(" ")
}

fn base64_of(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn run(mut worker: JournalWorker, rx: &Receiver<Observation>) {
    let mut last_maintain = std::time::Instant::now();
    loop {
        match rx.recv_timeout(MAINTAIN_EVERY) {
            Ok(observation) => worker.handle(observation),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                worker.flush(wall_now_ns());
                return;
            }
        }
        if last_maintain.elapsed() >= MAINTAIN_EVERY {
            last_maintain = std::time::Instant::now();
            worker.maintain(wall_now_ns());
        }
    }
}

struct Service {
    feed: JournalFeed,
    reader: JournalReader,
    live: LiveCell,
}
static SERVICE: OnceLock<Service> = OnceLock::new();

/// Start the process-wide journal once. A failed thread spawn leaves the
/// journal off and the run unaffected.
pub fn start(root: PathBuf, limits: JournalLimits) {
    if SERVICE.get().is_some() {
        return;
    }
    let counters = Arc::new(JournalCounters::default());
    let (tx, rx) = sync_channel(QUEUE_CAPACITY);
    let worker = JournalWorker::new(JournalStore::new(
        root.clone(),
        limits,
        Arc::clone(&counters),
    ));
    let live = Arc::clone(&worker.live);
    let spawned = std::thread::Builder::new()
        .name("multiview-event-journal".to_owned())
        .spawn(move || run(worker, &rx));
    if let Err(error) = spawned {
        tracing::warn!(%error, "event journal not started");
        return;
    }
    let _ = SERVICE.set(Service {
        feed: JournalFeed {
            tx,
            counters: Arc::clone(&counters),
        },
        reader: JournalReader::new(root, counters),
        live,
    });
}

/// The producer handle, when the journal is running.
#[must_use]
pub fn feed() -> Option<JournalFeed> {
    SERVICE.get().map(|service| service.feed.clone())
}

/// The API reader, when the journal is running.
#[must_use]
pub fn reader() -> Option<JournalReader> {
    SERVICE.get().map(|service| service.reader.clone())
}

/// Lock-free read of a source's panel summary.
#[must_use]
pub fn live_summary(source: &str) -> Option<Arc<LiveSummary>> {
    SERVICE
        .get()
        .and_then(|service| service.live.load().get(source).cloned())
}

/// Ask the worker to persist repeat counters before shutdown.
pub fn flush() {
    if let Some(feed) = feed() {
        feed.offer(Observation::Flush);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use multiview_control::event_journal::JournalQuery;

    const POS: &str =
        "/DA0AAAAAAAA///wBQb+cr0AUAAeAhxDVUVJSAAAjn/PAAGlmbAICAAAAAAsoKGKNAIAmsnRfg==";
    const SPLICE: u64 = 0x72BD_0050;
    const NOW: i64 = 1_793_000_000_000_000_000;

    fn section() -> Vec<u8> {
        base64::engine::general_purpose::STANDARD.decode(POS).unwrap()
    }
    fn decoded() -> DecodedSection {
        decode_section(&section()).unwrap()
    }
    fn anchor(source: JournalClockSource, utc_ns: i64) -> ClockAnchor {
        ClockAnchor {
            pts: SPLICE - 900_000,
            utc_ns,
            source,
            uncertainty_ms: 100,
        }
    }

    #[test]
    fn pre_roll_and_wall_clock_use_the_pdt_first() {
        let clocks = Clocks {
            pdt: Some(anchor(JournalClockSource::Pdt, NOW)),
            id3: Some(anchor(JournalClockSource::Id3Utc, NOW + 5_000_000_000)),
            output: None,
        };
        let t = section_timing(&decoded(), Some(SPLICE - 360_000), NOW, &clocks);
        assert_eq!(t.class, JournalTimingClass::Scheduled);
        assert_eq!(t.pre_roll_ms, Some(4000));
        assert_eq!(t.clock_source, Some(JournalClockSource::Pdt));
        assert_eq!(t.start_at, Some(rfc3339_ns(NOW + 10_000_000_000)));
        assert_eq!(t.duration_ms, Some(307_000));
        assert_eq!(t.end_at, Some(rfc3339_ns(NOW + 317_000_000_000)));
    }

    #[test]
    fn id3_utc_is_used_when_there_is_no_pdt() {
        let clocks = Clocks {
            id3: Some(ClockAnchor {
                uncertainty_ms: 1000,
                ..anchor(JournalClockSource::Id3Utc, NOW)
            }),
            ..Clocks::default()
        };
        let t = section_timing(&decoded(), Some(SPLICE), NOW, &clocks);
        assert_eq!(t.clock_source, Some(JournalClockSource::Id3Utc));
        assert_eq!(t.uncertainty_ms, Some(1000));
        assert_eq!(t.start_at, Some(rfc3339_ns(NOW + 10_000_000_000)));
    }

    #[test]
    fn receive_time_plus_pre_roll_is_the_labelled_fallback() {
        let t = section_timing(&decoded(), Some(SPLICE - 180_000), NOW, &Clocks::default());
        assert_eq!(t.clock_source, Some(JournalClockSource::ReceiveTime));
        assert_eq!(t.uncertainty_ms, Some(RECEIVE_UNCERTAINTY_MS));
        assert_eq!(t.start_at, Some(rfc3339_ns(NOW + 2_000_000_000)));
    }

    #[test]
    fn a_stale_anchor_falls_through_to_the_next_clock() {
        let clocks = Clocks {
            pdt: Some(ClockAnchor {
                pts: SPLICE.wrapping_add(4 * 3600 * 90_000),
                ..anchor(JournalClockSource::Pdt, NOW)
            }),
            ..Clocks::default()
        };
        let t = section_timing(&decoded(), Some(SPLICE), NOW, &clocks);
        assert_eq!(t.clock_source, Some(JournalClockSource::ReceiveTime));
    }

    #[test]
    fn output_time_comes_from_the_source_to_output_mapping_across_a_wrap() {
        let clocks = Clocks {
            output: Some(OutputAnchor {
                raw_pts_90k: PTS_MODULUS - 90_000,
                wall_ns: NOW,
            }),
            ..Clocks::default()
        };
        assert_eq!(clocks.airs(90_000), Some(NOW + 2_000_000_000));
        let clocks = Clocks {
            output: Some(OutputAnchor {
                raw_pts_90k: SPLICE - 1_080_000,
                wall_ns: NOW,
            }),
            ..Clocks::default()
        };
        let t = section_timing(&decoded(), Some(SPLICE), NOW, &clocks);
        assert_eq!(t.airs_at, Some(rfc3339_ns(NOW + 12_000_000_000)));
        assert_eq!(t.airs_end_at, Some(rfc3339_ns(NOW + 319_000_000_000)));
    }

    #[test]
    fn immediate_splices_stay_immediate_at_the_packet_pts() {
        // time_signal with time_specified_flag = 0.
        let mut s = vec![0xFC, 0x30, 0, 0, 0, 0, 0, 0, 0, 0, 0xFF, 0xF0, 1, 6, 0x7F, 0, 0];
        s[2] = u8::try_from(s.len() + 4 - 3).unwrap();
        let crc = multiview_input::mpegts::crc::crc32_mpeg2(&s);
        s.extend_from_slice(&crc.to_be_bytes());
        let section = decode_section(&s).unwrap();
        let t = section_timing(&section, Some(1000), NOW, &Clocks::default());
        assert_eq!(t.class, JournalTimingClass::Immediate);
        assert_eq!(t.splice_pts, Some(1000));
        assert_eq!(t.pre_roll_ms, Some(0));
        assert_eq!(t.start_at, Some(rfc3339_ns(NOW)));
    }

    #[test]
    fn packet_timestamps_rescale_and_mask_to_33_bits() {
        assert_eq!(to_90k(1000, 1, 1000), Some(90_000));
        assert_eq!(to_90k(1_i64 << 33, 1, 90_000), Some(0));
        assert_eq!(to_90k(5, 0, 90_000), None);
    }

    fn worker(dir: &std::path::Path) -> JournalWorker {
        JournalWorker::new(JournalStore::new(
            dir.to_path_buf(),
            JournalLimits::default(),
            Arc::default(),
        ))
    }
    fn packet(kind: PacketKind, bytes: Vec<u8>, pts: u64, received_ns: i64) -> Observation {
        Observation::Packet {
            source: "cnn".into(),
            kind,
            bytes,
            pid: Some(0x1E3),
            pts_90k: Some(pts),
            received_ns,
        }
    }
    fn records(dir: &std::path::Path) -> Vec<JournalRecord> {
        let reader = JournalReader::new(dir.to_path_buf(), Arc::default());
        let query = JournalQuery {
            limit: 100,
            ..JournalQuery::default()
        };
        reader.page("cnn", &query).unwrap().records
    }

    #[test]
    fn a_repeated_section_is_one_record_with_a_counter_and_one_marker() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = worker(dir.path());
        for n in 0..3 {
            w.handle(packet(PacketKind::Scte35, section(), SPLICE - 360_000, NOW + n));
        }
        w.flush(NOW + 10);
        let rows = records(dir.path());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].repeats, 2);
        assert_eq!(rows[0].transport.pid, Some(0x1E3));
        assert_eq!(rows[0].raw, POS);
        assert_eq!(rows[0].name, "Provider Placement Opportunity Start");
        assert_eq!(rows[0].decoded.as_ref().unwrap()["crc_ok"], true);
        assert_eq!(w.summary("cnn").unwrap().markers.len(), 1);
    }

    #[test]
    fn malformed_and_crc_failed_sections_are_recorded_with_the_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = worker(dir.path());
        let mut corrupt = section();
        *corrupt.last_mut().unwrap() ^= 1;
        w.handle(packet(PacketKind::Scte35, corrupt, 0, NOW));
        w.handle(packet(PacketKind::Scte35, vec![1, 2, 3], 0, NOW + 1));
        let rows = records(dir.path());
        assert_eq!(rows.len(), 2);
        assert!(rows[0].error.as_ref().unwrap().contains("table_id"));
        assert!(rows[0].decoded.is_none());
        assert!(rows[1].error.as_ref().unwrap().contains("CRC_32"));
        assert!(rows[1].decoded.is_some());
    }

    #[test]
    fn id3_utc_stamps_become_the_source_clock_and_time_later_cues() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = worker(dir.path());
        let mut payload = id3::ELEMENTAL_UTC_OWNER.as_bytes().to_vec();
        payload.push(0);
        payload.extend_from_slice(b"2026-11-03T21:14:58Z");
        let mut tag = b"ID3\x04\x00\x00\x00\x00".to_vec();
        let total = payload.len() + 10;
        tag.extend_from_slice(&[
            u8::try_from(total >> 7).unwrap(),
            u8::try_from(total & 0x7F).unwrap(),
        ]);
        tag.extend_from_slice(b"PRIV\x00\x00");
        tag.extend_from_slice(&[
            u8::try_from(payload.len() >> 7).unwrap(),
            u8::try_from(payload.len() & 0x7F).unwrap(),
            0,
            0,
        ]);
        tag.extend_from_slice(&payload);
        let utc = multiview_input::hls::parse_rfc3339_ns("2026-11-03T21:14:58Z").unwrap();
        w.handle(packet(PacketKind::Id3, tag, SPLICE - 900_000, utc + 2_000_000_000));
        let summary = w.summary("cnn").unwrap();
        let clock = summary.clock.as_ref().unwrap();
        assert_eq!(clock.offset_ms, -2000);
        assert!(summary.markers.is_empty());
        w.handle(packet(PacketKind::Scte35, section(), SPLICE - 90_000, utc + 9_000_000_000));
        let rows = records(dir.path());
        assert!(rows[1].clock_reference);
        let timing = rows[0].timing.as_ref().unwrap();
        assert_eq!(timing.clock_source, Some(JournalClockSource::Id3Utc));
        assert_eq!(timing.start_at, Some(rfc3339_ns(utc + 10_000_000_000)));
    }

    #[test]
    fn pdt_pairs_with_the_segment_pts_and_gives_packets_their_context() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = worker(dir.path());
        let text: Arc<str> = Arc::from(
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:7\n#EXT-X-PROGRAM-DATE-TIME:2026-11-03T21:14:58Z\n\
#EXTINF:6,\n7.ts\n#EXT-X-CUE-OUT:30\n#EXTINF:6,\n8.ts\n",
        );
        let utc = multiview_input::hls::parse_rfc3339_ns("2026-11-03T21:14:58Z").unwrap();
        for _ in 0..2 {
            w.handle(Observation::Playlist {
                source: "cnn".into(),
                text: Arc::clone(&text),
                received_ns: utc,
            });
        }
        w.handle(Observation::Segment {
            source: "cnn".into(),
            media_sequence: 7,
            uri: "7.ts".into(),
            first_pts: Some(SPLICE - 900_000),
        });
        w.handle(packet(PacketKind::Scte35, section(), SPLICE - 450_000, utc + 1));
        let rows = records(dir.path());
        assert_eq!(rows.len(), 2, "the reloaded tag is a repeat");
        assert_eq!(rows[0].transport.media_sequence, Some(7));
        assert_eq!(rows[0].transport.segment_uri.as_deref(), Some("7.ts"));
        assert_eq!(rows[0].transport.program_date_time, Some(rfc3339_ns(utc)));
        let timing = rows[0].timing.as_ref().unwrap();
        assert_eq!(timing.clock_source, Some(JournalClockSource::Pdt));
        assert_eq!(timing.start_at, Some(rfc3339_ns(utc + 10_000_000_000)));
        let cue = rows[1].timing.as_ref().unwrap();
        assert_eq!(cue.start_at, Some(rfc3339_ns(utc + 6_000_000_000)));
        assert_eq!(cue.duration_ms, Some(30_000));
    }

    #[test]
    fn a_full_queue_drops_and_counts_without_blocking() {
        let (tx, _rx) = sync_channel(1);
        let counters = Arc::new(JournalCounters::default());
        let feed = JournalFeed {
            tx,
            counters: Arc::clone(&counters),
        };
        for _ in 0..3 {
            feed.offer(Observation::Flush);
        }
        feed.offer(packet(PacketKind::Id3, vec![0; MAX_PAYLOAD_BYTES + 1], 0, 0));
        assert_eq!(counters.snapshot().dropped, 3);
    }

    /// Prints the hand-back example; run with `--nocapture`.
    #[test]
    fn example_record() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = worker(dir.path());
        w.handle(packet(PacketKind::Scte35, section(), SPLICE - 360_000, NOW));
        let row = serde_json::to_string_pretty(&records(dir.path())[0]).unwrap();
        assert!(row.contains("segmentation_event_id"));
        println!("{row}");
    }
}
