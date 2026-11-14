// Pure "paste a link or address" detector for the New source dialog (#419).
//
// An operator pasting a URL/path should not have to know which `Kind` select
// option it belongs to. This module maps a pasted string onto a transport
// kind + the kind's locator field + a suggested id/name, WITHOUT touching
// React, Lingui, or any network call. It is intentionally conservative:
// anything it cannot confidently classify returns `undefined` (no detection),
// leaving the full manual form exactly as it was.
//
// YouTube note (ADR-0015 / issue #423): there is NO server endpoint to resolve
// a YouTube URL's title. The suggested name is built from the video id / channel
// handle alone — never invented metadata.
import type { SourceFormKind } from './forms';

/** The result of successfully detecting a pasted link/address. */
export interface DetectedSource {
  /** The source kind this input belongs to. */
  readonly kind: SourceFormKind;
  /**
   * The locator value to place in the kind's primary field: `url` for the URL
   * kinds (rtsp/hls/youtube/ts/srt/rist/rtmp), the filesystem `path` for `file`.
   * `file` never has a url-shaped locator.
   */
  readonly locator: string;
  /** Which form field the locator belongs in. */
  readonly locatorField: 'url' | 'path';
  /** A suggested, slug-safe source id (collision-free against `existingIds`). */
  readonly suggestedId: string;
  /** A suggested human-readable display name. */
  readonly suggestedName: string;
}

/** A URL `scheme` to source `kind` mapping for the plain transport schemes. */
const SCHEME_KIND: ReadonlyMap<string, SourceFormKind> = new Map([
  ['rtsp', 'rtsp'],
  ['rtsps', 'rtsp'],
  ['srt', 'srt'],
  ['rist', 'rist'],
  ['rtmp', 'rtmp'],
  ['rtmps', 'rtmp'],
  ['udp', 'ts'],
  ['rtp', 'ts'],
]);

const YOUTUBE_HOSTS: ReadonlySet<string> = new Set([
  'youtube.com',
  'www.youtube.com',
  'm.youtube.com',
]);

/** Slugify into a lowercase, id-safe token: `[a-z0-9]` runs joined by `-`. */
function slugify(value: string): string {
  const slug = value
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, '-')
    .replace(/^-+|-+$/g, '');
  return slug === '' ? 'source' : slug;
}

/** Append `-2`, `-3`, … until `candidate` does not collide with `existingIds`. */
function dedupeId(candidate: string, existingIds: ReadonlySet<string>): string {
  if (!existingIds.has(candidate)) {
    return candidate;
  }
  let n = 2;
  while (existingIds.has(`${candidate}-${String(n)}`)) {
    n += 1;
  }
  return `${candidate}-${String(n)}`;
}

/**
 * Extract a YouTube video id from a `watch`/`live`/`youtu.be` URL, or
 * `undefined` when the path shape is not a recognized video locator. Channel
 * live URLs (`/@handle/live`) have no video id — the handle is returned via
 * `handle` instead so the caller can still build an id/name.
 */
function parseYoutubeLocator(
  url: URL,
): { readonly videoId: string } | { readonly handle: string } | undefined {
  const path = url.pathname;
  if (url.hostname === 'youtu.be') {
    const id = path.replace(/^\/+/, '').split('/')[0];
    return id !== undefined && id !== '' ? { videoId: id } : undefined;
  }
  if (!YOUTUBE_HOSTS.has(url.hostname)) {
    return undefined;
  }
  if (path === '/watch') {
    const v = url.searchParams.get('v');
    return v !== null && v !== '' ? { videoId: v } : undefined;
  }
  const liveMatch = /^\/live\/([^/]+)\/?$/.exec(path);
  if (liveMatch?.[1] !== undefined) {
    return { videoId: liveMatch[1] };
  }
  const handleLiveMatch = /^\/(@[^/]+)\/live\/?$/.exec(path);
  if (handleLiveMatch?.[1] !== undefined) {
    return { handle: handleLiveMatch[1] };
  }
  return undefined;
}

/** Whether `path` (ignoring query/fragment, already stripped by `URL`) ends in `.m3u8`. */
function isM3u8Path(pathname: string): boolean {
  return /\.m3u8$/i.test(pathname);
}

/**
 * Detect the source kind + locator + suggested id/name for a pasted link or
 * address. Returns `undefined` when `input` (after trimming) does not match
 * any recognized shape — the caller then leaves the full manual form as is.
 *
 * `existingIds` lets the caller avoid id collisions on the current page
 * (appends `-2`, `-3`, …).
 */
export function detectSourceKind(
  input: string,
  existingIds: ReadonlySet<string> = new Set(),
): DetectedSource | undefined {
  const trimmed = input.trim();
  if (trimmed === '') {
    return undefined;
  }

  // Absolute filesystem path (never URL-parsed: a bare `/...` is not a URL).
  if (trimmed.startsWith('/')) {
    const base = trimmed.split('/').filter((p) => p !== '').pop() ?? 'file';
    const id = dedupeId(slugify(base), existingIds);
    return {
      kind: 'file',
      locator: trimmed,
      locatorField: 'path',
      suggestedId: id,
      suggestedName: base,
    };
  }

  let url: URL;
  try {
    url = new URL(trimmed);
  } catch {
    return undefined;
  }
  const scheme = url.protocol.endsWith(':') ? url.protocol.slice(0, -1).toLowerCase() : url.protocol.toLowerCase();

  if (scheme === 'file') {
    const path = url.pathname;
    const base = path.split('/').filter((p) => p !== '').pop() ?? 'file';
    const id = dedupeId(slugify(base), existingIds);
    return {
      kind: 'file',
      locator: path,
      locatorField: 'path',
      suggestedId: id,
      suggestedName: base,
    };
  }

  if (scheme === 'http' || scheme === 'https') {
    // YouTube is checked before the generic .m3u8 rule (a YouTube URL is
    // never itself an .m3u8 playlist, but check order still matters for clarity).
    if (YOUTUBE_HOSTS.has(url.hostname) || url.hostname === 'youtu.be') {
      const locator = parseYoutubeLocator(url);
      if (locator !== undefined) {
        if ('videoId' in locator) {
          const lowerId = locator.videoId.toLowerCase();
          const id = dedupeId(slugify(`youtube-${lowerId}`), existingIds);
          return {
            kind: 'youtube',
            locator: trimmed,
            locatorField: 'url',
            suggestedId: id,
            suggestedName: `YouTube ${locator.videoId}`,
          };
        }
        const handle = locator.handle.replace(/^@/, '');
        const id = dedupeId(slugify(`youtube-${handle}`), existingIds);
        return {
          kind: 'youtube',
          locator: trimmed,
          locatorField: 'url',
          suggestedId: id,
          suggestedName: `YouTube @${handle} live`,
        };
      }
      // A youtube.com URL with no recognized video/live/channel shape: no
      // confident detection (e.g. the bare homepage, a playlist URL).
      return undefined;
    }
    if (isM3u8Path(url.pathname)) {
      const id = dedupeId(slugify(`hls-${url.hostname}`), existingIds);
      return {
        kind: 'hls',
        locator: trimmed,
        locatorField: 'url',
        suggestedId: id,
        suggestedName: url.hostname,
      };
    }
    return undefined;
  }

  const mapped = SCHEME_KIND.get(scheme);
  if (mapped !== undefined) {
    const id = dedupeId(slugify(`${mapped}-${url.hostname !== '' ? url.hostname : 'source'}`), existingIds);
    return {
      kind: mapped,
      locator: trimmed,
      locatorField: 'url',
      suggestedId: id,
      suggestedName: url.hostname !== '' ? url.hostname : mapped,
    };
  }

  return undefined;
}
