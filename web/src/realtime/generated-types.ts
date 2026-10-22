// GENERATED FILE — do not edit by hand.
// Source: docs/api/asyncapi.json (produced by `cargo xtask gen-asyncapi`).
// Regenerate: npm run generate:events
// Consumers: hand-authored runtime in envelope.ts and connection.ts is NOT
// replaced — see ADR-RT006. Import from this module for precise payload types.

/** The MEASURED sync tier actually achieved (never aspirational): frame-accurate (our nodes), bounded-skew (vendor decoders, ±100–500 ms drift), or none (never part of a synchronized canvas). */
export type AchievedSync = "frame-accurate" | "bounded-skew" | "none";

/** Data body of `alarm.raised`, `alarm.updated`, `alarm.cleared`, `alarm.acked`. Carries the current AlarmRecord value after the transition (X.733-aligned). */
export interface AlarmTransition {
  /** The current alarm record after the transition. */
  readonly record: Record<string, unknown>;
}

/** Data body of `alert.raised` and `alert.cleared` events. The `key` field is a stable dedupe identifier: multiple raised frames with the same key coalesce. */
export interface Alert {
  /** Whether the condition is currently active. */
  readonly active: boolean;
  /** Optional longer detail. */
  readonly detail?: string;
  /** Stable dedupe key for the condition. */
  readonly key: string;
  readonly severity: AlertSeverity;
  /** Short human-readable title. */
  readonly title: string;
}

/** Severity of an operator alert. */
export type AlertSeverity = "info" | "warning" | "critical";

/** Data body of the `audio.loudness` event: a program-bus EBU R128 loudness sample (M/S/I LUFS, LRA, true-peak dBTP) plus the compliance reference. Numeric metadata only — never audio content. Conflated/sampled at ~10 Hz on the wire (high-rate lane); ballistics are applied client-side. The integrating fields are absent below the -70 LUFS absolute gate (no fabricated value). */
export interface AudioLoudness {
  /** True-peak ceiling, dBTP (compliance reference, e.g. -1.5). */
  readonly ceiling_dbtp: number;
  /** Makeup gain the loudnorm processor is applying, dB. Absent when no normaliser is engaged. */
  readonly gain_db?: number;
  /** Integrated (gated) loudness, LUFS. Absent until enough gated audio. */
  readonly integrated?: number;
  /** Loudness range (EBU Tech 3342), LU. Absent until enough gated audio. */
  readonly lra?: number;
  /** Momentary loudness (400 ms window), LUFS. Absent below the absolute gate. */
  readonly momentary?: number;
  /** Program/bus index this loudness sample is for. */
  readonly program: number;
  /** Effective wire cadence (Hz). */
  readonly sampled_hz: number;
  /** Short-term loudness (3 s window), LUFS. Absent below the absolute gate. */
  readonly short_term?: number;
  /** Normalisation target loudness, LUFS (compliance reference, e.g. -23 / -16). */
  readonly target_lufs: number;
  /** Live convergence tolerance, LU (the in-spec band is target ± tolerance). */
  readonly tolerance_lu: number;
  /** Maximum true-peak across channels (4x oversampled), dBTP. Absent when disabled. */
  readonly true_peak_dbtp?: number;
}

/** Data body of the `audio.meter` event: a high-rate per-input/track peak/RMS/clip sample. This is numeric metadata only — never audio content. Conflated/sampled at 10–30 Hz on the wire (high-rate lane). */
export interface AudioMeter {
  /** Whether any channel clipped in this window. */
  readonly clip: boolean;
  /** Whether the meter pipeline overflowed (dropped windows). */
  readonly overflow: boolean;
  /** Per-channel peak level (dBFS). */
  readonly peak_db: readonly number[];
  /** Per-channel RMS level (dBFS). */
  readonly rms_db: readonly number[];
  /** Effective wire cadence (Hz). */
  readonly sampled_hz: number;
  /** Track index. */
  readonly track: number;
}

/** Data body of `cast.session.removed`: an ephemeral cast session was removed — stopped (the receiver STOP that clears the TV) or promoted to a saved device (playback continues under the promoted device id). */
export interface CastSessionRemoved {
  /** The runtime session id that was removed. */
  readonly session_id: string;
}

/** Data body of `cast.session.started`: an ephemeral cast session was started (session-list membership changed). The session's live state rides the conflated `device.status` lane keyed by the same session id. */
export interface CastSessionStarted {
  /** The device authority dialled (`host[:port]`, IPv6 bracketed). */
  readonly address: string;
  /** The operator-facing name, if given. */
  readonly name?: string;
  /** The output id whose HLS rendition the session casts. */
  readonly output: string;
  /** The runtime session id (`cast-session-…`, UUID-fresh per start). */
  readonly session_id: string;
}

/** Data body of `device.adopted`: a device was adopted into the registry. */
export interface DeviceAdopted {
  /** The registry device id. */
  readonly device_id: string;
  /** The compiled-in driver managing it (e.g. `zowietek`). */
  readonly driver: string;
  /** The operator-facing display name, if set. */
  readonly name?: string;
}

/** Fixed probed capability flags — a driver maps its device into exactly this shape. */
export interface DeviceCapabilities {
  /** The device handles audio. */
  readonly audio: boolean;
  /** The device can decode (receive a Multiview output). */
  readonly decode: boolean;
  /** The device drives a physical display. */
  readonly display: boolean;
  /** The device can encode (offers streams Multiview ingests as Sources). */
  readonly encode: boolean;
  /** The device supports managed firmware update. */
  readonly firmware_update: boolean;
  /** The device can be rebooted via the management channel. */
  readonly reboot: boolean;
  readonly sync: SyncCapability;
}

/** Data body of `device.discovered`: one untrusted discovery-inventory row streamed while a scan runs (correlated via the envelope `corr`). Requires explicit confirm-adopt; never auto-ingested. */
export interface DeviceDiscovered {
  /** The management endpoint (URL/host; IPv6 literals bracketed). */
  readonly address: string;
  /** The discovery domain the observing node stamped on this row (ADR-W026), sourced solely from that node's operator-declared `[discovery] domain` config — never from the responder payload or TXT records. Absent when the observing node declared no domain; a discovery-scoped principal is then denied the row (fail-closed). */
  readonly domain?: string;
  /** The candidate driver that recognised the device. */
  readonly driver: string;
  /** Address family — IPv6-first; IPv4 results are labelled legacy. */
  readonly family: "ipv6" | "ipv4-legacy";
  /** The advertised device name, if any. */
  readonly name?: string;
}

/** Data body of `device.error`: a driver-reported device error. */
export interface DeviceError {
  /** Short machine-readable code where the driver has one (vendor codes pass through verbatim). */
  readonly code?: string;
  /** The registry device id the error concerns. */
  readonly device_id: string;
  /** Human-readable description of the error. */
  readonly message: string;
}

/** Data body of `device.mode`: a mode convergence started/finished/failed, carrying the impact declared BEFORE apply (instant-apply doctrine). */
export interface DeviceMode {
  /** The human-readable declared-impact statement, if the driver provides one. */
  readonly detail?: string;
  /** The registry device id converging. */
  readonly device_id: string;
  readonly impact: ImpactClass;
  /** The target mode being converged to (driver vocabulary). */
  readonly mode: string;
  /** Which convergence phase this event reports. */
  readonly phase: "started" | "finished" | "failed";
}

/** Data body of `device.removed`: a device was removed from the registry. */
export interface DeviceRemoved {
  /** The registry device id that was removed. */
  readonly device_id: string;
}

/** Managed-device lifecycle state (managed-devices.md §2.2), uppercase on the wire. */
export type DeviceState = "DISCOVERED" | "ADOPTING" | "ONLINE" | "DEGRADED" | "AUTH_FAILED" | "UNREACHABLE";

/** Data body of `device.status` (managed-devices.md §2.1): the conflated latest-wins per-device runtime snapshot, envelope `id` = device id. Excluded from the lossless replay ring (a re-snapshot heals it); staleness is surfaced via `last_seen_ts`. */
export interface DeviceStatus {
  readonly capabilities?: DeviceCapabilities;
  /** The registry device id this snapshot describes. */
  readonly device_id: string;
  /** When the device last answered (engine monotonic nanoseconds). */
  readonly last_seen_ts?: number;
  /** The device's current converged mode (driver vocabulary), once known. */
  readonly mode?: string;
  readonly state: DeviceState;
  /** Device-reported active streams (omitted when none / unknown). */
  readonly streams?: readonly DeviceStreamStatus[];
  readonly sync?: DeviceSyncSummary;
  /** Device-reported temperature (°C), where exposed. */
  readonly temperature_c?: number;
}

/** One device-reported active stream. Optional fields are absent where the vendor does not report that figure. */
export interface DeviceStreamStatus {
  /** Device-reported stream bitrate (bits/sec), if reported. */
  readonly bitrate_bps?: number;
  /** Device-reported stream rate (fps), if reported. */
  readonly fps?: number;
  /** Whether the device reports this stream as healthy. */
  readonly healthy: boolean;
  /** The Multiview output this decode stream is bound to, if any. */
  readonly output_ref?: string;
  /** Whether the device is encoding or decoding this stream. */
  readonly role: "encode" | "decode";
}

/** Data body of `device.sync`: sync-group membership / achieved-tier / drift change. */
export interface DeviceSync {
  readonly change: SyncChange;
  /** The member device id. */
  readonly device_id: string;
  /** The sync group concerned. */
  readonly group: string;
}

/** A device's sync-group membership summary inside a device.status snapshot. */
export interface DeviceSyncSummary {
  readonly achieved: AchievedSync;
  /** The sync group this device belongs to. */
  readonly group: string;
  /** Per-member presentation offset trim (ms, AES67 link-offset semantics). */
  readonly offset_ms: number;
}

/** A per-GPU utilisation sample. Optional fields are absent where the vendor does not expose that signal. */
export interface GpuMetrics {
  /** Compute (graphics/CUDA) utilisation, 0.0-1.0. */
  readonly compute_util: number;
  /** Decoder (NVDEC/QSV) ASIC utilisation, 0.0-1.0 (vendor-dependent). */
  readonly decoder_util?: number;
  /** Runtime-discovered concurrent encode-session ceiling (NVIDIA). */
  readonly encoder_session_ceiling?: number;
  /** DEVICE-WIDE active concurrent encode sessions (NVIDIA) across all processes. */
  readonly encoder_sessions?: number;
  /** Encoder (NVENC/QSV) ASIC utilisation, 0.0-1.0 (vendor-dependent). */
  readonly encoder_util?: number;
  /** Stable device identity (UUID where available, else an index). */
  readonly id: string;
  /** Total VRAM (bytes). */
  readonly mem_total_bytes: number;
  /** VRAM in use (bytes). */
  readonly mem_used_bytes: number;
  /** Human-readable device name, if known. */
  readonly name?: string;
  /** Our process's compute (SM) utilisation on this GPU, 0.0-1.0. */
  readonly self_compute_util?: number;
  /** Our process's decoder (NVDEC) utilisation, 0.0-1.0. */
  readonly self_decoder_util?: number;
  /** Encode sessions owned by our process on this GPU. */
  readonly self_encoder_sessions?: number;
  /** Our process's encoder (NVENC) utilisation, 0.0-1.0. */
  readonly self_encoder_util?: number;
  /** VRAM (bytes) attributed to our process on this GPU. */
  readonly self_mem_used_bytes?: number;
  readonly vendor: GpuVendor;
}

/** GPU/accelerator hardware vendor (selects which per-engine signals to expect). */
export type GpuVendor = "nvidia" | "intel" | "amd" | "apple" | "other";

/** Data body of `health.warning.raised` and `health.warning.cleared`. A richer sibling of `Alert`: the `code` is the stable dedupe key, and `remediation` carries the concrete fix. Capability-mismatch codes are latched build-time facts (raised once, cleared on reconfigure/restart). */
export interface HealthWarning {
  /** Whether the condition is currently active (raise vs clear). */
  readonly active: boolean;
  readonly code: WarningCode;
  /** A clear, human-readable description of the condition. */
  readonly message: string;
  /** The concrete remediation — what the operator must do to fix it. */
  readonly remediation: string;
  readonly severity: WarningSeverity;
  /** When the condition was first raised (engine monotonic nanoseconds). */
  readonly since: number;
  /** The affected subsystem (e.g. `compositor`, `decode`, `encode`, `gpu`). */
  readonly subsystem: string;
}

/** Data body of `$hello`: the first server frame after auth. */
export interface Hello {
  /** Default wire cadence when `rate_hz` is omitted from `$subscribe`. */
  readonly default_rate_hz: number;
  /** Heartbeat interval (milliseconds). */
  readonly heartbeat_ms: number;
  /** Maximum clamped wire cadence (Hz). */
  readonly max_rate_hz: number;
  /** Minimum clamped wire cadence (Hz). */
  readonly min_rate_hz: number;
  /** Replay ring size (frames per session/topic). */
  readonly replay_ring: number;
  /** Envelope schema majors this server can speak. */
  readonly server_v: readonly number[];
  /** Server-assigned session id. */
  readonly session_id: string;
}

/** Declared impact class of a management change: cp (control-plane only), c1 (hot/seamless at a frame boundary), c2 (controlled reset via make-before-break), dev (the DEVICE pipeline restarts; Multiview program output is unaffected). */
export type ImpactClass = "cp" | "c1" | "c2" | "dev";

/** Data body of the `input.connection` event. */
export interface InputConnection {
  /** Reconnect attempt counter, if reconnecting. */
  readonly attempt?: number;
  /** The owning input's id (the configured source id) — the object axis this lifecycle event is authorized under (ADR-W026). */
  readonly input_id: string;
  readonly state: LifecycleState;
}

/** Data body of the `input.streams` event: an input's full elementary-stream inventory (RT-3). */
export interface InputStreams {
  /** The owning input's configured source id. */
  readonly input_id: string;
  /** The StreamInventory (multiview_core::stream::StreamInventory): every elementary stream the input offers. */
  readonly inventory: {
    /** The input id the inventory is bound to, if known. */
    readonly input_id?: unknown;
    /** Every elementary stream, in container order. */
    readonly streams: readonly Record<string, unknown>[];
  };
}

/** Data body of `job.progress`; correlated to a REST command via the envelope `corr`. */
export interface JobProgress {
  /** Optional human-readable progress message. */
  readonly message?: string;
  /** Percent complete (0–100). */
  readonly pct: number;
  /** Short machine-readable phase label. */
  readonly phase: string;
}

/** Data body of `$lag`: this connection's queue overflowed for a topic. */
export interface Lag {
  readonly action: LagAction;
  /** Number of frames dropped. */
  readonly dropped_n: number;
  /** Topic whose frames were dropped. */
  readonly topic: string;
}

/** What the server did with the dropped frames. */
export type LagAction = "conflated" | "resnapshot";

/** Tile/input lifecycle state (invariant #2: Live → Stale → Reconnecting → NoSignal). */
export type LifecycleState = "LIVE" | "STALE" | "RECONNECTING" | "NO_SIGNAL";

/** Data body of `media.player_state` (ADR-RT008): a media player's discrete transport-state transition, envelope `id` = player id. LOSSLESS — kept in the replay ring, never conflated. `position_frames` is the playhead in integer frames at the output cadence (never a float; clients interpolate between events). */
export interface MediaPlayerEvent {
  /** The asset currently loaded in the player, if any. */
  readonly asset?: string;
  /** Stable media-player id (matches the envelope `id`). */
  readonly player: string;
  /** Playhead position at the transition, in integer frames at the output cadence. */
  readonly position_frames: number;
  readonly state: MediaPlayerState;
}

/** A media player's discrete transport state (tagged by `kind`, never untagged). The `vamping` variant carries `exit_armed`: when set, the current vamp lap finishes then the player exits cleanly at the boundary. `#[non_exhaustive]`: a client must treat an unknown kind as forward-compatible, not an error. */
export type MediaPlayerState =
  | {
  readonly kind: "loading";
}
  | {
  readonly kind: "cued";
}
  | {
  readonly kind: "playing";
}
  | {
  readonly kind: "paused";
}
  | {
  readonly kind: "stopped";
}
  | {
  /** Whether a clean exit is armed: finish the current lap, then leave the vamp at the boundary. */
  readonly exit_armed: boolean;
  readonly kind: "vamping";
}
  | {
  readonly kind: "eof";
};

/** Running state of an output sink. */
export type OutputRunState = "starting" | "running" | "migrating" | "error";

/** Data body of the `output.status` event. */
export interface OutputStatus {
  /** Measured output bitrate (bits/sec), if known. */
  readonly bitrate_bps?: number;
  /** Number of currently-connected consumers, if known. */
  readonly clients?: number;
  readonly state: OutputRunState;
}

/** Data body of `$error`: a control-plane error. */
export interface ProtocolError {
  /** Short stable machine-readable error code. */
  readonly code: string;
  /** Human-readable description. */
  readonly message: string;
}

/** Data body of `$resume` (client→server): present last-seen cursor on reconnect. */
export interface Resume {
  /** Last `seq` the client successfully observed. */
  readonly last_seq: number;
  /** Session to resume. */
  readonly session_id: string;
}

/** Data body of `$resync`: the gap is unrecoverable; the client MUST rebuild state from the fresh snapshot that follows. */
export interface Resync {
  readonly reason: ResyncReason;
  /** Topics the client must rebuild. */
  readonly resubscribe: readonly string[];
}

/** Why a $resync was issued (the client must rebuild, not merge). The first three are unrecoverable resume gaps; `authz_changed` is a mid-session authorization change (object scope narrowed/widened) on an intact connection (ADR-RT010). */
export type ResyncReason = "seq_evicted" | "unknown_session" | "session_expired" | "authz_changed";

/** Data body of `salvo.armed`, `salvo.taken`, `salvo.cancelled`. */
export interface SalvoEvent {
  /** Output head this recall applies to, if scoped. */
  readonly head?: string;
  /** The lifecycle phase this event reports. */
  readonly phase: "armed" | "taken" | "cancelled";
  /** Stable salvo identifier/name. */
  readonly salvo: string;
}

/** Data body of `$set_rate` (client→server): change a topic's wire cadence. */
export interface SetRate {
  /** Requested cadence (Hz); server clamps to [min, max]. */
  readonly rate_hz: number;
  /** Topic whose cadence is changing. */
  readonly topic: string;
}

/** Data body of the `shed.load` event: a resource-adaptive shed-load decision (invariant #9). The consent-independent retention store records these for the §7.2 support bundle (ADR-0052). */
export interface ShedLoad {
  /** Cumulative frames/units shed under this condition at the time of the event. */
  readonly dropped: number;
  /** The degradation-ladder level after the shed (0 = full quality). */
  readonly level: number;
  readonly reason: ShedReason;
  readonly scope: ShedScope;
}

/** Why the resource-adaptive controller shed load rather than holding or migrating the pipeline (invariant #9). Mirrors the engine's `ShedReason`; additive + non-exhaustive (ADR-RT002/RT003). */
export type ShedReason = "pinned" | "display_bound" | "no_better_home" | "anti_storm" | "encoder_overload";

/** What a shed-load decision applied to (tagged by `kind`). */
export type ShedScope =
  | {
  readonly kind: "program";
}
  | {
  /** The configured input/source id the shed degraded. */
  readonly id: string;
  readonly kind: "input";
}
  | {
  readonly kind: "shared";
};

/** Data body of `$subscribe` (client→server): subscribe to topics. */
export interface Subscribe {
  /** Optional resource-id allowlist. */
  readonly ids?: readonly string[];
  /** Optional max cadence (Hz); server clamps and reports effective. */
  readonly rate_hz?: number;
  /** Optional resume cursor: subscribe + replay from after this seq. */
  readonly since_seq?: number;
  /** Topics to subscribe to. */
  readonly topics: readonly string[];
}

/** Data body of `$subscribed`: per-topic ack before the snapshot. */
export interface Subscribed {
  /** Actual cadence after server clamping. */
  readonly effective_rate_hz: number;
  /** The `seq` the forthcoming snapshot is current as of. */
  readonly snapshot_seq: number;
  /** The topic that was subscribed. */
  readonly topic: string;
}

/** How well a device can participate in synchronized presentation (fixed probed tri-state). */
export type SyncCapability = "frame-accurate" | "offset-only" | "none";

/** What changed about a device's sync participation (tagged by `kind`, never untagged). */
export type SyncChange =
  | {
  readonly kind: "joined";
  /** The per-member offset trim (ms). */
  readonly offset_ms: number;
}
  | {
  readonly kind: "left";
}
  | {
  readonly achieved: AchievedSync;
  readonly kind: "tier";
}
  | {
  /** true = crossed above the target; false = recovered back inside it. */
  readonly exceeded: boolean;
  readonly kind: "drift";
  /** The measured skew for this member (ms). */
  readonly measured_skew_ms: number;
  /** The group's configured skew target (ms). */
  readonly target_skew_ms: number;
};

/** One sync group's MEASURED skew/tier (achieved tier = weakest member, never over-claimed). */
export interface SyncGroupSkew {
  readonly achieved: AchievedSync;
  /** The sync group measured. */
  readonly group: string;
  /** The worst measured member skew (ms), where a measurement exists. */
  readonly measured_skew_ms?: number;
}

/** Data body of the `system.metrics` event: a high-rate whole-system sample (cpu / gpu / encoder-decoder). Numeric only; conflated at ~1-2 Hz (high-rate lane). */
export interface SystemMetrics {
  /** Whole-system CPU utilisation, 0.0-1.0. */
  readonly cpu_util: number;
  /** Per-GPU utilisation samples; empty on a GPU-free host. */
  readonly gpus?: readonly GpuMetrics[];
  /** Total host memory (bytes), if known. */
  readonly mem_total_bytes?: number;
  /** Host memory in use (bytes), if known. */
  readonly mem_used_bytes?: number;
  /** Aggregate program output rate (fps), if running. */
  readonly program_fps?: number;
  /** Effective wire sampling cadence (Hz). */
  readonly sampled_hz: number;
}

/** Data body of `tally.state`: resolved tally lamp/UMD state for one element. */
export interface TallyEvent {
  /** The resolved tally lamp/UMD state. */
  readonly state: Record<string, unknown>;
  readonly target: TallyTarget;
}

/** What a tally state applies to (tagged by `kind`). */
export type TallyTarget =
  | {
  /** Zero-based tile index. */
  readonly index: number;
  readonly kind: "tile";
}
  | {
  readonly kind: "element";
  /** Element name. */
  readonly name: string;
};

/** One tile's current lifecycle state inside a `tiles` `$snapshot`. The `id` is the same key the sparse `tile.state` deltas scope their envelope `id` with, so a snapshot-rebuilt cache and its delta patches address the same rows. */
export interface TileSnapshotEntry {
  /** The tile id (the bound source id in the run projection). */
  readonly id: string;
  /** The input bound to the tile, if known. */
  readonly input?: string;
  readonly state: LifecycleState;
}

/** Data body of the `tile.state` event: a tile lifecycle transition. */
export interface TileState {
  readonly from: LifecycleState;
  /** The input bound to the tile at the time of the transition, if any. */
  readonly input?: string;
  readonly to: LifecycleState;
  /** Short machine-readable trigger label (e.g. `nosignal_timeout`). */
  readonly trigger: string;
}

/** Data body of the `tiles`-topic `$snapshot` frame: the full current per-tile lifecycle baseline sent once at connect (after `$hello`). A receiver MUST rebuild (replace) its tile state from it, never merge. */
export interface TilesSnapshot {
  /** The engine state sequence this baseline is current as of. */
  readonly as_of_seq: number;
  /** Every tile's current lifecycle state. */
  readonly tiles: readonly TileSnapshotEntry[];
}

/** Data body of `timing.status` (ADR-M010): the outbound presentation epoch plus per-sync-group achieved skew, envelope `id` = program or sync-group id. Latest-wins and ring-excluded: the affine epoch stays valid when stale, so receivers free-run on a missed update — they never stall. */
export interface TimingStatus {
  /** The discipline quality of that clock (the engine servo's lock-state lifecycle). */
  readonly clock_quality: "locked" | "holdover" | "acquiring" | "freerun";
  /** What disciplines the wall estimate (ST 2059-2 PTP servo or chrony-disciplined system time). The clock labels the timeline; it never paces the tick loop. */
  readonly clock_source: "ptp" | "system";
  readonly epoch: WallClockRef;
  /** Per-sync-group measured skew/tier (omitted when no groups exist). */
  readonly groups?: readonly SyncGroupSkew[];
  /** Fixed receiver-side presentation delay (ns, AES67 link-offset semantics): uniformity is the goal, not smallness. */
  readonly link_offset_ns: number;
  /** The program/output stream this epoch maps. */
  readonly stream_id: string;
}

/** Data body of `$unsubscribe` (client→server): stop receiving topics. */
export interface Unsubscribe {
  /** Topics to stop receiving. */
  readonly topics: readonly string[];
}

/** The exact affine media↔wall map (multiview_core::wallclock::WallClockRef, ADR-0038): wall(pts) = wall_at_anchor_ns + rescale(pts − media_at_anchor). Integer/rational arithmetic only — never float. */
export interface WallClockRef {
  /** Media PTS of the anchor sample, in units of `rate`. */
  readonly media_at_anchor: number;
  /** The media rate (ticks per second) as an exact rational. */
  readonly rate: {
    /** Denominator. */
    readonly den: number;
    /** Numerator. */
    readonly num: number;
  };
  /** Wall-clock instant (ns past the Unix epoch) at the anchor sample. */
  readonly wall_at_anchor_ns: number;
}

/** Stable catalog code of a health warning (kebab-case). `#[non_exhaustive]`: the catalog grows over time, so a client must treat an unknown code as a forward-compatible warning, not an error. */
export type WarningCode = "gpu-present-no-vulkan-adapter" | "config-file-invalid" | "config-file-requires-restart" | "config-file-apply-incomplete";

/** Severity of a health warning (sibling of AlertSeverity). */
export type WarningSeverity = "info" | "warning" | "critical";
