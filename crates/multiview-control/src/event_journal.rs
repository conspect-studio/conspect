//! Append-only per-source journal of every raw marker observation.
//!
//! One JSONL file per source per UTC day under `<state>/events/<source>/`. A
//! single [`JournalStore`] owner writes, on its own thread behind a bounded
//! queue, so a stalled disk only drops records. [`JournalReader`] reads the
//! same files from the request plane. Neither touches an engine thread.
//!
//! An exact repeat is not rewritten. The first sighting is a record; later
//! sightings accumulate and land as a small `repeat` line, which readers fold
//! back into the record's `repeats` counter, so the files stay append-only.
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::{
    collections::{HashMap, VecDeque},
    fs::{self, File, OpenOptions},
    io::{self, BufRead as _, BufReader, Write as _},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

/// Default retention in days.
pub const DEFAULT_RETENTION_DAYS: u32 = 14;
/// Default total size cap in bytes, across all sources.
pub const DEFAULT_MAX_BYTES: u64 = 256 * 1024 * 1024;
/// Largest page the API serves.
pub const MAX_PAGE: usize = 500;
/// Page size when the caller names none.
pub const DEFAULT_PAGE: usize = 100;

const NS_PER_SECOND: i64 = 1_000_000_000;
const DEDUPE_WINDOW_NS: i64 = 60 * NS_PER_SECOND;
const REPEAT_FLUSH_NS: i64 = 30 * NS_PER_SECOND;
const RETENTION_INTERVAL_NS: i64 = 60 * NS_PER_SECOND;
const MAX_DEDUPE_ENTRIES: usize = 4096;
const MAX_FOLDED_REPEATS: usize = 65_536;

/// Where a marker was carried.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub enum JournalOrigin {
    /// SCTE-35 section on a transport PID.
    Scte35,
    /// Timed ID3 tag.
    Id3,
    /// HLS playlist tag.
    Hls,
}

/// Which clock produced a wall-clock prediction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub enum JournalClockSource {
    /// `EXT-X-PROGRAM-DATE-TIME` tied to the segment's first PTS.
    Pdt,
    /// The encoder's UTC stamp from an ID3 `PRIV` frame.
    Id3Utc,
    /// Host receive time plus the marker's pre-roll.
    ReceiveTime,
}

/// How a marker's time was derived.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub enum JournalTimingClass {
    /// The marker names a future or past splice point.
    Scheduled,
    /// The marker takes effect at its carrying packet.
    Immediate,
    /// The marker carries no time.
    None,
}

/// Transport context of one observation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct JournalTransport {
    /// Transport PID.
    pub pid: Option<u16>,
    /// HLS media sequence of the carrying segment.
    pub media_sequence: Option<u64>,
    /// Carrying segment URI, without query or credentials.
    pub segment_uri: Option<String>,
    /// That segment's `EXT-X-PROGRAM-DATE-TIME`.
    pub program_date_time: Option<String>,
}

/// Times derived for one marker. Predictions, never engine state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct JournalTiming {
    /// How the time was derived.
    pub class: JournalTimingClass,
    /// Splice point on the source timeline, 90 kHz, after `pts_adjustment`.
    pub splice_pts: Option<u64>,
    /// Splice point minus carrying packet time; negative means already past.
    pub pre_roll_ms: Option<i64>,
    /// Declared duration.
    pub duration_ms: Option<u64>,
    /// Clock used for the wall-clock fields.
    pub clock_source: Option<JournalClockSource>,
    /// Estimated uncertainty of that clock.
    pub uncertainty_ms: Option<u64>,
    /// Predicted splice wall-clock on the source, UTC.
    pub start_at: Option<String>,
    /// Predicted break end on the source, UTC.
    pub end_at: Option<String>,
    /// Predicted instant the splice reaches this multiview's output, UTC.
    pub airs_at: Option<String>,
    /// Predicted instant the break end reaches this multiview's output, UTC.
    pub airs_end_at: Option<String>,
}

/// One raw observation with its full decode.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct JournalRecord {
    /// Sortable identity, assigned by the writer; also the page cursor.
    #[serde(default)]
    pub record_id: String,
    /// Exact source identity.
    pub source_id: String,
    /// Host receive time, UTC.
    pub received_at: String,
    /// Host receive time, decimal nanoseconds past the Unix epoch.
    pub received_ns: String,
    /// Carrier.
    pub origin: JournalOrigin,
    /// Short name.
    pub name: String,
    /// One-line decoded summary.
    pub summary: String,
    /// Transport context.
    pub transport: JournalTransport,
    /// 90 kHz time of the carrying packet. SCTE-35 sections have no PES header, so
    /// the demuxer reports the last PCR, which trails video PTS by the mux delay.
    pub source_pts: Option<u64>,
    /// `base64` for binary payloads, `text` for HLS tag lines.
    pub raw_encoding: String,
    /// The raw payload.
    pub raw: String,
    /// The full decode.
    pub decoded: Option<serde_json::Value>,
    /// Derived times.
    pub timing: Option<JournalTiming>,
    /// Why the payload could not be fully decoded.
    pub error: Option<String>,
    /// Whether this is a clock stamp rather than an event.
    #[serde(default)]
    pub clock_reference: bool,
    /// Identical sightings after this record.
    #[serde(default)]
    pub repeats: u64,
    /// When the last identical sighting arrived.
    #[serde(default)]
    pub last_seen_at: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RepeatLine {
    kind: String,
    of: String,
    count: u64,
    last_seen_at: String,
}

/// Size and retention bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JournalLimits {
    /// Days of history kept.
    pub retention_days: u32,
    /// Total bytes kept across all sources.
    pub max_bytes: u64,
}
impl Default for JournalLimits {
    fn default() -> Self {
        Self {
            retention_days: DEFAULT_RETENTION_DAYS,
            max_bytes: DEFAULT_MAX_BYTES,
        }
    }
}

/// Monotonic counters shared by the producers, writer and API.
#[derive(Debug, Default)]
pub struct JournalCounters {
    /// Observations dropped because the bounded queue was full.
    pub dropped: AtomicU64,
    /// Records written.
    pub written: AtomicU64,
    /// Exact repeats folded into a counter.
    pub repeats: AtomicU64,
    /// Failed disk writes; those records are lost.
    pub write_errors: AtomicU64,
    /// Files removed by retention.
    pub files_removed: AtomicU64,
}

/// Counter snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct JournalStats {
    /// Observations dropped because the bounded queue was full.
    pub dropped: u64,
    /// Records written since start.
    pub written: u64,
    /// Exact repeats folded into a counter since start.
    pub repeats: u64,
    /// Failed disk writes since start.
    pub write_errors: u64,
}
impl JournalCounters {
    /// Read the counters.
    #[must_use]
    pub fn snapshot(&self) -> JournalStats {
        JournalStats {
            dropped: self.dropped.load(Ordering::Relaxed),
            written: self.written.load(Ordering::Relaxed),
            repeats: self.repeats.load(Ordering::Relaxed),
            write_errors: self.write_errors.load(Ordering::Relaxed),
        }
    }
}

/// RFC 3339 UTC text with nanoseconds.
#[must_use]
pub fn rfc3339_ns(ns: i64) -> String {
    chrono::DateTime::from_timestamp_nanos(ns).to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
}

fn day_of(ns: i64) -> String {
    chrono::DateTime::from_timestamp_nanos(ns)
        .format("%Y-%m-%d")
        .to_string()
}

/// Encode a source id into one safe path component. The id is caller-supplied
/// text, so nothing but unreserved bytes may reach the filesystem verbatim.
#[must_use]
pub fn source_dir_name(id: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(id.len());
    for (index, byte) in id.bytes().enumerate() {
        let safe = byte.is_ascii_alphanumeric()
            || matches!(byte, b'-' | b'_')
            || (byte == b'.' && index > 0);
        if safe {
            out.push(char::from(byte));
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

fn is_day_file(name: &str) -> bool {
    name.strip_suffix(".jsonl").is_some_and(|day| {
        day.len() == 10
            && day.bytes().enumerate().all(|(i, b)| {
                if i == 4 || i == 7 {
                    b == b'-'
                } else {
                    b.is_ascii_digit()
                }
            })
    })
}

/// Identity of one exact payload, for repeat detection.
pub type DedupeKey = [u8; 16];

/// Hash a payload together with what makes two sightings the same event.
#[must_use]
pub fn dedupe_key(source: &str, origin: JournalOrigin, position: u64, raw: &[u8]) -> DedupeKey {
    let mut hash = Sha256::new();
    hash.update(source.as_bytes());
    hash.update([
        0,
        match origin {
            JournalOrigin::Scte35 => 1,
            JournalOrigin::Id3 => 2,
            JournalOrigin::Hls => 3,
        },
    ]);
    hash.update(position.to_be_bytes());
    hash.update(raw);
    let digest = hash.finalize();
    let mut key = [0_u8; 16];
    key.copy_from_slice(digest.as_slice().get(..16).unwrap_or(&[0_u8; 16]));
    key
}

struct DedupeEntry {
    record_id: String,
    source: String,
    pending: u64,
    last_seen_ns: i64,
    last_flush_ns: i64,
}

/// Single-owner writer. Every method may block on disk, so it lives on the
/// journal thread and nowhere else.
pub struct JournalStore {
    root: PathBuf,
    limits: JournalLimits,
    open: HashMap<String, (String, File)>,
    dedupe: HashMap<DedupeKey, DedupeEntry>,
    last_id_ns: i64,
    sequence: u64,
    bytes_since_retention: u64,
    last_retention_ns: i64,
    counters: Arc<JournalCounters>,
}
impl JournalStore {
    /// Prepare a store rooted at `root`. Touches no disk until the first write.
    #[must_use]
    pub fn new(root: PathBuf, limits: JournalLimits, counters: Arc<JournalCounters>) -> Self {
        Self {
            root,
            limits,
            open: HashMap::new(),
            dedupe: HashMap::new(),
            last_id_ns: 0,
            sequence: 0,
            bytes_since_retention: 0,
            last_retention_ns: 0,
            counters,
        }
    }

    /// Count a sighting of `key`. True means it repeats a recent record and
    /// needs no new one.
    pub fn repeat(&mut self, key: &DedupeKey, now_ns: i64) -> bool {
        let Some(entry) = self.dedupe.get_mut(key) else {
            return false;
        };
        if now_ns.saturating_sub(entry.last_seen_ns) > DEDUPE_WINDOW_NS {
            return false;
        }
        entry.pending = entry.pending.saturating_add(1);
        entry.last_seen_ns = now_ns;
        self.counters.repeats.fetch_add(1, Ordering::Relaxed);
        true
    }

    /// Append `record`, assigning its identity, and remember `key`.
    ///
    /// # Errors
    /// The disk error; the record is lost and counted.
    pub fn append(
        &mut self,
        record: &mut JournalRecord,
        key: Option<DedupeKey>,
        now_ns: i64,
    ) -> io::Result<()> {
        self.last_id_ns = now_ns.max(self.last_id_ns.saturating_add(1));
        self.sequence = self.sequence.wrapping_add(1);
        record.record_id = format!("{:020}-{:06}", self.last_id_ns, self.sequence % 1_000_000);
        let line = serde_json::to_string(record).map_err(io::Error::other)?;
        let source = record.source_id.clone();
        self.write_line(&source, &line, now_ns)?;
        self.counters.written.fetch_add(1, Ordering::Relaxed);
        if let Some(key) = key {
            if self.dedupe.len() >= MAX_DEDUPE_ENTRIES && !self.dedupe.contains_key(&key) {
                self.evict_oldest(now_ns);
            }
            if let Some(old) = self.dedupe.remove(&key) {
                self.flush_entry(old, now_ns);
            }
            self.dedupe.insert(
                key,
                DedupeEntry {
                    record_id: record.record_id.clone(),
                    source,
                    pending: 0,
                    last_seen_ns: now_ns,
                    last_flush_ns: now_ns,
                },
            );
        }
        Ok(())
    }

    fn write_line(&mut self, source: &str, line: &str, now_ns: i64) -> io::Result<()> {
        let day = day_of(now_ns);
        let current = self.open.get(source).is_some_and(|(open, _)| *open == day);
        if !current {
            let dir = self.root.join(source_dir_name(source));
            fs::create_dir_all(&dir).inspect_err(|_| self.count_error())?;
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join(format!("{day}.jsonl")))
                .inspect_err(|_| self.count_error())?;
            self.open.insert(source.to_owned(), (day, file));
        }
        let mut bytes = Vec::with_capacity(line.len().saturating_add(1));
        bytes.extend_from_slice(line.as_bytes());
        bytes.push(b'\n');
        let written = self
            .open
            .get_mut(source)
            .map_or(Ok(()), |(_, file)| file.write_all(&bytes));
        written.inspect_err(|_| self.count_error())?;
        let size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        self.bytes_since_retention = self.bytes_since_retention.saturating_add(size);
        Ok(())
    }

    fn count_error(&self) {
        self.counters.write_errors.fetch_add(1, Ordering::Relaxed);
    }

    fn flush_entry(&mut self, entry: DedupeEntry, now_ns: i64) {
        if entry.pending == 0 {
            return;
        }
        let line = RepeatLine {
            kind: "repeat".to_owned(),
            of: entry.record_id,
            count: entry.pending,
            last_seen_at: rfc3339_ns(entry.last_seen_ns),
        };
        if let Ok(text) = serde_json::to_string(&line) {
            // Already counted; the record itself is safe if this line is lost.
            let _ = self.write_line(&entry.source, &text, now_ns);
        }
    }

    fn evict_oldest(&mut self, now_ns: i64) {
        let oldest = self
            .dedupe
            .iter()
            .min_by_key(|(_, entry)| entry.last_seen_ns)
            .map(|(key, _)| *key);
        if let Some(entry) = oldest.and_then(|key| self.dedupe.remove(&key)) {
            self.flush_entry(entry, now_ns);
        }
    }

    /// Persist repeat counters, expire the window and apply retention when due.
    pub fn maintain(&mut self, now_ns: i64) {
        let due: Vec<DedupeKey> = self
            .dedupe
            .iter()
            .filter(|(_, e)| {
                now_ns.saturating_sub(e.last_seen_ns) > DEDUPE_WINDOW_NS
                    || (e.pending > 0 && now_ns.saturating_sub(e.last_flush_ns) >= REPEAT_FLUSH_NS)
            })
            .map(|(key, _)| *key)
            .collect();
        for key in due {
            let Some(mut entry) = self.dedupe.remove(&key) else {
                continue;
            };
            let expired = now_ns.saturating_sub(entry.last_seen_ns) > DEDUPE_WINDOW_NS;
            let keep = DedupeEntry {
                record_id: entry.record_id.clone(),
                source: entry.source.clone(),
                pending: 0,
                last_seen_ns: entry.last_seen_ns,
                last_flush_ns: now_ns,
            };
            entry.last_flush_ns = now_ns;
            self.flush_entry(entry, now_ns);
            if !expired {
                self.dedupe.insert(key, keep);
            }
        }
        let quota = self.limits.max_bytes / 16;
        if now_ns.saturating_sub(self.last_retention_ns) >= RETENTION_INTERVAL_NS
            || self.bytes_since_retention >= quota
        {
            self.apply_retention(now_ns);
        }
    }

    /// Write out every pending repeat counter.
    pub fn flush(&mut self, now_ns: i64) {
        let keys: Vec<DedupeKey> = self.dedupe.keys().copied().collect();
        for key in keys {
            if let Some(entry) = self.dedupe.remove(&key) {
                self.flush_entry(entry, now_ns);
            }
        }
    }

    /// Delete expired days, then the oldest files until under the size cap.
    pub fn apply_retention(&mut self, now_ns: i64) {
        self.last_retention_ns = now_ns;
        self.bytes_since_retention = 0;
        let days = i64::from(self.limits.retention_days);
        let cutoff = day_of(now_ns.saturating_sub(days.saturating_mul(86_400 * NS_PER_SECOND)));
        let mut files = day_files_under(&self.root);
        files.sort();
        let mut total: u64 = files.iter().map(|(_, _, size)| *size).sum();
        for (day, path, size) in files {
            if day.as_str() >= cutoff.as_str() && total <= self.limits.max_bytes {
                continue;
            }
            if fs::remove_file(&path).is_ok() {
                total = total.saturating_sub(size);
                self.counters.files_removed.fetch_add(1, Ordering::Relaxed);
                // Reopen on the next write rather than append to an unlinked inode.
                self.open.retain(|_, (open, _)| *open != day);
            }
        }
    }
}

fn day_files_under(root: &Path) -> Vec<(String, PathBuf, u64)> {
    let mut out = Vec::new();
    let Ok(sources) = fs::read_dir(root) else {
        return out;
    };
    for source in sources.flatten() {
        out.extend(day_files_in(&source.path()));
    }
    out
}

fn day_files_in(dir: &Path) -> Vec<(String, PathBuf, u64)> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if is_day_file(&name) {
            let size = entry.metadata().map_or(0, |m| m.len());
            let day = name.trim_end_matches(".jsonl").to_owned();
            out.push((day, entry.path(), size));
        }
    }
    out
}

/// Filters of one read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JournalQuery {
    /// Earliest receive time, inclusive.
    pub from_ns: Option<i64>,
    /// Latest receive time, inclusive.
    pub to_ns: Option<i64>,
    /// Only this carrier.
    pub origin: Option<JournalOrigin>,
    /// Page size, clamped to [`MAX_PAGE`].
    pub limit: usize,
    /// Return records strictly older than this `record_id`.
    pub cursor: Option<String>,
}
impl JournalQuery {
    fn admits(&self, record: &JournalRecord) -> bool {
        let received = record.received_ns.parse::<i64>().unwrap_or(0);
        self.from_ns.is_none_or(|from| received >= from)
            && self.to_ns.is_none_or(|to| received <= to)
            && self.origin.is_none_or(|origin| origin == record.origin)
            && self
                .cursor
                .as_ref()
                .is_none_or(|cursor| record.record_id < *cursor)
    }
    fn admits_day(&self, day: &str) -> bool {
        let cursor_ns = self
            .cursor
            .as_ref()
            .and_then(|c| c.split('-').next())
            .and_then(|ns| ns.parse::<i64>().ok());
        self.from_ns.is_none_or(|from| day >= day_of(from).as_str())
            && self.to_ns.is_none_or(|to| day <= day_of(to).as_str())
            && cursor_ns.is_none_or(|ns| day <= day_of(ns).as_str())
    }
}

/// Whether `value` has the shape of a `record_id`.
#[must_use]
pub fn valid_cursor(value: &str) -> bool {
    value.split_once('-').is_some_and(|(ns, seq)| {
        ns.len() == 20
            && seq.len() == 6
            && ns.bytes().all(|b| b.is_ascii_digit())
            && seq.bytes().all(|b| b.is_ascii_digit())
    })
}

/// One newest-first page.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct JournalPage {
    /// Exact source identity.
    pub source_id: String,
    /// Records, newest first.
    pub records: Vec<JournalRecord>,
    /// Pass as `cursor` for the next older page; absent on the last page.
    pub next_cursor: Option<String>,
    /// Journal counters; `dropped` above zero means the history has gaps.
    pub stats: JournalStats,
}

enum Line {
    Record(Box<JournalRecord>),
    Repeat(RepeatLine),
}

fn parse_line(text: &str) -> Option<Line> {
    if let Ok(repeat) = serde_json::from_str::<RepeatLine>(text) {
        return Some(Line::Repeat(repeat));
    }
    // A torn tail after a crash is skipped, not fatal.
    serde_json::from_str::<JournalRecord>(text)
        .ok()
        .map(|record| Line::Record(Box::new(record)))
}

/// Request-plane reader. All methods block on disk: call off the async runtime.
#[derive(Clone)]
pub struct JournalReader {
    root: PathBuf,
    counters: Arc<JournalCounters>,
    /// Caps concurrent whole-file scans.
    pub permits: Arc<tokio::sync::Semaphore>,
}
impl JournalReader {
    /// Read the journal rooted at `root`.
    #[must_use]
    pub fn new(root: PathBuf, counters: Arc<JournalCounters>) -> Self {
        Self {
            root,
            counters,
            permits: Arc::new(tokio::sync::Semaphore::new(2)),
        }
    }

    /// Whether this source has any journal directory.
    #[must_use]
    pub fn has_source(&self, id: &str) -> bool {
        self.root.join(source_dir_name(id)).is_dir()
    }

    /// Counter snapshot.
    #[must_use]
    pub fn stats(&self) -> JournalStats {
        self.counters.snapshot()
    }

    fn files_newest_first(&self, id: &str, query: &JournalQuery) -> Vec<(String, PathBuf, u64)> {
        let mut files = day_files_in(&self.root.join(source_dir_name(id)));
        files.retain(|(day, _, _)| query.admits_day(day));
        files.sort();
        files.reverse();
        files
    }

    /// Read one page, newest first.
    ///
    /// # Errors
    /// A read error on a file that exists.
    pub fn page(&self, id: &str, query: &JournalQuery) -> io::Result<JournalPage> {
        let limit = query.limit.clamp(1, MAX_PAGE);
        let mut records: Vec<JournalRecord> = Vec::new();
        let mut repeats: HashMap<String, (u64, String)> = HashMap::new();
        for (_, path, _) in self.files_newest_first(id, query) {
            let wanted = limit.saturating_add(1).saturating_sub(records.len());
            let mut newest: VecDeque<JournalRecord> = VecDeque::with_capacity(wanted);
            for line in BufReader::new(File::open(&path)?).lines() {
                match parse_line(&line?) {
                    Some(Line::Record(record)) if query.admits(&record) => {
                        if newest.len() == wanted {
                            newest.pop_front();
                        }
                        newest.push_back(*record);
                    }
                    Some(Line::Repeat(line)) if repeats.len() < MAX_FOLDED_REPEATS => {
                        let entry = repeats.entry(line.of).or_default();
                        entry.0 = entry.0.saturating_add(line.count);
                        entry.1 = entry.1.clone().max(line.last_seen_at);
                    }
                    _ => {}
                }
            }
            records.extend(newest.into_iter().rev());
            if records.len() > limit {
                break;
            }
        }
        let next_cursor = (records.len() > limit).then(|| {
            records.truncate(limit);
            records.last().map(|r| r.record_id.clone())
        });
        for record in &mut records {
            if let Some((count, last)) = repeats.get(&record.record_id) {
                record.repeats = *count;
                record.last_seen_at = Some(last.clone());
            }
        }
        Ok(JournalPage {
            source_id: id.to_owned(),
            records,
            next_cursor: next_cursor.flatten(),
            stats: self.stats(),
        })
    }

    /// Feed every matching line, oldest first, to `sink` until it returns false.
    ///
    /// # Errors
    /// A read error on a file that exists.
    pub fn dump(
        &self,
        source: Option<&str>,
        query: &JournalQuery,
        sink: &mut dyn FnMut(String) -> bool,
    ) -> io::Result<()> {
        let mut files = if let Some(id) = source {
            self.files_newest_first(id, query)
        } else {
            let mut all = day_files_under(&self.root);
            all.retain(|(day, _, _)| query.admits_day(day));
            all
        };
        files.sort();
        for (_, path, _) in files {
            for line in BufReader::new(File::open(&path)?).lines() {
                let line = line?;
                let keep = match parse_line(&line) {
                    Some(Line::Record(record)) => query.admits(&record),
                    Some(Line::Repeat(_)) => true,
                    None => false,
                };
                if keep && !sink(line) {
                    return Ok(());
                }
            }
        }
        Ok(())
    }
}
