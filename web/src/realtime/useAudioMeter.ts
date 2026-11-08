// useAudioMeter — React binding over the `audio.meter` realtime event (AUD-8 /
// SW-14). The engine pushes a CONFLATED (~30 Hz, drop-oldest) per-channel
// peak/RMS dBFS sample for the program bus (events contract:
// crates/multiview-events/src/event.rs::AudioMeter). Like `useAudioLoudness`
// the socket callbacks are cheap and non-blocking; a slow UI only loses its own
// samples and can never back-pressure the engine (invariant #10).
import { useEffect, useState } from "react";

import { RealtimeConnection } from "./connection";
import type { RealtimeStatus } from "./connection";
import { mintWsTicket, realtimeWsBaseUrl } from "./ticket";

/** A program-bus peak/RMS sample (mirrors the Rust `AudioMeter`). */
export interface AudioMeterSample {
  /** Track index this sample is for (0 = program bus). */
  readonly track: number;
  /** Per-channel peak level, dBFS. */
  readonly peak_db: readonly number[];
  /** Per-channel RMS level, dBFS. */
  readonly rms_db: readonly number[];
  /** Source/input id for a per-input sample (track >= 1); absent on the program bus. */
  readonly input_id?: string;
  /** Whether any channel clipped in this window. */
  readonly clip: boolean;
  /** Whether the meter pipeline dropped windows. */
  readonly overflow: boolean;
  /** Effective wire cadence, Hz. */
  readonly sampled_hz: number;
}

/** The wire `t` discriminator for the meter event. */
export const AUDIO_METER_EVENT = "audio.meter";

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

function finiteList(value: unknown): number[] | undefined {
  if (!Array.isArray(value)) {
    return undefined;
  }
  const out: number[] = [];
  for (const v of value as unknown[]) {
    if (typeof v !== "number" || !Number.isFinite(v)) {
      return undefined;
    }
    out.push(v);
  }
  return out;
}

/** Defensively narrow an envelope `data` body to an {@link AudioMeterSample}. */
export function parseAudioMeter(data: unknown): AudioMeterSample | undefined {
  if (!isRecord(data)) {
    return undefined;
  }
  const peak = finiteList(data.peak_db);
  const rms = finiteList(data.rms_db);
  const { track, sampled_hz: hz } = data;
  if (
    peak === undefined ||
    rms === undefined ||
    typeof track !== "number" ||
    typeof hz !== "number"
  ) {
    return undefined;
  }
  const inputId = typeof data.input_id === "string" ? data.input_id : undefined;
  return {
    track,
    ...(inputId === undefined ? {} : { input_id: inputId }),
    peak_db: peak,
    rms_db: rms,
    clip: data.clip === true,
    overflow: data.overflow === true,
    sampled_hz: hz,
  };
}

/** Bottom of the displayed scale, dBFS. */
const FLOOR_DB = -60;

/** Map a dBFS level onto a 0..100 bar fill over the -60..0 dBFS scale. */
export function meterFillPercent(db: number): number {
  const clamped = Math.min(0, Math.max(FLOOR_DB, db));
  return ((clamped - FLOOR_DB) / -FLOOR_DB) * 100;
}

/** What the meter widget renders. */
export interface AudioMeterState {
  readonly status: RealtimeStatus;
  /** Latest program-bus (track 0) sample, or `undefined` before the first. */
  readonly current: AudioMeterSample | undefined;
  /** Latest sample per input track (ordered by track), excluding the program bus. */
  readonly inputs: readonly AudioMeterSample[];
}

/** Subscribe to `audio.meter` and expose the latest program-bus sample. */
export function useAudioMeter(): AudioMeterState {
  const [status, setStatus] = useState<RealtimeStatus>("connecting");
  const [current, setCurrent] = useState<AudioMeterSample | undefined>(undefined);
  const [inputs, setInputs] = useState<readonly AudioMeterSample[]>([]);

  useEffect(() => {
    const connection: RealtimeConnection = new RealtimeConnection(
      realtimeWsBaseUrl(),
      {
        onStatus: (next): void => {
          setStatus(next);
        },
        onEnvelope: (envelope): void => {
          if (envelope.t !== AUDIO_METER_EVENT) {
            return;
          }
          const sample = parseAudioMeter(envelope.data);
          if (sample === undefined) {
            return;
          }
          if (sample.track === 0) {
            setCurrent(sample);
          } else {
            setInputs((prev) =>
              [...prev.filter((p) => p.track !== sample.track), sample].sort(
                (a, b) => a.track - b.track,
              ),
            );
          }
        },
      },
      mintWsTicket,
    );
    connection.start();
    return (): void => {
      connection.stop();
    };
  }, []);

  return { status, current, inputs };
}
