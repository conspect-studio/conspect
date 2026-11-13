// Output feed routing (ADR-0108 slice 1): plan / take an output's video feed
// and read the routing state back.
//   * `GET  /api/v1/routing` — per-output feed, tally and live readback.
//   * `POST /api/v1/routing/plan` with `target.kind = "output"` — the cost
//     (+0/+1 encoder, sessions only for whole-system principals) and typed
//     refusals, without applying.
//   * `POST /api/v1/routing/output/take` — `202 {operation_id, status, adopt?}`; the
//     terminal outcome arrives as the realtime `route.output` event.
//   * Audio (ADR-0108 slice 2): `target.stream = "audio"` with `audio` and no
//     `feed` plans / takes the output's audio feed alone (video untouched).
import { apiUrl, buildHeaders, OperationApiError, readProblem, sendCommand } from './operations';
import type { RequestOptions } from './operations';
import type { components } from './types';

/** An output video feed (externally tagged, never untagged). */
export type OutputVideoFeed =
  | { readonly bus: 'program' | 'preview' | 'clean' }
  | { readonly source: string }
  | { readonly layout: string };

/** An output audio feed (externally tagged like the config `audio`). */
export type OutputAudioFeed =
  | 'follow'
  | 'program'
  | 'none'
  | { readonly source: string }
  | { readonly tracks: readonly string[] };

/** One output's routing row (feeds are `{redacted: true}` when hidden). */
export type OutputRouteState = components['schemas']['OutputRouteState'];
/** The `POST /routing/plan` `200` body: a crosspoint plan or an output plan. */
export type RoutingPlanResponse = components['schemas']['RoutingPlanResponseDoc'];
/** The plan body for an output target (the output arm of the response). */
export type OutputPlan = Extract<RoutingPlanResponse, { readonly cost: unknown }>;
/** The take's `202` body. */
export type OutputTakeAccepted = components['schemas']['OutputTakeAccepted'];

/** The `GET /routing` body. */
export interface RoutingStateBody {
  readonly outputs: readonly OutputRouteState[];
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null;
}

function isRoutingState(value: unknown): value is RoutingStateBody {
  return (
    isRecord(value) &&
    Array.isArray(value.outputs) &&
    value.outputs.every(
      (row: unknown) => isRecord(row) && typeof row.output === 'string' && typeof row.tally === 'string',
    )
  );
}

// Narrows the generated plan response union to its output arm.
function isOutputPlan(value: unknown): value is OutputPlan {
  return (
    isRecord(value) &&
    typeof value.class === 'string' &&
    isRecord(value.cost) &&
    typeof value.cost.new_encoders === 'number' &&
    typeof value.cost.fits === 'boolean' &&
    Array.isArray(value.refusals) &&
    value.refusals.every(
      (r: unknown) => isRecord(r) && typeof r.type === 'string' && typeof r.detail === 'string',
    )
  );
}

function isTakeAccepted(value: unknown): value is OutputTakeAccepted {
  // A replay of a reserved key carries `status` but never `adopt`.
  return isRecord(value) && typeof value.operation_id === 'string' && typeof value.status === 'string';
}

function body(output: string, feed: OutputVideoFeed): Record<string, unknown> {
  return { target: { kind: 'output', output, stream: 'video' }, feed };
}

function audioBody(output: string, audio: OutputAudioFeed): Record<string, unknown> {
  return { target: { kind: 'output', output, stream: 'audio' }, audio };
}

/** Read the routing state (`GET /api/v1/routing`). */
export async function getRoutingState(options: RequestOptions = {}): Promise<RoutingStateBody> {
  const response = await fetch(apiUrl(options, '/api/v1/routing'), {
    headers: buildHeaders(options, false),
  });
  if (!response.ok) throw await readProblem(response);
  const json: unknown = await response.json();
  if (!isRoutingState(json)) {
    throw new OperationApiError('The server returned an unexpected routing state.');
  }
  return json;
}

/** Plan an output feed change (dry run). */
export async function planOutputFeed(
  output: string,
  feed: OutputVideoFeed,
  options: RequestOptions = {},
): Promise<OutputPlan> {
  const response = await fetch(apiUrl(options, '/api/v1/routing/plan'), {
    method: 'POST',
    headers: buildHeaders(options, true),
    body: JSON.stringify(body(output, feed)),
  });
  if (!response.ok) throw await readProblem(response);
  const json: unknown = await response.json();
  if (!isOutputPlan(json)) {
    throw new OperationApiError('The server returned an unexpected output plan.');
  }
  return json;
}

/** Take an output feed; resolves to the accepted body (not the outcome). */
export async function takeOutputFeed(
  output: string,
  feed: OutputVideoFeed,
  options: RequestOptions = {},
): Promise<OutputTakeAccepted> {
  const response = await sendCommand('/api/v1/routing/output/take', options, {
    body: { ...body(output, feed), audio: 'follow' },
  });
  const json: unknown = await response.json();
  if (!isTakeAccepted(json)) {
    throw new OperationApiError('The server returned an unexpected take result.');
  }
  return json;
}

/** Plan an output audio feed change (dry run; `stream: audio`). */
export async function planOutputAudio(
  output: string,
  audio: OutputAudioFeed,
  options: RequestOptions = {},
): Promise<OutputPlan> {
  const response = await fetch(apiUrl(options, '/api/v1/routing/plan'), {
    method: 'POST',
    headers: buildHeaders(options, true),
    body: JSON.stringify(audioBody(output, audio)),
  });
  if (!response.ok) throw await readProblem(response);
  const json: unknown = await response.json();
  if (!isOutputPlan(json)) {
    throw new OperationApiError('The server returned an unexpected output plan.');
  }
  return json;
}

/** Take an output audio feed alone; resolves to the accepted body. */
export async function takeOutputAudio(
  output: string,
  audio: OutputAudioFeed,
  options: RequestOptions = {},
): Promise<OutputTakeAccepted> {
  const response = await sendCommand('/api/v1/routing/output/take', options, {
    body: audioBody(output, audio),
  });
  const json: unknown = await response.json();
  if (!isTakeAccepted(json)) {
    throw new OperationApiError('The server returned an unexpected take result.');
  }
  return json;
}

/** The audio feed a routing row's `audio` carries, or `undefined` if hidden/unknown. */
export function audioOf(value: unknown): OutputAudioFeed | undefined {
  if (value === 'follow' || value === 'program' || value === 'none') return value;
  if (!isRecord(value)) return undefined;
  if (typeof value.source === 'string') return { source: value.source };
  if (Array.isArray(value.tracks) && value.tracks.every((t: unknown) => typeof t === 'string')) {
    return { tracks: value.tracks };
  }
  return undefined;
}

/** The select value of an audio feed (`follow`, `program`, `none`, `src:<id>`, `track:<name>`). */
export function audioKey(audio: OutputAudioFeed): string {
  if (typeof audio === 'string') return audio;
  if ('source' in audio) return `src:${audio.source}`;
  return `track:${audio.tracks.join(',')}`;
}

/** Parse an audio select value back into a feed. */
export function audioFromKey(key: string): OutputAudioFeed | undefined {
  if (key === 'follow' || key === 'program' || key === 'none') return key;
  if (key.startsWith('src:') && key.length > 4) return { source: key.slice(4) };
  if (key.startsWith('track:') && key.length > 6) return { tracks: [key.slice(6)] };
  return undefined;
}

/** Whether a feed value is the redaction marker. */
export function isRedacted(value: unknown): boolean {
  return isRecord(value) && value.redacted === true;
}

/** The feed a routing row's `video` carries, or `undefined` if hidden/unknown. */
export function feedOf(value: unknown): OutputVideoFeed | undefined {
  if (!isRecord(value)) return undefined;
  if (value.bus === 'program' || value.bus === 'preview' || value.bus === 'clean') {
    return { bus: value.bus };
  }
  if (typeof value.source === 'string') return { source: value.source };
  if (typeof value.layout === 'string') return { layout: value.layout };
  return undefined;
}

/** The select value of a feed (`bus:program`, `src:<id>`, `aux:<layout>`). */
export function feedKey(feed: OutputVideoFeed): string {
  if ('bus' in feed) return `bus:${feed.bus}`;
  if ('layout' in feed) return `aux:${feed.layout}`;
  return `src:${feed.source}`;
}

/** Parse a select value back into a feed. */
export function feedFromKey(key: string): OutputVideoFeed | undefined {
  if (key === 'bus:program') return { bus: 'program' };
  if (key === 'bus:preview') return { bus: 'preview' };
  if (key === 'bus:clean') return { bus: 'clean' };
  if (key.startsWith('src:') && key.length > 4) return { source: key.slice(4) };
  if (key.startsWith('aux:') && key.length > 4) return { layout: key.slice(4) };
  return undefined;
}
