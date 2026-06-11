// useSystemMetrics — the React binding over the `system` realtime topic.
//
// The engine pushes a high-rate `system.metrics` event (cpu / gpu / encoder /
// decoder utilisation) on the `system` topic, conflated + drop-oldest (events
// contract: crates/multiview-events/src/event.rs). This hook subscribes the
// same way `useEngineEvents` does — a self-reconnecting WebSocket whose
// callbacks are cheap and non-blocking — and folds every sample into a BOUNDED
// client-side ring buffer (the last {@link METRICS_RING_CAPACITY} samples). The
// engine is isolated (invariant #10): a slow UI only loses its own frames and
// can never back-pressure the engine, and the client buffer never grows.
import { useEffect, useState } from "react";

import { getStoredToken } from "../api/token";
import { RealtimeConnection } from "./connection";
import type { RealtimeStatus } from "./connection";

/** The GPU vendor classes the control plane reports (mirrors `GpuVendor`). */
export type GpuVendor = "nvidia" | "intel" | "amd" | "apple" | "other";

const GPU_VENDORS: readonly GpuVendor[] = [
  "nvidia",
  "intel",
  "amd",
  "apple",
  "other",
];

/** A per-GPU utilisation sample (mirrors the Rust `GpuMetrics`). */
export interface GpuMetrics {
  /** Stable device identity (UUID where available, else an index string). */
  readonly id: string;
  /** The hardware vendor. */
  readonly vendor: GpuVendor;
  /** Human-readable device name, if known. */
  readonly name?: string;
  /** Compute (graphics/CUDA) utilisation, 0..1. */
  readonly compute_util: number;
  /** VRAM in use (bytes). */
  readonly mem_used_bytes: number;
  /** Total VRAM (bytes). */
  readonly mem_total_bytes: number;
  /** Encoder (NVENC/QSV) ASIC utilisation, 0..1, where exposed. */
  readonly encoder_util?: number;
  /** Decoder (NVDEC/QSV) ASIC utilisation, 0..1, where exposed. */
  readonly decoder_util?: number;
  /** Active concurrent hardware encode sessions (NVIDIA), if known. */
  readonly encoder_sessions?: number;
  /** Runtime-discovered concurrent encode-session ceiling (NVIDIA), if known. */
  readonly encoder_session_ceiling?: number;
  /**
   * OUR process's share of {@link compute_util} (0..1), where the platform can
   * attribute it. The device is shared with co-tenant processes, so the
   * device-wide totals are not all ours; this is the portion we drive. Absent
   * when the platform cannot attribute per-process compute.
   */
  readonly self_compute_util?: number;
  /** OUR process's share of {@link encoder_util} (0..1), where attributable. */
  readonly self_encoder_util?: number;
  /** OUR process's share of {@link decoder_util} (0..1), where attributable. */
  readonly self_decoder_util?: number;
  /** OUR process's VRAM use (bytes) — the portion of {@link mem_used_bytes} ours. */
  readonly self_mem_used_bytes?: number;
  /** OUR active encode sessions — the portion of {@link encoder_sessions} ours. */
  readonly self_encoder_sessions?: number;
}

/** A whole-system metrics sample (mirrors the Rust `SystemMetrics`). */
export interface SystemMetrics {
  /** Whole-system CPU utilisation, 0..1. */
  readonly cpu_util: number;
  /**
   * OUR process's share of the host CPU (0..1), where known. The host is shared
   * with co-tenant processes, so {@link cpu_util} is not all ours; this is the
   * portion we drive. Absent when not attributable.
   */
  readonly self_cpu_util?: number;
  /** Host memory in use (bytes), where known. */
  readonly mem_used_bytes?: number;
  /** OUR process's resident host memory (bytes), the portion of mem_used_bytes. */
  readonly self_mem_used_bytes?: number;
  /** Total host memory (bytes), where known. */
  readonly mem_total_bytes?: number;
  /** Per-GPU utilisation samples; empty on a GPU-free host. */
  readonly gpus: readonly GpuMetrics[];
  /** Aggregate program output rate across active programs (fps), if running. */
  readonly program_fps?: number;
  /** The effective sampling cadence on the wire (Hz). */
  readonly sampled_hz: number;
}

/** The realtime topic the system metrics ride. */
export const SYSTEM_TOPIC = "system";

/** How many samples the client ring buffer retains (bounded; ~1 min at 2 Hz). */
export const METRICS_RING_CAPACITY = 120;

/** Narrow an unknown value to a plain record without an unsafe assertion. */
function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

/** Read a required finite number, or `undefined` if absent/mistyped. */
function num(value: unknown): number | undefined {
  return typeof value === "number" && Number.isFinite(value) ? value : undefined;
}

function vendorFrom(value: unknown): GpuVendor {
  // An unknown/absent vendor maps to "other" (forward-compatible with the
  // `#[non_exhaustive]` Rust enum) rather than dropping the GPU.
  return GPU_VENDORS.find((v) => v === value) ?? "other";
}

function parseGpu(value: unknown): GpuMetrics | undefined {
  if (!isRecord(value)) {
    return undefined;
  }
  const id = value.id;
  const compute = num(value.compute_util);
  const memUsed = num(value.mem_used_bytes);
  const memTotal = num(value.mem_total_bytes);
  if (
    typeof id !== "string" ||
    compute === undefined ||
    memUsed === undefined ||
    memTotal === undefined
  ) {
    return undefined;
  }
  const gpu: {
    id: string;
    vendor: GpuVendor;
    name?: string;
    compute_util: number;
    mem_used_bytes: number;
    mem_total_bytes: number;
    encoder_util?: number;
    decoder_util?: number;
    encoder_sessions?: number;
    encoder_session_ceiling?: number;
    self_compute_util?: number;
    self_encoder_util?: number;
    self_decoder_util?: number;
    self_mem_used_bytes?: number;
    self_encoder_sessions?: number;
  } = {
    id,
    vendor: vendorFrom(value.vendor),
    compute_util: compute,
    mem_used_bytes: memUsed,
    mem_total_bytes: memTotal,
  };
  if (typeof value.name === "string") {
    gpu.name = value.name;
  }
  const encUtil = num(value.encoder_util);
  if (encUtil !== undefined) {
    gpu.encoder_util = encUtil;
  }
  const decUtil = num(value.decoder_util);
  if (decUtil !== undefined) {
    gpu.decoder_util = decUtil;
  }
  const sessions = num(value.encoder_sessions);
  if (sessions !== undefined) {
    gpu.encoder_sessions = sessions;
  }
  const ceiling = num(value.encoder_session_ceiling);
  if (ceiling !== undefined) {
    gpu.encoder_session_ceiling = ceiling;
  }
  // Our-process share fields: present only when the platform can attribute them;
  // a missing/mistyped value stays undefined (never a false 0) so the UI can
  // omit the "ours" part rather than imply we used nothing.
  const selfCompute = num(value.self_compute_util);
  if (selfCompute !== undefined) {
    gpu.self_compute_util = selfCompute;
  }
  const selfEncUtil = num(value.self_encoder_util);
  if (selfEncUtil !== undefined) {
    gpu.self_encoder_util = selfEncUtil;
  }
  const selfDecUtil = num(value.self_decoder_util);
  if (selfDecUtil !== undefined) {
    gpu.self_decoder_util = selfDecUtil;
  }
  const selfMemUsed = num(value.self_mem_used_bytes);
  if (selfMemUsed !== undefined) {
    gpu.self_mem_used_bytes = selfMemUsed;
  }
  const selfSessions = num(value.self_encoder_sessions);
  if (selfSessions !== undefined) {
    gpu.self_encoder_sessions = selfSessions;
  }
  return gpu;
}

/** Defensively narrow an envelope `data` body to a {@link SystemMetrics}. */
export function parseSystemMetrics(data: unknown): SystemMetrics | undefined {
  if (!isRecord(data)) {
    return undefined;
  }
  const cpu = num(data.cpu_util);
  const sampledHz = num(data.sampled_hz);
  if (cpu === undefined || sampledHz === undefined) {
    return undefined;
  }
  const gpus: GpuMetrics[] = [];
  if (Array.isArray(data.gpus)) {
    for (const raw of data.gpus) {
      const gpu = parseGpu(raw);
      if (gpu !== undefined) {
        gpus.push(gpu);
      }
    }
  }
  const metrics: {
    cpu_util: number;
    self_cpu_util?: number;
    mem_used_bytes?: number;
    self_mem_used_bytes?: number;
    mem_total_bytes?: number;
    gpus: GpuMetrics[];
    program_fps?: number;
    sampled_hz: number;
  } = { cpu_util: cpu, gpus, sampled_hz: sampledHz };
  const selfCpu = num(data.self_cpu_util);
  if (selfCpu !== undefined) {
    metrics.self_cpu_util = selfCpu;
  }
  const memUsed = num(data.mem_used_bytes);
  if (memUsed !== undefined) {
    metrics.mem_used_bytes = memUsed;
  }
  const selfMemUsed = num(data.self_mem_used_bytes);
  if (selfMemUsed !== undefined) {
    metrics.self_mem_used_bytes = selfMemUsed;
  }
  const memTotal = num(data.mem_total_bytes);
  if (memTotal !== undefined) {
    metrics.mem_total_bytes = memTotal;
  }
  const fps = num(data.program_fps);
  if (fps !== undefined) {
    metrics.program_fps = fps;
  }
  return metrics;
}

/** The per-metric series the footer/page sparklines plot (each ≤ capacity). */
export interface SystemMetricsSeries {
  /** Whole-system CPU utilisation, 0..1. */
  readonly cpu: number[];
  /** OUR-process CPU share, 0..1; `undefined` per tick where not attributed. */
  readonly selfCpu: (number | undefined)[];
  /** First-GPU compute utilisation, 0..1 (0 when no GPU is present). */
  readonly gpu0Util: number[];
  /** First-GPU VRAM used fraction, 0..1 (0 when no GPU is present). */
  readonly vram: number[];
  /** First-GPU active NVENC sessions (0 when not reported). */
  readonly nvenc: number[];
  /** First-GPU decoder (NVDEC/QSV) utilisation, 0..1 (0 when not reported). */
  readonly dec: number[];
  /** Aggregate program output rate (fps; 0 when not running). */
  readonly fps: number[];
}

/**
 * The per-GPU sparkline series, keyed by stable device id, so the footer/page
 * can plot EVERY GPU (not just the first). Each array is the same length as the
 * retained ring and aligned in arrival order. `total` channels carry the
 * device-wide value (0 where the GPU was absent that tick, so the series stays
 * aligned); `self*` channels carry OUR-process share and hold `undefined` for
 * any tick the platform did not attribute it (never a false 0).
 */
export interface GpuSeries {
  /** Stable device identity this series belongs to. */
  readonly id: string;
  /** Device-wide compute utilisation, 0..1 (0 where the GPU was absent). */
  readonly compute: number[];
  /** OUR-process compute share, 0..1; `undefined` where not attributed. */
  readonly selfCompute: (number | undefined)[];
  /** Device-wide VRAM used fraction, 0..1 (0 where the GPU was absent). */
  readonly vram: number[];
  /** OUR-process VRAM fraction of total, 0..1; `undefined` where not attributed. */
  readonly selfVram: (number | undefined)[];
  /** Device-wide active NVENC sessions (0 where absent/unreported). */
  readonly nvenc: number[];
  /** OUR-process NVENC sessions; `undefined` where not attributed. */
  readonly selfNvenc: (number | undefined)[];
  /** Device-wide decoder utilisation, 0..1 (0 where absent/unreported). */
  readonly dec: number[];
  /** OUR-process decoder share, 0..1; `undefined` where not attributed. */
  readonly selfDec: (number | undefined)[];
}

/** A point-in-time view of the ring: the latest sample + the parallel series. */
export interface SystemMetricsSnapshot {
  /** The most recent sample (conflated), or `undefined` before the first. */
  readonly current: SystemMetrics | undefined;
  /** The retained series, oldest first. */
  readonly series: SystemMetricsSeries;
  /** One sparkline series per GPU ever seen in the ring, in first-seen order. */
  readonly gpuSeries: GpuSeries[];
}

function vramFraction(gpu: GpuMetrics | undefined): number {
  if (gpu === undefined || gpu.mem_total_bytes <= 0) {
    return 0;
  }
  return gpu.mem_used_bytes / gpu.mem_total_bytes;
}

/** OUR-process VRAM as a fraction of the device total, or `undefined` if absent. */
function selfVramFraction(gpu: GpuMetrics | undefined): number | undefined {
  const selfUsed = gpu?.self_mem_used_bytes;
  if (gpu === undefined || selfUsed === undefined || gpu.mem_total_bytes <= 0) {
    return undefined;
  }
  return selfUsed / gpu.mem_total_bytes;
}

/**
 * The stable, first-seen-ordered list of GPU ids across every retained sample,
 * so a per-GPU series exists even for a GPU that blinks out for a tick.
 */
function gpuIdsAcross(samples: readonly SystemMetrics[]): string[] {
  const seen = new Set<string>();
  const ids: string[] = [];
  for (const s of samples) {
    for (const gpu of s.gpus) {
      if (!seen.has(gpu.id)) {
        seen.add(gpu.id);
        ids.push(gpu.id);
      }
    }
  }
  return ids;
}

/** Build one aligned {@link GpuSeries} for `id` across the retained `samples`. */
function gpuSeriesFor(id: string, samples: readonly SystemMetrics[]): GpuSeries {
  const at = (s: SystemMetrics): GpuMetrics | undefined =>
    s.gpus.find((g) => g.id === id);
  return {
    id,
    // Device-wide channels: 0 for a tick the GPU was absent so series stay aligned.
    compute: samples.map((s) => at(s)?.compute_util ?? 0),
    vram: samples.map((s) => vramFraction(at(s))),
    nvenc: samples.map((s) => at(s)?.encoder_sessions ?? 0),
    dec: samples.map((s) => at(s)?.decoder_util ?? 0),
    // Our-share channels: undefined where unattributed/absent (never a false 0).
    selfCompute: samples.map((s) => at(s)?.self_compute_util),
    selfVram: samples.map((s) => selfVramFraction(at(s))),
    selfNvenc: samples.map((s) => at(s)?.self_encoder_sessions),
    selfDec: samples.map((s) => at(s)?.self_decoder_util),
  };
}

/**
 * A bounded ring buffer of {@link SystemMetrics} samples. Pushing past
 * {@link METRICS_RING_CAPACITY} drops the oldest sample, so the buffer can never
 * grow without bound. {@link snapshot} returns the latest sample plus the
 * derived per-metric series (in arrival order) the sparklines plot.
 */
export class SystemMetricsRing {
  readonly #capacity: number;
  #samples: SystemMetrics[] = [];

  constructor(capacity: number = METRICS_RING_CAPACITY) {
    this.#capacity = capacity > 0 ? capacity : METRICS_RING_CAPACITY;
  }

  /** Append a sample, dropping the oldest if at capacity. */
  push(sample: SystemMetrics): void {
    const next = this.#samples.concat(sample);
    this.#samples =
      next.length > this.#capacity ? next.slice(next.length - this.#capacity) : next;
  }

  /** The current latest-sample + per-metric series view. */
  snapshot(): SystemMetricsSnapshot {
    const samples = this.#samples;
    const series: SystemMetricsSeries = {
      cpu: samples.map((s) => s.cpu_util),
      selfCpu: samples.map((s) => s.self_cpu_util),
      gpu0Util: samples.map((s) => s.gpus[0]?.compute_util ?? 0),
      vram: samples.map((s) => vramFraction(s.gpus[0])),
      nvenc: samples.map((s) => s.gpus[0]?.encoder_sessions ?? 0),
      dec: samples.map((s) => s.gpus[0]?.decoder_util ?? 0),
      fps: samples.map((s) => s.program_fps ?? 0),
    };
    const gpuSeries = gpuIdsAcross(samples).map((id) =>
      gpuSeriesFor(id, samples),
    );
    return { current: samples[samples.length - 1], series, gpuSeries };
  }
}

function resolveWsUrl(): string {
  // Same-origin: the dev proxy and the embedded build both serve `/api/v1/ws`.
  const { protocol, host } = window.location;
  const wsProtocol = protocol === "https:" ? "wss:" : "ws:";
  const base = `${wsProtocol}//${host}/api/v1/ws`;
  // A browser WebSocket cannot send an Authorization header; the control plane
  // also accepts the bearer token as an `access_token` query parameter.
  const token = getStoredToken();
  return token === undefined
    ? base
    : `${base}?access_token=${encodeURIComponent(token)}`;
}

/** What {@link useSystemMetrics} returns. */
export interface SystemMetricsState extends SystemMetricsSnapshot {
  /** Coarse realtime connection status (drives the footer's live dot). */
  readonly status: RealtimeStatus;
}

/**
 * Subscribe to the engine's `system` metrics topic and keep a bounded rolling
 * history for the status footer + System page. Returns the latest sample, the
 * per-metric series, and the connection status. The hook never blocks render:
 * the socket lives in the effect, and samples land via `setState` from the
 * (cheap, non-blocking) frame callback.
 */
export function useSystemMetrics(): SystemMetricsState {
  const [status, setStatus] = useState<RealtimeStatus>("connecting");
  const [snapshot, setSnapshot] = useState<SystemMetricsSnapshot>({
    current: undefined,
    series: {
      cpu: [],
      selfCpu: [],
      gpu0Util: [],
      vram: [],
      nvenc: [],
      dec: [],
      fps: [],
    },
    gpuSeries: [],
  });

  useEffect(() => {
    const ring = new SystemMetricsRing();
    const connection: RealtimeConnection = new RealtimeConnection(resolveWsUrl(), {
      onStatus: (next): void => {
        setStatus(next);
      },
      onEnvelope: (envelope): void => {
        // Only fold our topic's metric samples; everything else is ignored.
        if (envelope.t !== "system.metrics") {
          return;
        }
        const metrics = parseSystemMetrics(envelope.data);
        if (metrics === undefined) {
          return;
        }
        ring.push(metrics);
        setSnapshot(ring.snapshot());
      },
    });
    connection.start();
    return (): void => {
      connection.stop();
    };
  }, []);

  return {
    status,
    current: snapshot.current,
    series: snapshot.series,
    gpuSeries: snapshot.gpuSeries,
  };
}
