//! The discriminated event payload — one internally-tagged union for every
//! frame in both directions.
//!
//! Per ADR-RT002 the payload is a Rust internally-tagged enum
//! (`#[serde(tag = "t", content = "data")]`) that renders as a JSON-Schema
//! `oneOf` with a `const` discriminator and maps to a perfect TypeScript
//! discriminated `switch`. Control frames (`$`-prefixed `t`) and data events
//! (dotted `t`) share the one union so a single parse/route path serves WS and
//! SSE.
//!
//! This is **not** `#[serde(untagged)]` (guardrails + conventions §5 forbid it);
//! the explicit `t` tag gives one unambiguous parse path.
use multiview_core::alarm::AlarmRecord;
use multiview_core::stream::StreamInventory;
use multiview_core::tally::TallyState;
use multiview_core::time::MediaTime;
use multiview_core::traits::SourceState;
use multiview_core::wallclock::WallClockRef;
use serde::{Deserialize, Serialize};

use crate::subscription::{
    Hello, Lag, ProtocolError, Resume, Resync, SetRate, Subscribe, Subscribed, Unsubscribe,
};

/// The wire form of a tile/source lifecycle state (invariant #2:
/// `Live -> Stale -> Reconnecting -> NoSignal`).
///
/// This is the serde-serializable mirror of [`multiview_core::traits::SourceState`]
/// (which is `#[non_exhaustive]` and intentionally carries no serde impl). It
/// owns the wire contract for tile/input state — uppercase strings as in the
/// realtime-api brief — and converts losslessly from the core enum via
/// [`From<SourceState>`](LifecycleState::from). Any future `SourceState`
/// variant the conversion does not know maps to [`LifecycleState::NoSignal`]
/// (the safe "render a placeholder" default) rather than panicking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[non_exhaustive]
pub enum LifecycleState {
    /// Delivering fresh frames.
    Live,
    /// Holding the last-good frame; no fresh frame recently.
    Stale,
    /// The supervisor is re-establishing the connection.
    Reconnecting,
    /// No usable signal; a placeholder is rendered.
    NoSignal,
}

impl From<SourceState> for LifecycleState {
    fn from(state: SourceState) -> Self {
        match state {
            SourceState::Live => Self::Live,
            SourceState::Stale => Self::Stale,
            SourceState::Reconnecting => Self::Reconnecting,
            // `SourceState` is `#[non_exhaustive]`; an unknown future variant
            // maps to the safe placeholder state.
            _ => Self::NoSignal,
        }
    }
}

/// The trigger that drove a tile state transition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TileState {
    /// The state the tile transitioned from.
    pub from: LifecycleState,
    /// The state the tile transitioned to.
    pub to: LifecycleState,
    /// The input bound to the tile, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<String>,
    /// A short machine-readable trigger label (e.g. `nosignal_timeout`).
    pub trigger: String,
}

/// One tile's current lifecycle state as carried in the `tiles` `$snapshot`
/// baseline (realtime-api §5).
///
/// In today's run projection the tile `id` is the bound source id (the same
/// key the sparse `tile.state` deltas scope their envelope `id` with), so a
/// client's snapshot-rebuilt cache and its delta patches address the same rows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TileSnapshotEntry {
    /// The tile id (the bound source id in the run projection).
    pub id: String,
    /// The tile's current lifecycle state.
    pub state: LifecycleState,
    /// The input bound to the tile, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<String>,
}

/// The full current per-tile lifecycle baseline (the `data` body of the
/// `tiles`-topic `$snapshot` frame, realtime-api §5).
///
/// Sent once at connect (after `$hello`) so a fresh client REBUILDS its tile
/// cache to the current truth instead of waiting for the next sparse
/// `tile.state` delta (`snapshot ⊕ ordered deltas = current truth`,
/// ADR-RT003). [`as_of_seq`](TilesSnapshot::as_of_seq) is the engine state
/// sequence the baseline is current as of.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TilesSnapshot {
    /// The engine state sequence this baseline is current as of.
    pub as_of_seq: u64,
    /// Every tile's current lifecycle state.
    pub tiles: Vec<TileSnapshotEntry>,
}

/// A high-rate audio meter sample (numeric only — never audio).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AudioMeter {
    /// The track index this sample is for.
    pub track: u32,
    /// Per-channel peak level (dBFS).
    pub peak_db: Vec<f32>,
    /// Per-channel RMS level (dBFS).
    pub rms_db: Vec<f32>,
    /// Whether any channel clipped in this window.
    pub clip: bool,
    /// Whether the meter pipeline overflowed (dropped windows).
    pub overflow: bool,
    /// The effective sampling cadence on the wire (Hz).
    pub sampled_hz: u32,
}

/// The hardware vendor of a GPU/accelerator (for telemetry display + which
/// per-engine signals to expect). `#[non_exhaustive]` so a future vendor does
/// not break the wire enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum GpuVendor {
    /// NVIDIA — exposes compute, encoder/decoder util, and NVENC session counts.
    Nvidia,
    /// Intel — exposes compute + encoder/decoder util.
    Intel,
    /// AMD — exposes compute + a combined media-engine util.
    Amd,
    /// Apple — exposes compute; no per-engine util.
    Apple,
    /// A vendor the control plane did not classify.
    Other,
}

/// A high-rate per-GPU utilisation sample (numeric only). Optional fields are
/// absent where the vendor does not expose that signal (see [`GpuVendor`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GpuMetrics {
    /// Stable device identity (UUID where available, else an index string).
    pub id: String,
    /// The hardware vendor.
    pub vendor: GpuVendor,
    /// Human-readable device name, if known (e.g. `NVIDIA GeForce RTX 4060`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Compute (graphics/CUDA) utilisation, 0.0–1.0.
    pub compute_util: f32,
    /// VRAM in use (bytes).
    pub mem_used_bytes: u64,
    /// Total VRAM (bytes).
    pub mem_total_bytes: u64,
    /// Encoder (NVENC/QSV) ASIC utilisation, 0.0–1.0, where the vendor exposes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoder_util: Option<f32>,
    /// Decoder (NVDEC/QSV) ASIC utilisation, 0.0–1.0, where the vendor exposes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decoder_util: Option<f32>,
    /// Active concurrent hardware encode sessions (NVIDIA), if known. This is the
    /// DEVICE-WIDE count across every process sharing the GPU (e.g. a co-tenant
    /// NVR), not just ours — see `self_encoder_sessions` for our share.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoder_sessions: Option<u32>,
    /// The runtime-discovered concurrent encode-session ceiling (NVIDIA), if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoder_session_ceiling: Option<u32>,

    // ---- Our-process share (vs the device-wide totals above) ----
    // The GPU is frequently SHARED with co-tenant processes (an NVR, another
    // encoder); these `self_*` fields attribute the portion driven by THIS
    // Multiview process (and its children), so the UI can show "ours vs total".
    // All optional: present only where the platform exposes per-process counters
    // (NVIDIA via NVML per-process queries) and our process is actually resident.
    /// Our process's compute (SM) utilisation on this GPU, 0.0–1.0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub self_compute_util: Option<f32>,
    /// Our process's encoder (NVENC) utilisation on this GPU, 0.0–1.0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub self_encoder_util: Option<f32>,
    /// Our process's decoder (NVDEC) utilisation on this GPU, 0.0–1.0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub self_decoder_util: Option<f32>,
    /// VRAM (bytes) attributed to our process on this GPU.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub self_mem_used_bytes: Option<u64>,
    /// Concurrent hardware encode sessions owned by our process on this GPU.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub self_encoder_sessions: Option<u32>,
}

/// A high-rate whole-system metrics sample (numeric only — never a screenshot).
/// Pushed on topic [`crate::topic::Topic::System`], **conflated + drop-oldest**
/// like [`AudioMeter`] (inv #10): the engine never blocks on a client, and a slow
/// UI simply skips samples. The UI subscribes — it never polls these (a REST poll
/// of a per-second value is the wrong shape; only cold historic windows are REST).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SystemMetrics {
    /// Whole-system CPU utilisation, 0.0–1.0 (from `/proc/stat` on Linux) — the
    /// WHOLE host, including co-tenant processes; see `self_cpu_util` for our share.
    pub cpu_util: f32,
    /// Host memory in use (bytes), where known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem_used_bytes: Option<u64>,
    /// Total host memory (bytes), where known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem_total_bytes: Option<u64>,
    /// Our process's CPU utilisation, 0.0–1.0 (from `/proc/self/stat`), as a
    /// fraction of total host CPU capacity — so it composes with `cpu_util` on the
    /// same scale ("we use X of the host's Y total"). `None` off Linux.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub self_cpu_util: Option<f32>,
    /// Resident memory (bytes) of our process (`/proc/self/status` `VmRSS`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub self_mem_used_bytes: Option<u64>,
    /// Per-GPU utilisation samples; empty on a GPU-free host.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gpus: Vec<GpuMetrics>,
    /// Aggregate program output rate across active programs (fps), if running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program_fps: Option<f32>,
    /// The effective sampling cadence on the wire (Hz).
    pub sampled_hz: u32,
}

/// The running state of an output sink.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum OutputRunState {
    /// Not running — program output is stopped/idle.
    Idle,
    /// Coming up.
    Starting,
    /// Serving program output.
    Running,
    /// Make-before-break migration in progress.
    Migrating,
    /// Faulted.
    Error,
}

/// An output sink status update.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputStatus {
    /// The run state.
    pub state: OutputRunState,
    /// Measured output bitrate (bits/sec), if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bitrate_bps: Option<u64>,
    /// Number of currently-connected consumers, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clients: Option<u32>,
}

/// Severity of an operator alert.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AlertSeverity {
    /// Informational.
    Info,
    /// Degraded but operating.
    Warning,
    /// Operator action required.
    Critical,
}

/// An operator alert raised, updated, or cleared. The `key` dedupes the same
/// condition so it coalesces rather than spamming.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Alert {
    /// Stable dedupe key for the condition.
    pub key: String,
    /// The severity.
    pub severity: AlertSeverity,
    /// A short human-readable title.
    pub title: String,
    /// Optional longer detail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Whether the condition is currently active.
    pub active: bool,
}

/// Severity of a [`HealthWarning`] (mirrors [`AlertSeverity`]; a *sibling*, not a
/// reuse, so the two wire enums can evolve independently).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum WarningSeverity {
    /// Informational (e.g. a degradation rung that does not yet affect program).
    Info,
    /// Degraded but operating (the default for a capability mismatch).
    Warning,
    /// Operator action required (program output is or will be affected).
    Critical,
}

/// The stable catalog code of a [`HealthWarning`] (ADR-0035 §5.1).
///
/// Serialised as a **kebab-case** string (the wire-stable code an operator and
/// the UI key on). `#[non_exhaustive]` so the catalog can grow (SA-1+ adds the
/// decode/encode/metric codes) without a breaking wire change. SA-0 surfaces the
/// single compositor-mismatch code; the deserialiser carries an explicit
/// `Unknown` fall-through so a newer engine's code does not hard-fail an older
/// client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum WarningCode {
    /// A GPU is present (NVML/`DeviceLoad` or a presence probe) but the wgpu
    /// compositor resolved a software/CPU-tier adapter — the silent CPU fallback.
    /// Remediation: grant the `graphics` driver capability + install the Vulkan
    /// loader/ICD. **Latched** (a build-time fact; raised once, cleared on
    /// reconfigure/restart — it cannot flap).
    GpuPresentNoVulkanAdapter,
}

/// An actionable health warning — a richer *sibling* of [`Alert`].
///
/// Reuses `Alert`'s dedupe-by-key (`code`) + raise/clear coalescing (`active`)
/// shape, and **adds** the operator-actionable fields `Alert` lacks: a stable
/// `code`, the affected `subsystem`, the `remediation` (the *fix*), and `since`
/// (when it was first raised, engine monotonic nanoseconds). Carried by the
/// [`Event::HealthWarningRaised`] / [`Event::HealthWarningCleared`] variants on
/// [`crate::topic::Topic::Alerts`] (the existing operator-alert lane), emitted
/// through the identical drop-oldest publisher as `SystemMetrics` (inv #10).
///
/// SA-0 surfaces the latched compositor-mismatch warning; the model is the
/// copy-source for SA-1+'s richer catalog (current/expected etc. are added
/// later — kept minimal here, per the SA-0 scope).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthWarning {
    /// The stable catalog code — the dedupe key the store + UI coalesce on.
    pub code: WarningCode,
    /// The severity.
    pub severity: WarningSeverity,
    /// The affected subsystem (e.g. `compositor`, `decode`, `encode`, `gpu`).
    pub subsystem: String,
    /// A clear, human-readable description of the condition.
    pub message: String,
    /// The concrete remediation — what the operator must do to fix it.
    pub remediation: String,
    /// When the condition was first raised (engine monotonic nanoseconds).
    pub since: i64,
    /// Whether the condition is currently active (raise vs clear coalescing).
    pub active: bool,
}

impl HealthWarning {
    /// The stable dedupe key for this warning — its catalog [`WarningCode`] as
    /// the kebab-case wire string. The store coalesces on this so a repeated
    /// raise updates rather than stacks (latched build-time facts cannot flap).
    #[must_use]
    pub fn key(&self) -> &'static str {
        self.code.as_str()
    }
}

impl WarningCode {
    /// The kebab-case wire string for this code (matches the `#[serde(rename)]`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::GpuPresentNoVulkanAdapter => "gpu-present-no-vulkan-adapter",
        }
    }
}

/// An input source connection-state change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputConnection {
    /// The new lifecycle state of the source.
    pub state: LifecycleState,
    /// The reconnect attempt counter, if reconnecting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u32>,
}

/// The full elementary-stream inventory an input offers (RT-3, ADR-0034 §9).
///
/// This is the `data` body of the `input.streams` event (topic
/// [`crate::topic::Topic::Inputs`]): read-only **discovery** so the API/UI can
/// SHOW every elementary stream (video / audio tracks / subtitles / SCTE-35 /
/// KLV / timecode) an input carries. It is emitted when an input's inventory
/// first appears and again as a **delta** on re-probe / PMT-version bump (the
/// inventory is an open-time snapshot; a re-probe replaces it wholesale). The
/// [`inventory`](InputStreams::inventory) is built **off the engine** by the
/// ingest at `open()` (invariant #10) — this crate only carries it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputStreams {
    /// The owning input's id (the configured source id).
    pub input_id: String,
    /// Every elementary stream the input offers, with stable ids + kinds.
    pub inventory: StreamInventory,
}

impl InputStreams {
    /// Build an `input.streams` event body for `input_id` carrying `inventory`.
    #[must_use]
    pub fn new(input_id: impl Into<String>, inventory: StreamInventory) -> Self {
        Self {
            input_id: input_id.into(),
            inventory,
        }
    }
}

/// Progress of a long-running REST command job (correlated via `corr`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobProgress {
    /// A short machine-readable phase label.
    pub phase: String,
    /// Percent complete, `0..=100`.
    pub pct: u8,
    /// An optional human-readable progress message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// An alarm lifecycle transition carrying the full X.733 alarm record.
///
/// This is the `data` body of the `alarm.raised` / `alarm.updated` /
/// `alarm.cleared` / `alarm.acked` events (topic [`crate::topic::Topic::Alarms`]).
/// The kind of transition is encoded by the surrounding [`Event`] variant; the
/// body is the same [`AlarmRecord`] in every case so a receiver always has the
/// current value of the alarm (its severity, scope, dwell and ack state). The
/// roll-up/state-machine logic that produces these records lives in
/// `multiview-engine`; this crate only carries them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlarmTransition {
    /// The current value of the alarm after the transition.
    pub record: AlarmRecord,
}

impl AlarmTransition {
    /// Wrap an [`AlarmRecord`] as an alarm-transition event body.
    #[must_use]
    pub const fn new(record: AlarmRecord) -> Self {
        Self { record }
    }
}

/// What a tally state applies to — the routing scope of a [`TallyEvent`].
///
/// Serialised **tagged** (`#[serde(tag = "kind")]`) per repo conventions; never
/// `untagged`. `#[non_exhaustive]` so finer targets can be added later.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum TallyTarget {
    /// A multiview tile, by zero-based index.
    Tile {
        /// Zero-based tile index.
        index: u32,
    },
    /// A named UMD/tally element (e.g. an external tally-bus name).
    Element {
        /// Element name.
        name: String,
    },
}

/// A resolved tally state for one element (topic [`crate::topic::Topic::Tally`]).
///
/// Carries the [`TallyState`] (lamp colour, brightness, originating bus)
/// produced by the tally arbiter in `multiview-engine` and the
/// [`target`](TallyEvent::target) it applies to. Rendered by `multiview-overlay`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TallyEvent {
    /// What the tally state applies to.
    pub target: TallyTarget,
    /// The resolved tally lamp/UMD state.
    pub state: TallyState,
}

/// The lifecycle phase a salvo transitioned into.
///
/// A salvo is a named atomic multi-element recall (layout + source assignment +
/// UMD + tally arming) applied via an arm/take (ADR-MV004). Serialised
/// **tagged** (`#[serde(tag = "phase")]`); never `untagged`. `#[non_exhaustive]`
/// so additional phases can be added without a breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
#[non_exhaustive]
pub enum SalvoPhase {
    /// The salvo has been armed (staged) and is awaiting a take.
    Armed,
    /// The salvo has been taken (applied atomically).
    Taken,
    /// A previously-armed salvo was cancelled before being taken.
    Cancelled,
}

/// A salvo arm/take lifecycle event (topic [`crate::topic::Topic::Tally`]).
///
/// Identifies the [`salvo`](SalvoEvent::salvo) by name, the
/// [`phase`](SalvoEvent::phase) it transitioned into, and optionally the output
/// [`head`](SalvoEvent::head) the recall applies to (multi-head walls).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SalvoEvent {
    /// Stable salvo identifier/name.
    pub salvo: String,
    /// The lifecycle phase this event reports.
    pub phase: SalvoPhase,
    /// The output head this recall applies to, if scoped to one head.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
}

impl SalvoEvent {
    /// Construct a salvo event for `salvo` entering `phase`, with no head scope.
    #[must_use]
    pub fn new(salvo: impl Into<String>, phase: SalvoPhase) -> Self {
        Self {
            salvo: salvo.into(),
            phase,
            head: None,
        }
    }

    /// Builder: scope this salvo event to a specific output head.
    #[must_use]
    pub fn with_head(mut self, head: impl Into<String>) -> Self {
        self.head = Some(head.into());
        self
    }
}

// ---- Devices realtime surface (ADR-RT007, managed-devices.md §2.1/§7) ----

/// A managed device's lifecycle state (managed-devices.md §2.2), uppercase on
/// the wire exactly as the status JSON documents it. `#[non_exhaustive]` so a
/// future state does not break the wire enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[non_exhaustive]
pub enum DeviceState {
    /// Present only in the untrusted discovery inventory; not yet adopted.
    Discovered,
    /// Registry record created; the first probe is in flight.
    Adopting,
    /// Reachable and healthy.
    Online,
    /// Reachable, but the device reports a fault (decode stalled,
    /// over-temperature) while the management channel still answers.
    Degraded,
    /// Credentials rejected; the breaker opens immediately (no retry storm).
    AuthFailed,
    /// Supervised reconnect in progress (backoff + jitter + circuit breaker).
    Unreachable,
}

/// How well a device can participate in synchronized presentation — a fixed
/// probed tri-state (managed-devices.md §2.3), kebab-case on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum SyncCapability {
    /// The device presents by frame choice against the published epoch
    /// (display nodes): the same frame index everywhere.
    FrameAccurate,
    /// The device accepts only a fixed presentation offset trim (vendor
    /// decoders): drift stays bounded, never frame-locked.
    OffsetOnly,
    /// No sync mechanism at all (e.g. Cast-class endpoints).
    None,
}

/// The fixed probed capability flags of a managed device (managed-devices.md
/// §2.3): a driver maps its device into exactly this shape — never a vendor's
/// full feature tree.
// The documented wire contract IS a fixed set of independent probed boolean
// flags (`{"encode":false,"decode":true,…}`, managed-devices.md §2.1/§2.3);
// a state-machine or bitflags refactor would change the JSON shape, so the
// bools are the root design here, not an accident.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceCapabilities {
    /// The device can encode (offer streams Multiview ingests as Sources).
    pub encode: bool,
    /// The device can decode (receive a Multiview output).
    pub decode: bool,
    /// The device drives a physical display.
    pub display: bool,
    /// How well the device can participate in synchronized presentation.
    pub sync: SyncCapability,
    /// The device handles audio.
    pub audio: bool,
    /// The device can be rebooted via the management channel.
    pub reboot: bool,
    /// The device supports managed firmware update.
    pub firmware_update: bool,
}

/// The direction of a device-reported active stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum DeviceStreamRole {
    /// The device encodes; Multiview ingests it as a Source.
    Encode,
    /// The device decodes a Multiview output.
    Decode,
}

/// One device-reported active stream in a [`DeviceStatus`] summary
/// (managed-devices.md §2.1). Optional fields are absent where the vendor does
/// not report that figure.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceStreamStatus {
    /// Whether the device is encoding or decoding this stream.
    pub role: DeviceStreamRole,
    /// The Multiview output this decode stream is bound to, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_ref: Option<String>,
    /// Device-reported stream bitrate (bits/sec), if reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bitrate_bps: Option<u64>,
    /// Device-reported stream rate (fps), if reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fps: Option<f32>,
    /// Whether the device reports this stream as healthy.
    pub healthy: bool,
}

/// The **measured** sync tier a device or group actually achieves (ADR-M010's
/// published tier table, collapsed to its honest wire vocabulary): never an
/// aspirational claim. Kebab-case on the wire (managed-devices.md §2.1 uses
/// `"bounded-skew"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum AchievedSync {
    /// The same frame index everywhere (tiers S/A/B: our nodes on PTP/chrony).
    FrameAccurate,
    /// Drift-bounded only (tier C: vendor decoders, ±100–500 ms).
    BoundedSkew,
    /// No measurable sync (tier D: never part of a synchronized canvas).
    None,
}

/// A device's sync-group membership summary inside a [`DeviceStatus`]
/// (managed-devices.md §2.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceSyncSummary {
    /// The sync group this device belongs to.
    pub group: String,
    /// The per-member presentation offset trim (milliseconds, AES67
    /// link-offset semantics applied to video).
    pub offset_ms: i64,
    /// The tier this member actually achieves (weakest-member inputs).
    pub achieved: AchievedSync,
}

/// The conflated, latest-wins per-device runtime snapshot — the `data` body of
/// the `device.status` event (topic [`crate::topic::Topic::Devices`], envelope
/// `id` = device id), matching the status JSON shape in managed-devices.md
/// §2.1 with one additive field: [`device_id`](DeviceStatus::device_id), so
/// snapshot rows are self-describing (the envelope `id` carries the same
/// value).
///
/// **Conflation policy (ADR-RT007 / ADR-RT004, invariant #10):** this is
/// telemetry whose latest value supersedes all prior values. The contract its
/// producer must honour (the driver pollers + conflating broadcaster land in
/// `multiview-control` with DEV-A3/A4 — no producer emits this event yet): a
/// control-plane poller (~1 Hz per device) publishes into a `tokio::watch`
/// (latest-wins), the session pump conflates per connection, and the frame is
/// **excluded from the lossless replay ring** ([`Event::is_conflated`]): a
/// re-snapshot heals it, so conflation is *correct*, not lossy. Staleness is
/// surfaced via [`last_seen_ts`](DeviceStatus::last_seen_ts), never papered
/// over. The engine never produces, forwards, or awaits this event; this
/// crate carries the type and the policy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceStatus {
    /// The registry device id this snapshot describes.
    pub device_id: String,
    /// The device's lifecycle state.
    pub state: DeviceState,
    /// The device's current converged mode (driver vocabulary, e.g.
    /// `decoder`), once known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// The probed capability flags; absent until the first successful probe.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<DeviceCapabilities>,
    /// Device-reported active streams (empty when none / unknown).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub streams: Vec<DeviceStreamStatus>,
    /// Sync-group membership summary, if the device is in a group.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync: Option<DeviceSyncSummary>,
    /// Device-reported temperature (°C), where the vendor exposes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature_c: Option<f32>,
    /// When the device last answered (engine monotonic nanoseconds — the same
    /// clock family as the envelope `ts`, so staleness math is direct). Absent
    /// until the device has answered at least once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen_ts: Option<MediaTime>,
}

impl DeviceStatus {
    /// A minimal pre-probe status row for `device_id` in `state`: every
    /// optional field absent, no streams.
    #[must_use]
    pub fn new(device_id: impl Into<String>, state: DeviceState) -> Self {
        Self {
            device_id: device_id.into(),
            state,
            mode: None,
            capabilities: None,
            streams: Vec::new(),
            sync: None,
            temperature_c: None,
            last_seen_ts: None,
        }
    }
}

/// A device was adopted into the registry — the `data` body of
/// `device.adopted` (lossless low-rate lifecycle lane).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceAdopted {
    /// The registry device id.
    pub device_id: String,
    /// The compiled-in driver managing it (e.g. `zowietek`).
    pub driver: String,
    /// The operator-facing display name, if set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// A device was removed from the registry — the `data` body of
/// `device.removed` (lossless low-rate lifecycle lane).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceRemoved {
    /// The registry device id that was removed.
    pub device_id: String,
}

impl DeviceRemoved {
    /// Build a removal event body for `device_id`.
    #[must_use]
    pub fn new(device_id: impl Into<String>) -> Self {
        Self {
            device_id: device_id.into(),
        }
    }
}

/// The phase of a device mode convergence reported by `device.mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ModePhase {
    /// Convergence began (the declared impact is now in effect).
    Started,
    /// The device converged to the requested mode.
    Finished,
    /// Convergence failed; the driver re-converges per its supervision policy.
    Failed,
}

/// The declared impact class of a management change (the instant-apply
/// doctrine's legend, managed-devices.md §10). `#[non_exhaustive]` so further
/// classes can be added without a breaking wire change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ImpactClass {
    /// Control-plane only: no media path is touched.
    #[serde(rename = "cp")]
    ControlPlane,
    /// Class-1: hot/seamless at a frame boundary (invariant #11).
    #[serde(rename = "c1")]
    Class1,
    /// Class-2: controlled reset via make-before-break migration.
    #[serde(rename = "c2")]
    Class2,
    /// Device-side reset: the DEVICE pipeline restarts; Multiview program
    /// output is unaffected.
    #[serde(rename = "dev")]
    Device,
}

/// A device mode convergence started/finished/failed — the `data` body of
/// `device.mode` (lossless low-rate lifecycle lane), carrying the impact
/// declared **before** apply per the instant-apply doctrine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceMode {
    /// The registry device id converging.
    pub device_id: String,
    /// The target mode being converged to (driver vocabulary).
    pub mode: String,
    /// Which convergence phase this event reports.
    pub phase: ModePhase,
    /// The declared impact class (DEV for vendor decoders: close-before-open,
    /// the device pipeline restarts).
    pub impact: ImpactClass,
    /// The human-readable declared-impact statement shown to the operator
    /// pre-apply, if the driver provides one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// A driver-reported device error — the `data` body of `device.error`
/// (lossless low-rate lifecycle lane).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceError {
    /// The registry device id the error concerns.
    pub device_id: String,
    /// A short machine-readable code where the driver has one (vendor status
    /// codes pass through verbatim).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// A human-readable description of the error.
    pub message: String,
}

/// What changed about a device's sync participation, carried by
/// [`DeviceSync`]. Serialised **tagged** (`#[serde(tag = "kind")]`) per repo
/// conventions; never `untagged`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum SyncChange {
    /// The device joined the group with this presentation offset trim.
    Joined {
        /// The per-member offset trim (milliseconds).
        offset_ms: i64,
    },
    /// The device left the group.
    Left,
    /// The member's achieved tier changed (weakest-member recomputation
    /// inputs).
    Tier {
        /// The tier this member now actually achieves.
        achieved: AchievedSync,
    },
    /// A drift-alarm threshold crossing: the measured skew moved past (or back
    /// inside) the group's target.
    Drift {
        /// The measured skew for this member (milliseconds).
        measured_skew_ms: f32,
        /// The group's configured skew target (milliseconds).
        target_skew_ms: u32,
        /// `true` when the measurement crossed above the target (alarm raised
        /// after dwell); `false` when it recovered back inside it.
        exceeded: bool,
    },
}

/// A device's sync-group membership / achieved-tier / drift state changed —
/// the `data` body of `device.sync` (lossless low-rate lifecycle lane).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceSync {
    /// The member device id.
    pub device_id: String,
    /// The sync group concerned.
    pub group: String,
    /// What changed.
    pub change: SyncChange,
}

/// The address family of a discovery result. IPv6-first (ADR-0042): IPv4
/// results are explicitly labelled **legacy** on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum AddressFamily {
    /// An IPv6 endpoint (the lead/default family).
    #[serde(rename = "ipv6")]
    Ipv6,
    /// A legacy IPv4-only endpoint (deprecation-path interop).
    #[serde(rename = "ipv4-legacy")]
    Ipv4Legacy,
}

/// One untrusted discovery-inventory row streamed while a
/// `POST /discovery/devices/scan` operation runs — the `data` body of
/// `device.discovered`, correlated to the scan via the envelope `corr`
/// (ADR-RT007). Discovery rows are an **untrusted inventory** requiring
/// explicit confirm-adopt (ADR-0041 doctrine); they carry no registry id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceDiscovered {
    /// The candidate driver that recognised the device (e.g. `zowietek`).
    pub driver: String,
    /// The management endpoint (URL/host; IPv6 literals bracketed).
    pub address: String,
    /// The address family — IPv4 results are labelled legacy.
    pub family: AddressFamily,
    /// The advertised device name, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// What disciplines the wall-clock estimate behind a [`TimingStatus`] epoch
/// (ADR-M010): PTP servo or chrony-disciplined system time. The clock
/// **never** paces the tick loop (invariant #1) — it labels the timeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ClockSource {
    /// ST 2059-2 PTP servo (PHC-disciplined).
    Ptp,
    /// chrony/NTP-disciplined system time.
    System,
}

/// The discipline quality of the clock behind a [`TimingStatus`] epoch —
/// mirrors the engine servo's lock-state lifecycle (see
/// `multiview_core::wallclock`: `Locked`/`Holdover`/`Acquiring`/`Freerun`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ClockQuality {
    /// The servo is locked to its reference.
    Locked,
    /// The reference was lost recently; coasting on the last discipline.
    Holdover,
    /// Acquiring lock; the estimate is not yet trustworthy.
    Acquiring,
    /// Undisciplined free-run.
    Freerun,
}

/// One sync group's **measured** skew/tier as carried by [`TimingStatus`]
/// (achieved tier = weakest member, never over-claimed).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SyncGroupSkew {
    /// The sync group measured.
    pub group: String,
    /// The tier the group actually achieves (weakest member).
    pub achieved: AchievedSync,
    /// The worst measured member skew (milliseconds), where a measurement
    /// exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub measured_skew_ms: Option<f32>,
}

/// The F3 sync-telemetry payload — the `data` body of `timing.status` (topic
/// [`crate::topic::Topic::Devices`], envelope `id` = program or sync-group
/// id): the outbound presentation epoch
/// `{stream_id, WallClockRef, link_offset, clock_source, clock_quality}`
/// published per ADR-M010, plus per-sync-group achieved-skew measurements.
///
/// **Conflation policy (ADR-RT007):** latest-wins and **excluded from the
/// lossless replay ring** ([`Event::is_conflated`]) — the epoch is an exact
/// affine map that remains valid when stale, so a node that misses updates
/// keeps the last epoch and free-runs (it degrades, never stalls). Produced by
/// a control-plane task from the engine's watch-published estimate; the engine
/// never awaits a reader (invariant #10). The achieved tier is the *measured*
/// tier, never an aspirational one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimingStatus {
    /// The program/output stream this epoch maps.
    pub stream_id: String,
    /// The exact affine media↔wall map anchoring the output tick counter to
    /// disciplined wall-clock nanoseconds.
    pub epoch: WallClockRef,
    /// The fixed receiver-side presentation delay (nanoseconds, AES67
    /// link-offset semantics applied to video): uniformity across nodes is
    /// the goal, not smallness.
    pub link_offset_ns: i64,
    /// What disciplines the wall estimate.
    pub clock_source: ClockSource,
    /// The discipline quality of that clock.
    pub clock_quality: ClockQuality,
    /// Per-sync-group measured skew/tier (omitted when no groups exist).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<SyncGroupSkew>,
}

/// Why the resource-adaptive controller shed load rather than holding or
/// migrating the pipeline (invariant #9).
///
/// The wire mirror of the engine's `multiview_engine::placement::ShedReason`
/// (and `multiview_telemetry::placement::SuppressReason` / the retention store's
/// `ShedReason`) carried on the realtime stream so the consent-independent
/// retention store can record *why* a shed happened (§7.2 support diagnostics).
/// Serialised **`snake_case`** (the stable label the UI + retention store key
/// on); `#[non_exhaustive]` so a future reason is an additive, non-breaking
/// change (ADR-RT002/RT003: additive, versioned, never breaking).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ShedReason {
    /// The overloaded pipeline is pinned to its device and may not migrate, so
    /// the only relief is the cheap local degradation ladder.
    Pinned,
    /// The pipeline feeds a local display sink whose framebuffer must live on
    /// the connector-owning GPU (ADR-0044 §3), so composite may not migrate off
    /// it — the only relief is a local shed.
    DisplayBound,
    /// No materially-better home exists (the whole host is loaded), so a
    /// migration would not cure the imbalance — shed locally.
    NoBetterHome,
    /// A better home exists but the anti-storm gate (cooldown / per-GPU budget)
    /// forbids moving this tick, so shed locally to hold quality.
    AntiStorm,
    /// The encode/egress stage could not keep up at the output cadence, so a
    /// composited frame was shed (drop-on-overload) rather than blocking the
    /// output clock (invariants #1 + #10) — the real live shed today.
    EncoderOverload,
}

impl ShedReason {
    /// The stable, lower-case wire label for this reason (matches the
    /// `#[serde(rename_all)]`).
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Pinned => "pinned",
            Self::DisplayBound => "display_bound",
            Self::NoBetterHome => "no_better_home",
            Self::AntiStorm => "anti_storm",
            Self::EncoderOverload => "encoder_overload",
        }
    }
}

/// What a shed-load decision applied to — the scope of a [`ShedLoad`] event.
///
/// Serialised **tagged** (`#[serde(tag = "kind")]`) per repo conventions; never
/// `untagged`. `#[non_exhaustive]` so finer scopes can be added later without a
/// breaking wire change.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ShedScope {
    /// The shed touched the whole-program encode/egress path (the
    /// drop-on-overload egress shed): a composited canvas frame was dropped.
    Program,
    /// The shed degraded a specific input/tile (the cheapest-impact-first
    /// tile-by-tile shedding of the degradation ladder).
    Input {
        /// The configured input/source id the shed degraded.
        id: String,
    },
    /// The shed degraded a shared resource (e.g. a preview/encode pool) rather
    /// than the program output or a single input.
    Shared,
}

/// A resource-adaptive **shed-load** decision — the engine relieved sustained
/// overload by shedding work rather than blocking the output clock (invariant
/// #9). Carried on topic [`crate::topic::Topic::Alerts`] (the lossless
/// degradation-signal lane, sibling to [`HealthWarning`]) and emitted through
/// the same drop-oldest publisher as every other engine event (invariant #10) —
/// the engine never blocks on it.
///
/// This is the live producer the consent-independent retention store's
/// shed-load category (`record_shed_at`) consumes (ADR-0052 §3,
/// conspect-account-architecture §7.2): timestamp comes from the envelope `ts`
/// / the feed's wall clock, `reason`/`scope` say *what* was shed and *why*, and
/// `level`/`dropped` quantify it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShedLoad {
    /// Why load was shed rather than held or migrated.
    pub reason: ShedReason,
    /// What the shed applied to (program / a specific input / a shared resource).
    pub scope: ShedScope,
    /// The degradation-ladder level after the shed (`0` = full quality); higher
    /// means more aggressive shedding.
    pub level: u32,
    /// The cumulative count of frames/units shed under this condition at the
    /// time of the event (monotonic for a sustained overload; `0` when the shed
    /// is a ladder move that dropped nothing yet).
    pub dropped: u64,
}

/// The discriminated payload of every frame: control frames and data events.
///
/// Internally tagged on `t` with the body under `data` (ADR-RT002). The
/// `Envelope` flattens this so the wire frame carries `t` and `data` alongside
/// the envelope metadata.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", content = "data")]
#[non_exhaustive]
pub enum Event {
    // ---- Control frames (topic == `$control`) ----
    /// First server frame after auth.
    #[serde(rename = "$hello")]
    Hello(Hello),
    /// Client asks to receive topics.
    #[serde(rename = "$subscribe")]
    Subscribe(Subscribe),
    /// Per-topic subscribe ack (precedes the snapshot).
    #[serde(rename = "$subscribed")]
    Subscribed(Subscribed),
    /// Client stops receiving topics.
    #[serde(rename = "$unsubscribe")]
    Unsubscribe(Unsubscribe),
    /// Client changes a high-rate topic's wire cadence.
    #[serde(rename = "$set_rate")]
    SetRate(SetRate),
    /// Client presents its resume cursor after reconnect.
    #[serde(rename = "$resume")]
    Resume(Resume),
    /// Server: gap unrecoverable; client must rebuild from a fresh snapshot.
    #[serde(rename = "$resync")]
    Resync(Resync),
    /// Server: this connection overflowed; re-snapshot the affected topic.
    #[serde(rename = "$lag")]
    Lag(Lag),
    /// Application-level heartbeat (server->client).
    #[serde(rename = "$ping")]
    Ping,
    /// Application-level heartbeat reply (client->server).
    #[serde(rename = "$pong")]
    Pong,
    /// A control-plane error.
    #[serde(rename = "$error")]
    Error(ProtocolError),

    // ---- Data events (dotted `t`, carried on their topic) ----
    /// Tile state-machine transition (topic `tiles`).
    #[serde(rename = "tile.state")]
    TileState(TileState),
    /// Full current per-tile lifecycle baseline (topic `tiles`): the
    /// connect-time `$snapshot` a client REBUILDS its tile cache from before
    /// any sparse `tile.state` delta (realtime-api §5, ADR-RT003). Despite the
    /// `$`-prefixed tag this is **not** a control frame: the documented
    /// per-topic `$snapshot` rides its DATA topic (`tiles`), never `$control`,
    /// and clients discriminate it by `t` **plus** `topic`.
    #[serde(rename = "$snapshot")]
    TilesSnapshot(TilesSnapshot),
    /// High-rate audio meter sample (topic `audio.meters`).
    #[serde(rename = "audio.meter")]
    AudioMeter(AudioMeter),
    /// High-rate whole-system metrics sample (topic `system`): cpu / gpu /
    /// encoder-decoder. Conflated + drop-oldest; pushed, never polled.
    #[serde(rename = "system.metrics")]
    SystemMetrics(SystemMetrics),
    /// Output sink status (topic `outputs`).
    #[serde(rename = "output.status")]
    OutputStatus(OutputStatus),
    /// An operator alert raised (topic `alerts`).
    #[serde(rename = "alert.raised")]
    AlertRaised(Alert),
    /// An operator alert cleared (topic `alerts`).
    #[serde(rename = "alert.cleared")]
    AlertCleared(Alert),
    /// An actionable health warning was raised (topic `alerts`): e.g. a GPU is
    /// present but compositing fell back to CPU (ADR-0035 SA-0). A richer sibling
    /// of `alert.raised` carrying a stable `code` + a `remediation`. Latched for
    /// capability-mismatch codes (a build-time fact; raised once).
    #[serde(rename = "health.warning.raised")]
    HealthWarningRaised(HealthWarning),
    /// A previously-raised health warning cleared (topic `alerts`): the carried
    /// warning has `active = false`. Mirrors `alert.cleared`.
    #[serde(rename = "health.warning.cleared")]
    HealthWarningCleared(HealthWarning),
    /// A resource-adaptive shed-load decision (topic `alerts`): the engine
    /// relieved sustained overload by shedding work rather than blocking the
    /// output clock (invariant #9). A discrete, lossless degradation-signal
    /// event the consent-independent retention store records (§7.2).
    #[serde(rename = "shed.load")]
    ShedLoad(ShedLoad),
    /// Input source connection change (topic `inputs`).
    #[serde(rename = "input.connection")]
    InputConnection(InputConnection),
    /// An input's elementary-stream inventory appeared or changed on re-probe
    /// (topic `inputs`): read-only discovery of every stream the input offers
    /// (RT-3). Rides the same `inputs` lane as `input.connection`.
    #[serde(rename = "input.streams")]
    InputStreams(InputStreams),
    /// Long-running job progress (topic `jobs`, correlated by `corr`).
    #[serde(rename = "job.progress")]
    JobProgress(JobProgress),

    // ---- Broadcast monitoring/control events ----
    /// A monitoring alarm was first raised (topic `alarms`).
    #[serde(rename = "alarm.raised")]
    AlarmRaised(AlarmTransition),
    /// An active alarm's value changed — e.g. severity escalated or dwell
    /// advanced (topic `alarms`).
    #[serde(rename = "alarm.updated")]
    AlarmUpdated(AlarmTransition),
    /// An alarm's underlying condition cleared (topic `alarms`).
    #[serde(rename = "alarm.cleared")]
    AlarmCleared(AlarmTransition),
    /// An operator acknowledged an alarm (topic `alarms`).
    #[serde(rename = "alarm.acked")]
    AlarmAcked(AlarmTransition),
    /// A tile/element's resolved tally lamp/UMD state changed (topic `tally`).
    #[serde(rename = "tally.state")]
    TallyState(TallyEvent),
    /// A salvo was armed (staged, awaiting take) (topic `tally`).
    #[serde(rename = "salvo.armed")]
    SalvoArmed(SalvoEvent),
    /// A salvo was taken (applied atomically) (topic `tally`).
    #[serde(rename = "salvo.taken")]
    SalvoTaken(SalvoEvent),
    /// A previously-armed salvo was cancelled before being taken (topic `tally`).
    #[serde(rename = "salvo.cancelled")]
    SalvoCancelled(SalvoEvent),

    // ---- Devices realtime surface (topic `devices`, ADR-RT007) ----
    /// Conflated latest-wins per-device runtime snapshot (envelope `id` =
    /// device id). Excluded from the lossless replay ring per event type
    /// ([`Event::is_conflated`]); a re-snapshot heals it.
    #[serde(rename = "device.status")]
    DeviceStatus(DeviceStatus),
    /// A device was adopted into the registry (lossless lifecycle lane).
    #[serde(rename = "device.adopted")]
    DeviceAdopted(DeviceAdopted),
    /// A device was removed from the registry (lossless lifecycle lane).
    #[serde(rename = "device.removed")]
    DeviceRemoved(DeviceRemoved),
    /// A device mode convergence started/finished/failed, with its declared
    /// impact (lossless lifecycle lane).
    #[serde(rename = "device.mode")]
    DeviceMode(DeviceMode),
    /// A driver-reported device error (lossless lifecycle lane).
    #[serde(rename = "device.error")]
    DeviceError(DeviceError),
    /// A device's sync-group membership / achieved tier / drift state changed
    /// (lossless lifecycle lane).
    #[serde(rename = "device.sync")]
    DeviceSync(DeviceSync),
    /// An untrusted discovery row streamed while a scan operation runs,
    /// correlated via the envelope `corr` (lossless lifecycle lane).
    #[serde(rename = "device.discovered")]
    DeviceDiscovered(DeviceDiscovered),
    /// F3 sync telemetry: the outbound presentation epoch + per-group achieved
    /// skew (envelope `id` = program or sync-group id). Latest-wins and
    /// ring-excluded like `device.status` — the affine epoch stays valid when
    /// stale, so receivers free-run on a missed update, never stall.
    #[serde(rename = "timing.status")]
    TimingStatus(TimingStatus),
}

impl Event {
    /// The wire discriminator string (`t`) for this event.
    #[must_use]
    pub const fn type_tag(&self) -> &'static str {
        match self {
            Self::Hello(_) => "$hello",
            Self::Subscribe(_) => "$subscribe",
            Self::Subscribed(_) => "$subscribed",
            Self::Unsubscribe(_) => "$unsubscribe",
            Self::SetRate(_) => "$set_rate",
            Self::Resume(_) => "$resume",
            Self::Resync(_) => "$resync",
            Self::Lag(_) => "$lag",
            Self::Ping => "$ping",
            Self::Pong => "$pong",
            Self::Error(_) => "$error",
            Self::TileState(_) => "tile.state",
            Self::TilesSnapshot(_) => "$snapshot",
            Self::AudioMeter(_) => "audio.meter",
            Self::SystemMetrics(_) => "system.metrics",
            Self::OutputStatus(_) => "output.status",
            Self::AlertRaised(_) => "alert.raised",
            Self::AlertCleared(_) => "alert.cleared",
            Self::HealthWarningRaised(_) => "health.warning.raised",
            Self::HealthWarningCleared(_) => "health.warning.cleared",
            Self::ShedLoad(_) => "shed.load",
            Self::InputConnection(_) => "input.connection",
            Self::InputStreams(_) => "input.streams",
            Self::JobProgress(_) => "job.progress",
            Self::AlarmRaised(_) => "alarm.raised",
            Self::AlarmUpdated(_) => "alarm.updated",
            Self::AlarmCleared(_) => "alarm.cleared",
            Self::AlarmAcked(_) => "alarm.acked",
            Self::TallyState(_) => "tally.state",
            Self::SalvoArmed(_) => "salvo.armed",
            Self::SalvoTaken(_) => "salvo.taken",
            Self::SalvoCancelled(_) => "salvo.cancelled",
            Self::DeviceStatus(_) => "device.status",
            Self::DeviceAdopted(_) => "device.adopted",
            Self::DeviceRemoved(_) => "device.removed",
            Self::DeviceMode(_) => "device.mode",
            Self::DeviceError(_) => "device.error",
            Self::DeviceSync(_) => "device.sync",
            Self::DeviceDiscovered(_) => "device.discovered",
            Self::TimingStatus(_) => "timing.status",
        }
    }

    /// Whether this event is a **conflated latest-wins** telemetry sample —
    /// the per-event-type half of the replay-ring exclusion rule.
    ///
    /// The ring-exclusion rule the session pump must apply (ADR-RT003) is
    /// `topic.is_high_rate() || event.is_conflated()`. ADR-RT007 extends the
    /// per-topic rule ([`crate::topic::Topic::is_high_rate`]) to per-event-type
    /// granularity for the one mixed-cadence `devices` topic: `device.status`
    /// and `timing.status` are latest-wins (a re-snapshot heals them — the
    /// status is a snapshot by definition, and the timing epoch is an affine
    /// map that stays valid when stale), while the device lifecycle events on
    /// the same topic stay lossless in the ring. The existing conflated lanes
    /// (`audio.meter`, `system.metrics`) answer `true` here too, so this
    /// predicate is the single per-event source of truth for conflation. No
    /// production consumer consults it yet: the pump's Devices handling lands
    /// with the producers in DEV-A3.
    #[must_use]
    pub const fn is_conflated(&self) -> bool {
        matches!(
            self,
            Self::AudioMeter(_)
                | Self::SystemMetrics(_)
                | Self::DeviceStatus(_)
                | Self::TimingStatus(_)
        )
    }

    /// Whether this is a control frame (carried on [`crate::topic::Topic::Control`]).
    #[must_use]
    pub const fn is_control(&self) -> bool {
        matches!(
            self,
            Self::Hello(_)
                | Self::Subscribe(_)
                | Self::Subscribed(_)
                | Self::Unsubscribe(_)
                | Self::SetRate(_)
                | Self::Resume(_)
                | Self::Resync(_)
                | Self::Lag(_)
                | Self::Ping
                | Self::Pong
                | Self::Error(_)
        )
    }
}
