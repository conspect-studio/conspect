// Map an IPTV-ORG stream onto the ordinary source form (ONE code path for
// “Add as source” and “Bulk add”): the same SourceFormState → sourceFormToBody.
import type { IndexStream } from '../api/catalogue';
import { emptySourceForm } from '../resources/forms';
import type { OriginFollow, SourceFormKind, SourceFormState } from '../resources/forms';

/** The protocols the engine can ingest, mapped to a source kind. */
export function sourceKindFor(s: IndexStream): SourceFormKind | undefined {
  switch (s.proto) {
    case 'hls':
      return 'hls';
    case 'rtmp':
      return 'rtmp';
    case 'srt':
      return 'srt';
    case 'rtsp':
      return 'rtsp';
    default:
      return undefined;
  }
}

/** Whether a stream may be added (supported protocol, not closed/blocklisted). */
export function isAddable(s: IndexStream): boolean {
  return (
    sourceKindFor(s) !== undefined &&
    !s.flags.includes('unsupported') &&
    !s.flags.includes('blocklisted')
  );
}

/** Upstream label strings reconstructed from the flag vocabulary. */
export function labelsOf(s: IndexStream): string[] {
  const labels: string[] = [];
  if (s.flags.includes('geo_blocked')) {
    labels.push('Geo-blocked');
  }
  if (s.flags.includes('not_24_7')) {
    labels.push('Not 24/7');
  }
  return labels;
}

/** A DNS-label-safe slug for the source id. */
export function slugId(s: IndexStream): string {
  const base = (s.ch ?? s.title)
    .toLowerCase()
    .normalize('NFD')
    .replace(/\p{M}/gu, '')
    .replace(/[^a-z0-9]+/g, '-')
    .replace(/^-+|-+$/g, '')
    .slice(0, 48);
  const feed = s.feed !== null ? `-${s.feed.toLowerCase().replace(/[^a-z0-9]+/g, '')}` : '';
  return `${base === '' ? 'iptv' : base}${feed}`;
}

/** Choose an id that is not already taken. */
export function uniqueId(base: string, taken: ReadonlySet<string>): string {
  if (!taken.has(base)) {
    return base;
  }
  for (let i = 2; i < 1000; i += 1) {
    if (!taken.has(`${base}-${String(i)}`)) {
      return `${base}-${String(i)}`;
    }
  }
  return `${base}-${String(Date.now())}`;
}

/** Build the prefilled source form for a stream. */
export function toSourceForm(
  s: IndexStream,
  taken: ReadonlySet<string>,
  follow: OriginFollow = 'notify',
  now: Date = new Date(),
  catNames: ReadonlyMap<string, string> = new Map(),
): SourceFormState {
  const kind = sourceKindFor(s) ?? 'hls';
  return {
    ...emptySourceForm(),
    id: uniqueId(slugId(s), taken),
    // Lead with the station (stream title), not the network channel name.
    name: s.title,
    kind,
    url: s.url,
    userAgent: kind === 'hls' ? (s.ua ?? '') : '',
    referrer: kind === 'hls' ? (s.ref ?? '') : '',
    // rc3 `playlist_metadata` (epg-playlist): tvg_id = channel@feed, name, logo, group.
    metaTvgId: s.ch !== null ? `${s.ch}@${s.feed ?? ''}`.replace(/@$/, '') : '',
    metaName: s.title,
    metaLogo: s.logo_url ?? '',
    metaGroup: s.cats[0] !== undefined ? (catNames.get(s.cats[0]) ?? s.cats[0]) : '',
    origin: {
      provider: 'iptv-org',
      ...(s.ch !== null ? { channel: s.ch } : {}),
      ...(s.feed !== null ? { feed: s.feed } : {}),
      stream_url: s.url,
      labels: labelsOf(s),
      linked_at: now.toISOString().replace(/\.\d{3}Z$/, 'Z'),
      follow,
    },
  };
}
