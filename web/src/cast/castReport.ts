// The Conspect receiver's report (`crates/multiview-control/src/cast_receiver/
// receiver.js#buildReport`), sent on the custom namespace and POSTed to the
// engine (`CastStreamDoc.last_report.report`):
//   {v:1, state: idle|loading|buffering|playing|error,
//    error: {code, detailed, reason, shaka_code, shaka_category, severity, message}|null,
//    stats: {width, height, bitrate, buffer_s, latency_s, decoded, dropped, stalls, current_time}|null,
//    shaka, caf, device, at_ms}
// Everything is untrusted JSON: parse defensively, keep only what the UI shows.
import { t } from '@lingui/core/macro';

/** The custom message namespace (also served as `namespace` by `/cast/config`). */
export const CAST_NAMESPACE = 'urn:x-cast:studio.conspect.cast';

export type ReceiverState = 'idle' | 'loading' | 'buffering' | 'playing' | 'error';

export interface ReceiverError {
  readonly code: string | null;
  readonly detailed: string | null;
  readonly reason: string | null;
  readonly shakaCode: string | null;
  readonly message: string | null;
}

export interface ReceiverReport {
  readonly state: ReceiverState;
  readonly error: ReceiverError | null;
  readonly width: number | null;
  readonly height: number | null;
  readonly latencyS: number | null;
}

const STATES: readonly ReceiverState[] = ['idle', 'loading', 'buffering', 'playing', 'error'];

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function finite(value: unknown): number | null {
  return typeof value === 'number' && Number.isFinite(value) ? value : null;
}

function text(value: unknown): string | null {
  if (typeof value === 'string' && value !== '') return value.slice(0, 200);
  if (typeof value === 'number' && Number.isFinite(value)) return String(value);
  return null;
}

/** Parse a report object (or its JSON text); `null` when it is not one. */
export function parseReceiverReport(raw: unknown): ReceiverReport | null {
  let value = raw;
  if (typeof value === 'string') {
    try {
      value = JSON.parse(value) as unknown;
    } catch {
      return null;
    }
  }
  if (!isRecord(value)) return null;
  const state = STATES.find((s) => s === value.state);
  if (state === undefined) return null;
  const err = isRecord(value.error) ? value.error : null;
  const stats = isRecord(value.stats) ? value.stats : null;
  return {
    state,
    error:
      err === null
        ? null
        : {
            code: text(err.code),
            detailed: text(err.detailed),
            reason: text(err.reason),
            shakaCode: text(err.shaka_code),
            message: text(err.message),
          },
    width: stats === null ? null : finite(stats.width),
    height: stats === null ? null : finite(stats.height),
    latencyS: stats === null ? null : finite(stats.latency_s),
  };
}

/** The receiver's error as an operator sentence (its own message when it sent one). */
export function receiverErrorSentence(error: ReceiverError | null): string {
  if (error?.message !== null && error?.message !== undefined) return error.message;
  return t`The TV could not play the stream.`;
}

/** The receiver's error codes on one line (`code 905 · detailed 905 · shaka 1001`), or `null`. */
export function receiverErrorCodes(error: ReceiverError | null): string | null {
  if (error === null) return null;
  const parts: string[] = [];
  if (error.code !== null) parts.push(`code ${error.code}`);
  if (error.detailed !== null && error.detailed !== error.code) parts.push(`detailed ${error.detailed}`);
  if (error.shakaCode !== null) parts.push(`shaka ${error.shakaCode}`);
  if (error.reason !== null) parts.push(error.reason);
  return parts.length === 0 ? null : parts.join(' · ');
}
