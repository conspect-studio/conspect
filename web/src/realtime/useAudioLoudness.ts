// useAudioLoudness — the React binding over the `audio.loudness` realtime topic
// (AUD-8). The engine pushes a CONFLATED EBU R128 loudness sample (momentary /
// short-term / integrated LUFS, loudness range, true-peak dBTP, plus the
// compliance reference) on the `audio.loudness` topic, drop-oldest (events
// contract: crates/multiview-events/src/event.rs::AudioLoudness). Like
// `useSystemMetrics`, this hook subscribes via a self-reconnecting WebSocket
// whose callbacks are cheap and non-blocking — the engine is isolated
// (invariant #10): a slow UI only loses its own samples and can never
// back-pressure the engine.
//
// Per ADR-R006 the wire carries the RAW measured values; the DISPLAY BALLISTICS
// (momentary fast-attack/slow-decay + true-peak hold) are applied here, on the
// client. The compliance colour zones are also computed here against the
// per-sample reference (target ± tolerance, dBTP ceiling).
import { useEffect, useState } from "react";

import { RealtimeConnection } from "./connection";
import type { RealtimeStatus } from "./connection";
import { mintWsTicket, realtimeWsBaseUrl } from "./ticket";

/** A program-bus EBU R128 loudness sample (mirrors the Rust `AudioLoudness`). */
export interface AudioLoudnessSample {
  /** Program/bus index this sample is for. */
  readonly program: number;
  /** Momentary loudness (400 ms), LUFS. Absent below the absolute gate. */
  readonly momentary?: number;
  /** Short-term loudness (3 s), LUFS. Absent below the absolute gate. */
  readonly short_term?: number;
  /** Integrated (gated) loudness, LUFS. Absent until enough gated audio. */
  readonly integrated?: number;
  /** Loudness range (EBU Tech 3342), LU. Absent until enough gated audio. */
  readonly lra?: number;
  /** Maximum true-peak across channels (4x oversampled), dBTP. Absent if disabled. */
  readonly true_peak_dbtp?: number;
  /** Normalisation target loudness, LUFS (compliance reference; always present). */
  readonly target_lufs: number;
  /** True-peak ceiling, dBTP (compliance reference; always present). */
  readonly ceiling_dbtp: number;
  /** Live convergence tolerance, LU (the in-spec band is target ± tolerance). */
  readonly tolerance_lu: number;
  /** Makeup gain the loudnorm processor is applying, dB. Absent when none. */
  readonly gain_db?: number;
  /** The effective sampling cadence on the wire (Hz). */
  readonly sampled_hz: number;
}

/** The realtime topic the loudness samples ride. */
export const AUDIO_LOUDNESS_TOPIC = "audio.loudness";

/** The wire `t` discriminator for the loudness event. */
export const AUDIO_LOUDNESS_EVENT = "audio.loudness";

/** Narrow an unknown value to a plain record without an unsafe assertion. */
function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

/** Read a finite number, or `undefined` if absent/mistyped (never a false 0). */
function num(value: unknown): number | undefined {
  return typeof value === "number" && Number.isFinite(value) ? value : undefined;
}

/** Defensively narrow an envelope `data` body to an {@link AudioLoudnessSample}. */
export function parseAudioLoudness(data: unknown): AudioLoudnessSample | undefined {
  if (!isRecord(data)) {
    return undefined;
  }
  const program = num(data.program);
  const target = num(data.target_lufs);
  const ceiling = num(data.ceiling_dbtp);
  const tolerance = num(data.tolerance_lu);
  const sampledHz = num(data.sampled_hz);
  // The compliance reference + program index + cadence are always present on the
  // wire; without them the meter cannot colour, so the sample is rejected.
  if (
    program === undefined ||
    target === undefined ||
    ceiling === undefined ||
    tolerance === undefined ||
    sampledHz === undefined
  ) {
    return undefined;
  }
  const out: {
    program: number;
    momentary?: number;
    short_term?: number;
    integrated?: number;
    lra?: number;
    true_peak_dbtp?: number;
    target_lufs: number;
    ceiling_dbtp: number;
    tolerance_lu: number;
    gain_db?: number;
    sampled_hz: number;
  } = {
    program,
    target_lufs: target,
    ceiling_dbtp: ceiling,
    tolerance_lu: tolerance,
    sampled_hz: sampledHz,
  };
  const momentary = num(data.momentary);
  if (momentary !== undefined) {
    out.momentary = momentary;
  }
  const shortTerm = num(data.short_term);
  if (shortTerm !== undefined) {
    out.short_term = shortTerm;
  }
  const integrated = num(data.integrated);
  if (integrated !== undefined) {
    out.integrated = integrated;
  }
  const lra = num(data.lra);
  if (lra !== undefined) {
    out.lra = lra;
  }
  const peak = num(data.true_peak_dbtp);
  if (peak !== undefined) {
    out.true_peak_dbtp = peak;
  }
  const gain = num(data.gain_db);
  if (gain !== undefined) {
    out.gain_db = gain;
  }
  return out;
}

/** A loudness compliance zone (text/glyph carries the meaning; colour aids it). */
export type LoudnessZone = "in-spec" | "near" | "out" | "absent";

/** The compliance classification the meter renders. */
export interface LoudnessClassification {
  /** The loudness zone against the target ± tolerance band. */
  readonly loudnessZone: LoudnessZone;
  /** Whether the measured true-peak is at/over the dBTP ceiling. */
  readonly peakOver: boolean;
}

/**
 * Classify a `loudness` (LUFS) reading against a sample's compliance reference.
 *
 * - within `±tolerance` of `target` → `in-spec` (green);
 * - within `2× tolerance` → `near` (amber, drifting);
 * - beyond → `out` (red);
 * - `undefined` reading (gated silence) → `absent` (neutral).
 *
 * `peakOver` is `true` when the sample's `true_peak_dbtp` is at or above the
 * ceiling (a clip-risk flag — never raised for an absent measurement).
 */
export function classifyLoudness(
  sample: AudioLoudnessSample,
  loudness: number | undefined,
): LoudnessClassification {
  const peakOver =
    sample.true_peak_dbtp !== undefined && sample.true_peak_dbtp >= sample.ceiling_dbtp;
  if (loudness === undefined) {
    return { loudnessZone: "absent", peakOver };
  }
  const delta = Math.abs(loudness - sample.target_lufs);
  const tol = Math.abs(sample.tolerance_lu);
  let loudnessZone: LoudnessZone;
  if (delta <= tol) {
    loudnessZone = "in-spec";
  } else if (delta <= tol * 2) {
    loudnessZone = "near";
  } else {
    loudnessZone = "out";
  }
  return { loudnessZone, peakOver };
}

/**
 * Per-sample decay applied to the displayed momentary value on a quieter reading
 * (slow decay). A fraction of the gap toward the (lower) target is closed each
 * sample, so the bar falls back smoothly; a louder reading is adopted instantly
 * (fast attack). PPM-style ballistics per ADR-R006, done client-side.
 */
export const MOMENTARY_DECAY = 0.3;

/**
 * Client-side display ballistics for the loudness meter (ADR-R006: ballistics
 * applied once, in the client). Tracks the displayed momentary value with a fast
 * attack / slow decay, and holds the maximum true-peak (peak-hold) until reset.
 * The slow meters (short-term / integrated / LRA) are read raw from the latest
 * sample — they are already integrated, so no display ballistics apply.
 */
export class LoudnessBallistics {
  #displayMomentary: number | undefined = undefined;
  #heldPeak: number | undefined = undefined;
  #latest: AudioLoudnessSample | undefined = undefined;
  readonly #decay: number;

  constructor(decay: number = MOMENTARY_DECAY) {
    this.#decay = decay > 0 && decay <= 1 ? decay : MOMENTARY_DECAY;
  }

  /** Fold a fresh sample: advance the momentary ballistics + the peak hold. */
  push(sample: AudioLoudnessSample): void {
    this.#latest = sample;
    const m = sample.momentary;
    if (m !== undefined) {
      const prev = this.#displayMomentary;
      // Fast attack (jump up), slow decay (ease down by a fraction of the gap).
      this.#displayMomentary =
        prev === undefined || m >= prev ? m : prev + (m - prev) * this.#decay;
    }
    const p = sample.true_peak_dbtp;
    if (p !== undefined) {
      this.#heldPeak = this.#heldPeak === undefined ? p : Math.max(this.#heldPeak, p);
    }
  }

  /** The ballistics-shaped momentary value, or `undefined` before any reading. */
  displayMomentary(): number | undefined {
    return this.#displayMomentary;
  }

  /** The held (max) true-peak in dBTP, or `undefined` if none seen since reset. */
  heldPeakDbtp(): number | undefined {
    return this.#heldPeak;
  }

  /** The most recent raw sample (the slow meters read straight from this). */
  latest(): AudioLoudnessSample | undefined {
    return this.#latest;
  }

  /** Clear the peak hold (e.g. an operator "reset peak" action). */
  resetPeak(): void {
    this.#heldPeak = undefined;
  }
}

/** A point-in-time view the meter widget renders. */
export interface AudioLoudnessState {
  /** Coarse realtime connection status. */
  readonly status: RealtimeStatus;
  /** The most recent raw sample (conflated), or `undefined` before the first. */
  readonly current: AudioLoudnessSample | undefined;
  /** The ballistics-shaped momentary value (fast attack / slow decay), LUFS. */
  readonly displayMomentary: number | undefined;
  /** The held (max) true-peak since the meter mounted, dBTP. */
  readonly heldPeakDbtp: number | undefined;
}

/**
 * Subscribe to the engine's `audio.loudness` topic and expose the latest
 * loudness sample plus the client-side ballistics (momentary decay + true-peak
 * hold) the meter renders. Never blocks render: the socket lives in the effect,
 * and samples land via `setState` from the (cheap, non-blocking) frame callback.
 */
export function useAudioLoudness(): AudioLoudnessState {
  const [status, setStatus] = useState<RealtimeStatus>("connecting");
  const [state, setState] = useState<{
    current: AudioLoudnessSample | undefined;
    displayMomentary: number | undefined;
    heldPeakDbtp: number | undefined;
  }>({ current: undefined, displayMomentary: undefined, heldPeakDbtp: undefined });

  useEffect(() => {
    const ballistics = new LoudnessBallistics();
    const connection: RealtimeConnection = new RealtimeConnection(realtimeWsBaseUrl(), {
      onStatus: (next): void => {
        setStatus(next);
      },
      onEnvelope: (envelope): void => {
        // Only fold our topic's loudness samples; everything else is ignored.
        if (envelope.t !== AUDIO_LOUDNESS_EVENT) {
          return;
        }
        const sample = parseAudioLoudness(envelope.data);
        if (sample === undefined) {
          return;
        }
        ballistics.push(sample);
        setState({
          current: sample,
          displayMomentary: ballistics.displayMomentary(),
          heldPeakDbtp: ballistics.heldPeakDbtp(),
        });
      },
    }, mintWsTicket);
    connection.start();
    return (): void => {
      connection.stop();
    };
  }, []);

  return {
    status,
    current: state.current,
    displayMomentary: state.displayMomentary,
    heldPeakDbtp: state.heldPeakDbtp,
  };
}
