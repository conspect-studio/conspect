// Pure helpers of the browser cast flow: the LOAD request for a cast stream,
// the bounded wait for the stream to be playable, and the CAF error sentences.
import { t } from '@lingui/core/macro';

import { getCastStream } from '../api/castStreams';
import type { CastStream } from '../api/castStreams';
import type { CastError, ChromeCastLoadRequest } from './castTypes';

/** How long a new stream may take to become playable. */
export const READY_TIMEOUT_MS = 15_000;
/** Poll cadence while waiting for `ready`. */
export const READY_POLL_MS = 500;
/**
 * Google's Default Media Receiver starts slowly on a young playlist: wait for
 * this many video segments before loading it (the Conspect receiver only needs
 * `ready`).
 */
export const DEFAULT_RECEIVER_MIN_SEGMENTS = 8;

/** Thrown when a stream does not become playable within {@link READY_TIMEOUT_MS}. */
export class StreamNotReadyError extends Error {
  constructor() {
    super('The cast stream did not become ready.');
    this.name = 'StreamNotReadyError';
  }
}

/** Whether `stream` can be loaded on the given receiver. */
export function streamPlayable(stream: CastStream, customReceiver: boolean): boolean {
  return stream.ready && (customReceiver || stream.segments >= DEFAULT_RECEIVER_MIN_SEGMENTS);
}

const sleep = (ms: number): Promise<void> =>
  new Promise((resolve) => {
    setTimeout(resolve, ms);
  });

/**
 * Poll `GET /cast/streams/{id}` until it is playable (bounded). `alive()`
 * false stops the wait early (the start was invalidated); the result is then
 * the last doc seen.
 */
export async function waitPlayable(
  created: CastStream,
  customReceiver: boolean,
  alive: () => boolean,
  now: () => number = Date.now,
  get: (id: string) => Promise<CastStream> = getCastStream,
): Promise<CastStream> {
  const deadline = now() + READY_TIMEOUT_MS;
  let stream = created;
  while (!streamPlayable(stream, customReceiver)) {
    if (!alive()) return stream;
    if (now() >= deadline) throw new StreamNotReadyError();
    await sleep(READY_POLL_MS);
    if (!alive()) return stream;
    const fresh = await get(created.id);
    // The owner's GET carries `path` too; keep the create answer's if not.
    stream = { ...fresh, path: fresh.path ?? stream.path ?? null };
  }
  return stream;
}

/** The absolute stream URL a TV fetches: this page's origin + the token path. */
export function streamUrl(path: string): string {
  return `${window.location.origin}${path}`;
}

/** The LOAD for a cast stream (fields per the stream doc; LIVE; fMP4). */
export function buildLoadRequest(stream: CastStream, title: string): ChromeCastLoadRequest {
  const chromeCast = window.chrome?.cast;
  if (chromeCast === undefined) {
    throw new Error('chrome.cast is unavailable.');
  }
  if (stream.path === undefined || stream.path === null || stream.path === '') {
    throw new Error('The cast stream has no path.');
  }
  const url = streamUrl(stream.path);
  const media = new chromeCast.media.MediaInfo(url, stream.content_type);
  media.contentId = url;
  media.contentUrl = url;
  media.streamType = chromeCast.media.StreamType.LIVE;
  // The doc names the formats in lower case (`fmp4`), as CAF's enum values are.
  media.hlsSegmentFormat = stream.hls_segment_format;
  media.hlsVideoSegmentFormat = stream.hls_video_segment_format;
  media.customData = { title };
  const metadata = new chromeCast.media.GenericMediaMetadata();
  metadata.metadataType = chromeCast.media.MetadataType.GENERIC;
  metadata.title = title;
  media.metadata = metadata;
  return new chromeCast.media.LoadRequest(media);
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null;
}

function isCastErrorLike(value: unknown): value is CastError {
  return isRecord(value) && typeof value.code === 'string';
}

/**
 * The CAF sender's promises (`requestSession`, `loadMedia`) reject with a bare
 * `chrome.cast.ErrorCode` string (e.g. `"cancel"`); the older callback API
 * passes a `chrome.cast.Error` object. Normalise both to the code.
 */
export function errorCode(error: unknown): string | null {
  if (typeof error === 'string') return error;
  return isCastErrorLike(error) ? error.code : null;
}

export function isUserCancellation(error: unknown): boolean {
  return errorCode(error) === 'cancel';
}

/** A human-actionable sentence for a failed connect/load. */
export function describeCastError(error: unknown): string {
  if (error instanceof StreamNotReadyError) {
    return t`The engine did not have the cast stream ready within 15 seconds. Try again; if it keeps failing, check the cast output on the Outputs page.`;
  }
  const code = errorCode(error);
  if (code !== null) {
    switch (code) {
      case 'session_error':
        return t`Cast session error.`;
      case 'timeout':
        return t`The Cast request timed out.`;
      case 'receiver_unavailable':
        return t`No Cast receiver is available on this network.`;
      case 'load_media_failed':
        return t`The Cast receiver could not load the media.`;
      case 'media_error':
        return t`The Cast receiver could not play this stream. Check that the address uses HTTPS with a valid certificate.`;
      default:
        return (isCastErrorLike(error) ? error.description : undefined) ?? t`Could not cast to this device.`;
    }
  }
  return error instanceof Error ? error.message : t`Could not cast to this device.`;
}
