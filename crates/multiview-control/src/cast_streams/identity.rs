//! Per-component content identity and the push-rung attachment (ADR-0114
//! decision 5.4, "Unrequested changes"; design note section 11).
//!
//! * **Components.** Video, the program-audio slot, and each discrete
//!   audio track (by track name) an output carries.
//! * **Identity.** A 64-bit value per component that changes whenever
//!   anything its authorization closure depends on changes. It is a digest of
//!   a canonical *content* string ([`video_content`], [`audio_content`],
//!   [`track_content`]): the bus or source, a layout's set of cell sources and
//!   its audio attribute, the audio source, a track's route input. A cut on
//!   the switcher does not change `program`, as the closure treats the bus as
//!   one grant. The control plane computes the identity of every change it
//!   requests from the same functions (it "mints" it), so a cleared owner
//!   survives the change; a change the engine makes by itself (a fallback) is
//!   labelled with [`ENGINE_MINTED`] set, a range the control plane never
//!   computes, so nobody is cleared for it until a new admission authorized
//!   the owner against the state it stands for.
//! * **Change list.** Per output and component the engine keeps
//!   `(first tick, identity)` ([`ChangeList`], at most [`MAX_CHANGES`]),
//!   written on the bake consumer by the code that applies the change, and
//!   published lock-free ([`OutputLabels`]). A packet takes the identity of the
//!   last entry at or before the tick of its own timestamp; a tick older than
//!   the oldest kept entry has no identity.
//! * **Labelled through.** A change the engine learns of only after the
//!   window carrying it was offered (a dispatcher move outcome) is recorded in
//!   the same consumer iteration, before [`OutputLabels::labelled_through`]
//!   moves past that window. The gate judges a window only once every tick in
//!   it is below that mark, so no window is ever judged against a list that
//!   could still change for it.
//! * **The gate** ([`CastAttachment::admit_window`]): every component of
//!   every packet in the window must resolve to an identity in the
//!   attachment's clearance for that component. One miss, or no identity, and
//!   the whole window is refused and the attachment fenced. Bounded integer
//!   comparisons, no lock, no allocation on the pass path.
//!
//! Isolation (invariants #1 and #10): the engine side is one `SwapCell`
//! publish per change (rare) and one atomic store per consumer iteration;
//! readers never take a lock.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use multiview_framestore::SwapCell;
use sha2::{Digest, Sha256};

use crate::output_routing::{RoutedAudio, RoutedFeed, RoutedLayout};

/// Change-list entries kept per output and component.
pub const MAX_CHANGES: usize = 16;
/// Identities an attachment is cleared for per component: the current one
/// and the unsettled changes (the registry bounds those at
/// [`super::MAX_CARRY`]).
pub const MAX_CLEARED: usize = super::MAX_CARRY + 1;
/// Components an attachment's clearance covers at most (video, the program
/// audio slot and discrete tracks).
pub const MAX_COMPONENTS: usize = 18;
/// The range bit of an identity the engine minted for a change nobody
/// requested. The control plane never computes such a value.
pub const ENGINE_MINTED: u64 = 1 << 63;

/// One media component of an output.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Component {
    /// The picture.
    Video,
    /// The program-audio slot (the mix, a source, a track, silence).
    Audio,
    /// One discrete audio track, by its name.
    Track(String),
}

impl Component {
    fn tag(&self) -> String {
        match self {
            Self::Video => "video".to_owned(),
            Self::Audio => "audio".to_owned(),
            Self::Track(name) => format!("track:{name}"),
        }
    }
}

/// The identity of `content` on `component` (control-plane range: the top
/// bit is clear).
#[must_use]
pub fn identity(component: &Component, content: &str) -> u64 {
    let mut hash = Sha256::new();
    hash.update(b"conspect-cast-identity-v1\0");
    hash.update(component.tag().as_bytes());
    hash.update([0]);
    hash.update(content.as_bytes());
    let digest = hash.finalize();
    let mut bytes = [0_u8; 8];
    for (slot, byte) in bytes.iter_mut().zip(digest.iter()) {
        *slot = *byte;
    }
    u64::from_be_bytes(bytes) & !ENGINE_MINTED
}

/// `id` moved to the engine's range (a change nobody requested).
#[must_use]
pub const fn engine_minted(id: u64) -> u64 {
    id | ENGINE_MINTED
}

/// The canonical content of a picture on bus `bus` (`program`, `preview`,
/// `clean`, `src:<id>`, `aux:<layout>`), with the resolved body of a layout
/// bus: its id, the sorted set of sources its cells bind and its audio
/// attribute — the inputs of the layout's closure, independent of cell
/// order and geometry.
#[must_use]
pub fn video_content(bus: &str, layout: Option<&RoutedLayout>) -> String {
    let Some(layout) = layout else {
        return bus.to_owned();
    };
    let mut sources: Vec<&str> = layout
        .layout
        .cells
        .iter()
        .filter_map(|cell| cell.source.as_deref())
        .collect();
    sources.sort_unstable();
    sources.dedup();
    format!(
        "aux:{}|{}|audio={}",
        layout.id,
        sources.join(","),
        layout_audio_label(&layout.audio)
    )
}

/// The same content from a layout's id, cell sources and audio attribute
/// (the control plane's stored library view).
#[must_use]
pub fn layout_content(
    id: &str,
    sources: &[String],
    audio: &multiview_config::LayoutAudio,
) -> String {
    let mut sources: Vec<&str> = sources.iter().map(String::as_str).collect();
    sources.sort_unstable();
    sources.dedup();
    format!(
        "aux:{id}|{}|audio={}",
        sources.join(","),
        layout_audio_label(audio)
    )
}

fn layout_audio_label(audio: &multiview_config::LayoutAudio) -> String {
    match audio {
        multiview_config::LayoutAudio::None => "none".to_owned(),
        multiview_config::LayoutAudio::Source(id) => format!("src:{id}"),
        _ => "program".to_owned(),
    }
}

/// The canonical content of the program-audio slot carrying `label` (the
/// engine's audio-source label: `program`, `preview`, `none`, `src:<id>`),
/// or the discrete track `track` fed from `input`.
#[must_use]
pub fn audio_content(label: &str, track: Option<(&str, &str)>) -> String {
    match track {
        Some((name, input)) => format!("track:{name}>{input}"),
        None => label.to_owned(),
    }
}

/// The canonical content of a discrete track fed from route input `input`.
#[must_use]
pub fn track_content(input: &str) -> String {
    format!("input:{input}")
}

/// What the program-audio slot carries for `choice` on `video` (the
/// engine's own resolution, `routing_audio::AudioChoice::resolve`).
#[must_use]
pub fn audio_slot_content(
    choice: &RoutedAudio,
    video: &RoutedFeed,
    layout: Option<&multiview_config::LayoutAudio>,
) -> String {
    match choice {
        RoutedAudio::Follow => match video {
            RoutedFeed::Program | RoutedFeed::Clean => "program".to_owned(),
            RoutedFeed::Preview => "preview".to_owned(),
            RoutedFeed::Source(id) => format!("src:{id}"),
            RoutedFeed::Layout(_) => match layout {
                Some(multiview_config::LayoutAudio::None) => "none".to_owned(),
                Some(multiview_config::LayoutAudio::Source(id)) => format!("src:{id}"),
                _ => "program".to_owned(),
            },
        },
        RoutedAudio::Program => "program".to_owned(),
        RoutedAudio::None => "none".to_owned(),
        RoutedAudio::Source(id) => format!("src:{id}"),
        RoutedAudio::Track { name, input } => audio_content("", Some((name, input))),
    }
}

/// One output component's change list: `(first tick, identity)`, ascending
/// by tick, at most [`MAX_CHANGES`] (the oldest forgotten first; a tick
/// before the oldest kept entry then resolves to nothing).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChangeList {
    entries: Vec<(u64, u64)>,
    /// An entry was forgotten: ticks before the oldest kept one are unknown.
    truncated: bool,
}

impl ChangeList {
    /// The identity of a packet at `tick`.
    #[must_use]
    pub fn resolve(&self, tick: u64) -> Option<u64> {
        let at = self.entries.partition_point(|(first, _)| *first <= tick);
        let (_, id) = self.entries.get(at.checked_sub(1)?)?;
        Some(*id)
    }

    /// The latest identity.
    #[must_use]
    pub fn current(&self) -> Option<u64> {
        self.entries.last().map(|(_, id)| *id)
    }

    /// The entries, oldest first.
    #[must_use]
    pub fn entries(&self) -> &[(u64, u64)] {
        &self.entries
    }

    /// This list with `(first_tick, id)` recorded. An entry at or after
    /// `first_tick` is replaced (a later decision about the same ticks wins:
    /// the list never says two things about one tick). Unchanged when the
    /// identity at `first_tick` already is `id` and nothing follows it.
    #[must_use]
    pub fn with(&self, first_tick: u64, id: u64) -> Self {
        let mut next = self.clone();
        next.entries.retain(|(first, _)| *first < first_tick);
        if next.entries.last().is_some_and(|(_, last)| *last == id) {
            return next;
        }
        next.entries.push((first_tick, id));
        while next.entries.len() > MAX_CHANGES {
            next.entries.remove(0);
            next.truncated = true;
        }
        next
    }
}

/// The published change lists of one output for one engine run.
///
/// One writer (the bake consumer); any number of lock-free readers (the
/// control plane, the push rungs' gate threads).
#[derive(Debug)]
pub struct OutputLabels {
    lists: SwapCell<Arc<BTreeMap<Component, ChangeList>>>,
    labelled_through: AtomicU64,
    /// Identities the control plane minted for the changes it requested,
    /// each with a sequence number (newest last, at most [`MAX_CHANGES`]).
    /// The control plane is the ONLY writer (serialized by `expecting`);
    /// the bake consumer only loads it and keeps what it consumed in its own
    /// [`Consumed`] (PR 516 review round 2: no read-modify-write from two
    /// threads, so no registration is lost and none is consumed twice).
    requested: SwapCell<Arc<Vec<(u64, u64)>>>,
    /// Serializes the control-plane writers of `requested` (never taken by
    /// the bake consumer).
    expecting: Mutex<u64>,
}

/// The requests one labeller already used (owned by the bake consumer; see
/// [`OutputLabels::label`]). Bounded by the request log it is pruned to.
#[derive(Debug, Default, Clone)]
pub struct Consumed(Vec<u64>);

impl Default for OutputLabels {
    fn default() -> Self {
        Self {
            lists: SwapCell::from_pointee(BTreeMap::new()),
            labelled_through: AtomicU64::new(0),
            requested: SwapCell::from_pointee(Vec::new()),
            expecting: Mutex::new(0),
        }
    }
}

impl OutputLabels {
    /// An empty set of lists (nothing resolves yet).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The control plane requests a change carrying `ids` (before the
    /// engine hears of it). Control-plane writers only; serialized.
    pub fn expect(&self, ids: &[u64]) {
        let mut seq = self
            .expecting
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let mut all = self.requested.load().as_ref().clone();
        for id in ids {
            *seq = seq.saturating_add(1);
            all.push((*seq, *id));
        }
        while all.len() > MAX_CHANGES {
            all.remove(0);
        }
        self.requested.store(Arc::new(all));
    }

    /// The label of a change to content identity `id`: `id` when the control
    /// plane requested it and `consumed` has not used that request yet (it
    /// is then recorded there: one request labels one change), else `id` in
    /// the engine's range. Reads the request log only; `consumed` belongs to
    /// the one labelling thread and is pruned to the log, so it is bounded.
    #[must_use]
    pub fn label(&self, id: u64, consumed: &mut Consumed) -> u64 {
        let requested = self.requested.load();
        consumed
            .0
            .retain(|seq| requested.iter().any(|(s, _)| s == seq));
        let found = requested
            .iter()
            .find(|(seq, known)| *known == id && !consumed.0.contains(seq));
        match found {
            Some((seq, _)) => {
                consumed.0.push(*seq);
                id
            }
            None => engine_minted(id),
        }
    }

    /// Record `(first_tick, id)` on `component` (the bake consumer only).
    pub fn record(&self, component: &Component, first_tick: u64, id: u64) {
        let lists = self.lists.load();
        let current = lists.get(component).cloned().unwrap_or_default();
        let next = current.with(first_tick, id);
        if next == current {
            return;
        }
        let mut all = lists.as_ref().clone();
        all.insert(component.clone(), next);
        self.lists.store(Arc::new(all));
    }

    /// The identity of `component` at `tick`.
    #[must_use]
    pub fn resolve(&self, component: &Component, tick: u64) -> Option<u64> {
        self.lists.load().get(component)?.resolve(tick)
    }

    /// The latest identity of every component.
    #[must_use]
    pub fn current(&self) -> Vec<(Component, u64)> {
        self.lists
            .load()
            .iter()
            .filter_map(|(c, list)| list.current().map(|id| (c.clone(), id)))
            .collect()
    }

    /// The whole lists (tests, diagnostics).
    #[must_use]
    pub fn lists(&self) -> Arc<BTreeMap<Component, ChangeList>> {
        self.lists.load()
    }

    /// Every window whose ticks are all below `tick` was offered and every
    /// change it carries is recorded (the bake consumer, once per
    /// iteration; monotonic).
    pub fn set_labelled_through(&self, tick: u64) {
        self.labelled_through.fetch_max(tick, Ordering::AcqRel);
    }

    /// See [`Self::set_labelled_through`].
    #[must_use]
    pub fn labelled_through(&self) -> u64 {
        self.labelled_through.load(Ordering::Acquire)
    }
}

/// What an attachment is cleared for: per component, at most
/// [`MAX_CLEARED`] identities. Immutable; replaced whole.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Clearance {
    by_component: BTreeMap<Component, Vec<u64>>,
}

impl Clearance {
    /// Cleared for exactly `ids`.
    #[must_use]
    pub fn of(ids: &[(Component, u64)]) -> Self {
        let mut clearance = Self::default();
        for (component, id) in ids {
            clearance = clearance.with(component, *id);
        }
        clearance
    }

    /// Whether `id` is cleared on `component`.
    #[must_use]
    pub fn clears(&self, component: &Component, id: u64) -> bool {
        self.by_component
            .get(component)
            .is_some_and(|ids| ids.contains(&id))
    }

    /// This clearance plus `id` on `component` (the oldest forgotten past
    /// [`MAX_CLEARED`], which only narrows; a component past
    /// [`MAX_COMPONENTS`] is not added, which only narrows too).
    #[must_use]
    pub fn with(&self, component: &Component, id: u64) -> Self {
        let mut next = self.clone();
        if !next.by_component.contains_key(component) && next.by_component.len() >= MAX_COMPONENTS {
            return next;
        }
        let ids = next.by_component.entry(component.clone()).or_default();
        if !ids.contains(&id) {
            ids.push(id);
            while ids.len() > MAX_CLEARED {
                ids.remove(0);
            }
        }
        next
    }

    /// Identities cleared on `component` (tests).
    #[must_use]
    pub fn ids(&self, component: &Component) -> &[u64] {
        self.by_component.get(component).map_or(&[], Vec::as_slice)
    }
}

/// Why a window did not pass the gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateRefusal {
    /// The attachment is fenced: nothing more is offered to it.
    Fenced,
    /// A tick of the window is not labelled yet: hold the window and ask
    /// again (never offered meanwhile).
    NotYet,
}

/// The closer an attachment's transport registers: bounded and
/// non-blocking (it stops enqueue and wakes the transport's own thread,
/// which closes the connection abruptly).
pub type Closer = Box<dyn FnOnce() + Send>;

/// One push attachment (a `MoQ` lease, a `WebRTC` session) of one cast stream.
pub struct CastAttachment {
    /// The stream it belongs to.
    pub stream_id: String,
    /// The output's config id.
    pub config_id: String,
    /// `moq`, `webrtc` or `srt`.
    pub rung: &'static str,
    fenced: AtomicBool,
    clearance: SwapCell<Arc<Clearance>>,
    labels: Arc<OutputLabels>,
    closer: Mutex<Option<Closer>>,
    passed: AtomicU64,
    refused: AtomicU64,
    /// The highest window sequence number this attachment's gate passed
    /// (one shared feed, several viewers: the converter judges each window
    /// for every attachment and the writer hands a sample to a viewer only
    /// when its window is at or below this mark). 0: none yet.
    passed_through: AtomicU64,
    /// The first window sequence number this attachment's gate passed (0:
    /// none yet). Windows before it were never judged for this attachment
    /// (it joined later) and are never written to it (PR 521 review).
    passed_from: AtomicU64,
}

impl std::fmt::Debug for CastAttachment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CastAttachment")
            .field("stream_id", &self.stream_id)
            .field("rung", &self.rung)
            .field("fenced", &self.is_fenced())
            .finish_non_exhaustive()
    }
}

impl CastAttachment {
    /// A new attachment cleared for `clearance`, reading `labels`.
    #[must_use]
    pub fn new(
        stream_id: &str,
        config_id: &str,
        rung: &'static str,
        labels: Arc<OutputLabels>,
        clearance: Clearance,
    ) -> Self {
        Self {
            stream_id: stream_id.to_owned(),
            config_id: config_id.to_owned(),
            rung,
            fenced: AtomicBool::new(false),
            clearance: SwapCell::from_pointee(clearance),
            labels,
            closer: Mutex::new(None),
            passed: AtomicU64::new(0),
            refused: AtomicU64::new(0),
            passed_through: AtomicU64::new(0),
            passed_from: AtomicU64::new(0),
        }
    }

    /// Register the transport's closer. Already fenced: it runs at once.
    pub fn set_closer(&self, closer: Closer) {
        let mut slot = self.closer.lock().unwrap_or_else(PoisonError::into_inner);
        if self.is_fenced() {
            drop(slot);
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(closer));
            return;
        }
        *slot = Some(closer);
    }

    /// Fence: stop every offer at once (one atomic store), then run the
    /// transport's closer once. Idempotent.
    pub fn fence(&self) {
        if self.fenced.swap(true, Ordering::AcqRel) {
            return;
        }
        let closer = self
            .closer
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(closer) = closer {
            // The closer is the transport's; a panicking one must not unwind
            // through the registry lock it may run under (it would poison
            // it for every other token). The attachment is fenced already.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(closer));
        }
    }

    /// Whether the attachment is fenced.
    #[must_use]
    pub fn is_fenced(&self) -> bool {
        self.fenced.load(Ordering::Acquire)
    }

    /// The current clearance.
    #[must_use]
    pub fn clearance(&self) -> Arc<Clearance> {
        self.clearance.load()
    }

    /// Replace the clearance whole (the registry, under its lock).
    pub fn set_clearance(&self, clearance: Clearance) {
        self.clearance.store(Arc::new(clearance));
    }

    /// The labels it reads.
    #[must_use]
    pub fn labels(&self) -> &Arc<OutputLabels> {
        &self.labels
    }

    /// Record that the window with sequence number `seq` (from 1) passed
    /// this attachment's gate (monotonic).
    pub fn mark_passed(&self, seq: u64) {
        let _ = self
            .passed_from
            .compare_exchange(0, seq, Ordering::AcqRel, Ordering::Acquire);
        self.passed_through.fetch_max(seq, Ordering::AcqRel);
    }

    /// Whether a sample of window `seq` may be written to this attachment's
    /// transport: not fenced and its window passed the gate.
    #[must_use]
    pub fn may_write(&self, seq: u64) -> bool {
        // Every window from the first this attachment passed to the last was
        // judged by its gate in order (a refusal fences it, a hold is never
        // skipped), so the passed windows are exactly this interval.
        let from = self.passed_from.load(Ordering::Acquire);
        !self.is_fenced()
            && from != 0
            && seq >= from
            && seq <= self.passed_through.load(Ordering::Acquire)
    }

    /// `(windows passed, windows refused)`.
    #[must_use]
    pub fn counters(&self) -> (u64, u64) {
        (
            self.passed.load(Ordering::Acquire),
            self.refused.load(Ordering::Acquire),
        )
    }

    /// THE gate (decision 5.4): judge one whole window given each packet's
    /// `(component, tick)`. Every packet must resolve to an identity in the
    /// clearance for its component, else the window is refused and the
    /// attachment fenced. A tick not yet labelled holds the window
    /// ([`GateRefusal::NotYet`]); nothing is decided for it.
    ///
    /// # Errors
    /// [`GateRefusal`].
    pub fn admit_window<'a>(
        &self,
        packets: impl IntoIterator<Item = (&'a Component, u64)> + Clone,
    ) -> Result<(), GateRefusal> {
        if self.is_fenced() {
            return Err(GateRefusal::Fenced);
        }
        let through = self.labels.labelled_through();
        if packets.clone().into_iter().any(|(_, tick)| tick >= through) {
            return Err(GateRefusal::NotYet);
        }
        let lists = self.labels.lists();
        let clearance = self.clearance.load();
        let pass = packets.into_iter().all(|(component, tick)| {
            lists
                .get(component)
                .and_then(|list| list.resolve(tick))
                .is_some_and(|id| clearance.clears(component, id))
        });
        // Re-check after the comparison: a fence that raced it wins.
        if pass && !self.is_fenced() {
            self.passed.fetch_add(1, Ordering::AcqRel);
            return Ok(());
        }
        self.refused.fetch_add(1, Ordering::AcqRel);
        self.fence();
        Err(GateRefusal::Fenced)
    }
}

impl Drop for CastAttachment {
    fn drop(&mut self) {
        self.fence();
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn labels(entries: &[(Component, u64, u64)], through: u64) -> Arc<OutputLabels> {
        let labels = Arc::new(OutputLabels::new());
        for (c, tick, id) in entries {
            labels.record(c, *tick, *id);
        }
        labels.set_labelled_through(through);
        labels
    }

    #[test]
    fn a_packet_takes_the_last_entry_at_or_before_its_tick() {
        let list = ChangeList::default().with(10, 1).with(20, 2);
        assert_eq!(list.resolve(9), None);
        assert_eq!(list.resolve(10), Some(1));
        assert_eq!(list.resolve(19), Some(1));
        assert_eq!(list.resolve(20), Some(2));
        assert_eq!(list.current(), Some(2));
        // A later decision about the same ticks replaces, never adds a second.
        let again = list.with(15, 3);
        assert_eq!(again.entries(), &[(10, 1), (15, 3)]);
    }

    #[test]
    fn the_change_list_is_bounded_and_forgotten_ticks_have_no_identity() {
        let mut list = ChangeList::default();
        for i in 0..40_u64 {
            list = list.with(i * 10, i + 100);
        }
        assert_eq!(list.entries().len(), MAX_CHANGES);
        assert_eq!(list.resolve(0), None, "older than the oldest kept entry");
        assert_eq!(list.resolve(395), Some(139));
    }

    #[test]
    fn identities_differ_by_component_content_and_range() {
        let v = identity(&Component::Video, "program");
        assert_eq!(v, identity(&Component::Video, "program"));
        assert_ne!(v, identity(&Component::Audio, "program"));
        assert_ne!(v, identity(&Component::Video, "src:cam1"));
        assert_eq!(v & ENGINE_MINTED, 0);
        assert_ne!(engine_minted(v), v);
        assert_eq!(engine_minted(v) & ENGINE_MINTED, ENGINE_MINTED);
    }

    #[test]
    fn a_layout_identity_follows_its_closure_not_its_geometry() {
        let a = layout_content(
            "l",
            &["b".into(), "a".into()],
            &multiview_config::LayoutAudio::Program,
        );
        let b = layout_content(
            "l",
            &["a".into(), "b".into(), "a".into()],
            &multiview_config::LayoutAudio::Program,
        );
        assert_eq!(a, b);
        let c = layout_content(
            "l",
            &["a".into(), "b".into(), "secret".into()],
            &multiview_config::LayoutAudio::Program,
        );
        assert_ne!(a, c);
        let d = layout_content(
            "l",
            &["a".into(), "b".into()],
            &multiview_config::LayoutAudio::Source("a".into()),
        );
        assert_ne!(a, d);
    }

    #[test]
    fn clearance_is_bounded_per_component() {
        let mut c = Clearance::default();
        for i in 0..20 {
            c = c.with(&Component::Video, i);
        }
        assert_eq!(c.ids(&Component::Video).len(), MAX_CLEARED);
        assert!(!c.clears(&Component::Video, 0));
        assert!(c.clears(&Component::Video, 19));
        let mut c = Clearance::default();
        for i in 0..40 {
            c = c.with(&Component::Track(format!("t{i}")), 1);
        }
        assert!(c.by_component.len() <= MAX_COMPONENTS);
    }

    #[test]
    fn the_gate_passes_cleared_windows_and_fences_on_one_miss() {
        let v = Component::Video;
        let a = Component::Audio;
        let l = labels(
            &[(v.clone(), 0, 1), (a.clone(), 0, 7), (v.clone(), 50, 2)],
            100,
        );
        let att = CastAttachment::new(
            "s",
            "o",
            "moq",
            l,
            Clearance::of(&[(v.clone(), 1), (a.clone(), 7)]),
        );
        let closed = Arc::new(AtomicU64::new(0));
        let seen = closed.clone();
        att.set_closer(Box::new(move || {
            seen.fetch_add(1, Ordering::AcqRel);
        }));
        assert_eq!(att.admit_window([(&v, 10), (&a, 10)]), Ok(()));
        // Not labelled yet: held, nothing decided.
        assert_eq!(att.admit_window([(&v, 100)]), Err(GateRefusal::NotYet));
        assert!(!att.is_fenced());
        // Video of the new feed (identity 2, not cleared): the whole window,
        // its cleared audio with it, is refused and the attachment fenced.
        assert_eq!(
            att.admit_window([(&a, 60), (&v, 60)]),
            Err(GateRefusal::Fenced)
        );
        assert!(att.is_fenced());
        assert_eq!(closed.load(Ordering::Acquire), 1);
        // Nothing later is offered, even a cleared window.
        assert_eq!(att.admit_window([(&v, 10)]), Err(GateRefusal::Fenced));
        att.fence();
        assert_eq!(closed.load(Ordering::Acquire), 1, "the closer runs once");
        assert_eq!(att.counters(), (1, 1));
    }

    #[test]
    fn a_panicking_closer_still_fences_and_never_unwinds() {
        let l = labels(&[], 0);
        let att = CastAttachment::new("s", "o", "moq", l, Clearance::default());
        att.set_closer(Box::new(|| panic!("a broken transport")));
        att.fence();
        assert!(att.is_fenced());
        assert_eq!(
            att.admit_window([(&Component::Video, 0)]),
            Err(GateRefusal::Fenced)
        );
    }

    #[test]
    fn may_write_follows_the_passed_mark_and_the_fence() {
        let l = labels(&[], 0);
        let att = CastAttachment::new("s", "o", "webrtc", l, Clearance::default());
        assert!(!att.may_write(1));
        att.mark_passed(3);
        assert!(
            !att.may_write(1),
            "joined at window 3: older windows were never judged for it"
        );
        assert!(att.may_write(3) && !att.may_write(4));
        att.mark_passed(5);
        assert!(att.may_write(4) && att.may_write(5) && !att.may_write(2));
        att.fence();
        assert!(!att.may_write(1));
    }

    #[test]
    fn a_requested_identity_labels_one_change() {
        let labels = OutputLabels::new();
        let mut used = Consumed::default();
        labels.expect(&[7]);
        assert_eq!(labels.label(7, &mut used), 7);
        assert_eq!(
            labels.label(7, &mut used),
            engine_minted(7),
            "a second change was not requested"
        );
        assert_eq!(labels.label(8, &mut used), engine_minted(8));
        labels.expect(&[7]);
        assert_eq!(
            labels.label(7, &mut used),
            7,
            "a new request labels a new change"
        );
    }

    /// PR 516 review round 2: registrations and consumption from two threads
    /// never lose a registration nor resurrect a consumed one. The labeller
    /// never writes the shared log, so in every interleaving a registration
    /// is either seen (labelled once) or not yet (and stays for later).
    #[test]
    fn concurrent_registration_and_consumption_lose_nothing() {
        let n = u64::try_from(MAX_CHANGES).unwrap();
        let labels = Arc::new(OutputLabels::new());
        let writer = {
            let labels = Arc::clone(&labels);
            std::thread::spawn(move || {
                for id in 1..=n {
                    labels.expect(&[id]);
                    std::thread::yield_now();
                }
            })
        };
        let mut used = Consumed::default();
        let mut labelled = Vec::new();
        for _ in 0..100_000 {
            for id in 1..=n {
                if !labelled.contains(&id) && labels.label(id, &mut used) == id {
                    labelled.push(id);
                }
            }
            if labelled.len() == MAX_CHANGES {
                break;
            }
            std::thread::yield_now();
        }
        writer.join().unwrap();
        for id in 1..=n {
            if !labelled.contains(&id) && labels.label(id, &mut used) == id {
                labelled.push(id);
            }
        }
        labelled.sort_unstable();
        assert_eq!(labelled, (1..=n).collect::<Vec<_>>(), "each exactly once");
        for id in 1..=n {
            assert_eq!(
                labels.label(id, &mut used),
                engine_minted(id),
                "never twice"
            );
        }
    }

    #[test]
    fn a_component_without_identity_is_refused() {
        let v = Component::Video;
        let t = Component::Track("commentary".into());
        let l = labels(&[(v.clone(), 0, 1)], 10);
        let att = CastAttachment::new("s", "o", "moq", l, Clearance::of(&[(v.clone(), 1)]));
        assert_eq!(
            att.admit_window([(&v, 1), (&t, 1)]),
            Err(GateRefusal::Fenced)
        );
    }

    #[test]
    fn a_closer_registered_after_the_fence_runs_at_once() {
        let l = labels(&[], 0);
        let att = CastAttachment::new("s", "o", "moq", l, Clearance::default());
        att.fence();
        let ran = Arc::new(AtomicBool::new(false));
        let seen = ran.clone();
        att.set_closer(Box::new(move || seen.store(true, Ordering::Release)));
        assert!(ran.load(Ordering::Acquire));
    }

    #[test]
    fn audio_slot_content_mirrors_the_engine_resolution() {
        use multiview_config::LayoutAudio;
        let follow = RoutedAudio::Follow;
        assert_eq!(
            audio_slot_content(&follow, &RoutedFeed::Program, None),
            "program"
        );
        assert_eq!(
            audio_slot_content(&follow, &RoutedFeed::Clean, None),
            "program"
        );
        assert_eq!(
            audio_slot_content(&follow, &RoutedFeed::Preview, None),
            "preview"
        );
        assert_eq!(
            audio_slot_content(&follow, &RoutedFeed::Source("c".into()), None),
            "src:c"
        );
        assert_eq!(
            audio_slot_content(
                &follow,
                &RoutedFeed::Layout("l".into()),
                Some(&LayoutAudio::Source("x".into()))
            ),
            "src:x"
        );
        assert_eq!(
            audio_slot_content(
                &RoutedAudio::Track {
                    name: "n".into(),
                    input: "i".into()
                },
                &RoutedFeed::Program,
                None
            ),
            audio_content("track:n", Some(("n", "i")))
        );
    }
}
