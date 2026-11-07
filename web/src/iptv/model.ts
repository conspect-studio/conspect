// Pure catalogue logic for the IPTV-ORG browser: footprints, locality tiers,
// facets, text + place search, grouping. No React, no I/O — unit-testable.
import MiniSearch from 'minisearch';

import type { IndexStream, Place, StreamFlag } from '../api/catalogue';

/** Locality tiers, nearest first (docs/io/iptv-org.md §4.1). */
export const TIERS = ['city', 'nearby', 'subdivision', 'country', 'region'] as const;
export type Tier = (typeof TIERS)[number];

/** A stream with its resolved geographic footprint. */
export interface Entry {
  readonly s: IndexStream;
  /** `ct/…` ids named in the broadcast area. */
  readonly cityIds: readonly string[];
  /** `s/…` ids (exact area entries). */
  readonly subAreaIds: readonly string[];
  /** `c/…` ids (exact area entries). */
  readonly countryAreaIds: readonly string[];
  /** Region codes (`NAM`) from `r/…` entries. */
  readonly regionCodes: readonly string[];
  /** Every subdivision touched: from `s/…` and from the cities' subdivisions. */
  readonly allSubs: readonly string[];
  /** Every country touched: `c/…`, `s/…`, `ct/…` and the channel country. */
  readonly allCountries: readonly string[];
}

/** The place gazetteer, indexed. */
export interface PlaceIndex {
  readonly byId: ReadonlyMap<string, Place>;
  readonly cities: readonly Place[];
  readonly search: MiniSearch<PlaceDoc>;
}

interface PlaceDoc {
  readonly id: string;
  readonly name: string;
  readonly alt: string;
  readonly context: string;
}

/** Great-circle distance in kilometres (haversine). */
export function haversineKm(lat1: number, lon1: number, lat2: number, lon2: number): number {
  const rad = Math.PI / 180;
  const dLat = (lat2 - lat1) * rad;
  const dLon = (lon2 - lon1) * rad;
  const a =
    Math.sin(dLat / 2) ** 2 +
    Math.cos(lat1 * rad) * Math.cos(lat2 * rad) * Math.sin(dLon / 2) ** 2;
  return 6371.0088 * 2 * Math.asin(Math.min(1, Math.sqrt(a)));
}

/** Build the place index (typo-tolerant search over names + alt names + context). */
export function buildPlaceIndex(places: readonly Place[]): PlaceIndex {
  const byId = new Map(places.map((p) => [p.id, p]));
  const subName = new Map(
    places.filter((p) => p.kind === 'subdivision').map((p) => [p.subdivision ?? '', p.name]),
  );
  const countryName = new Map(
    places.filter((p) => p.kind === 'country').map((p) => [p.country ?? '', p.name]),
  );
  const search = new MiniSearch<PlaceDoc>({
    fields: ['name', 'alt', 'context'],
    storeFields: [],
    idField: 'id',
    searchOptions: {
      boost: { name: 4, alt: 2, context: 0.5 },
      fuzzy: 0.25,
      prefix: true,
      combineWith: 'OR',
    },
    tokenize: (text) => text.split(/[\s,./()-]+/u).filter((x) => x !== ''),
    processTerm: (term) =>
      term
        .normalize('NFD')
        .replace(/\p{M}/gu, '')
        .toLowerCase(),
  });
  search.addAll(
    places.map((p) => {
      const sub = p.subdivision ?? '';
      const subCode = sub.includes('-') ? sub.split('-')[1] : sub;
      const context = [
        subName.get(sub) ?? '',
        subCode ?? '',
        countryName.get(p.country ?? '') ?? '',
        p.country ?? '',
      ].join(' ');
      return { id: p.id, name: p.name, alt: p.alt.join(' '), context };
    }),
  );
  return { byId, cities: places.filter((p) => p.kind === 'city' && p.lat !== null), search };
}

const KIND_RANK: Record<Place['kind'], number> = { city: 0, subdivision: 1, country: 2, region: 3 };

/** Typo-tolerant place search; name matches outrank context-only matches. */
export function searchPlaces(index: PlaceIndex, query: string, limit = 8): Place[] {
  const q = query.trim();
  if (q === '') {
    return [];
  }
  const hits = index.search.search(q);
  const norm = q.toLowerCase();
  const scored = hits
    .map((hit) => {
      const place = index.byId.get(String(hit.id));
      if (place === undefined) {
        return undefined;
      }
      const exact = place.name.toLowerCase() === norm.split(/[\s,]+/u)[0] ? 1 : 0;
      return { place, score: hit.score + exact * 10 };
    })
    .filter((x): x is { place: Place; score: number } => x !== undefined);
  scored.sort(
    (a, b) =>
      b.score - a.score ||
      KIND_RANK[a.place.kind] - KIND_RANK[b.place.kind] ||
      a.place.name.localeCompare(b.place.name),
  );
  return scored.slice(0, limit).map((x) => x.place);
}

/** Resolve every stream's footprint against the gazetteer. */
export function buildEntries(streams: readonly IndexStream[], places: PlaceIndex): Entry[] {
  return streams.map((s) => {
    const cityIds: string[] = [];
    const subAreaIds: string[] = [];
    const countryAreaIds: string[] = [];
    const regionCodes: string[] = [];
    const subs = new Set<string>();
    const countries = new Set<string>();
    if (s.country !== null) {
      countries.add(s.country);
    }
    for (const a of s.area) {
      const slash = a.indexOf('/');
      const kind = a.slice(0, slash);
      const code = a.slice(slash + 1);
      if (kind === 'ct') {
        cityIds.push(a);
        const city = places.byId.get(a);
        if (city?.subdivision) {
          subs.add(city.subdivision);
        }
        if (city?.country) {
          countries.add(city.country);
        }
      } else if (kind === 's') {
        subAreaIds.push(a);
        subs.add(code);
        countries.add(code.split('-')[0] ?? code);
      } else if (kind === 'c') {
        countryAreaIds.push(a);
        countries.add(code);
      } else if (kind === 'r') {
        regionCodes.push(code);
      }
    }
    return {
      s,
      cityIds,
      subAreaIds,
      countryAreaIds,
      regionCodes,
      allSubs: [...subs],
      allCountries: [...countries],
    };
  });
}

/** A synthetic locality (e.g. “near this server”). */
export function pointPlace(name: string, lat: number, lon: number): Place {
  return {
    id: 'pt/server',
    kind: 'city',
    name,
    alt: [],
    country: null,
    subdivision: null,
    lat,
    lon,
    flag: null,
    members: [],
  };
}

/** The best locality tier of an entry for the selected place (null = no match). */
export function tierOf(
  e: Entry,
  place: Place,
  radiusKm: number,
  places: PlaceIndex,
): Tier | null {
  switch (place.kind) {
    case 'city': {
      if (e.cityIds.includes(place.id)) {
        return 'city';
      }
      if (place.lat !== null && place.lon !== null) {
        for (const id of e.cityIds) {
          const c = places.byId.get(id);
          if (c?.lat != null && c.lon != null) {
            if (haversineKm(place.lat, place.lon, c.lat, c.lon) <= radiusKm) {
              return 'nearby';
            }
          }
        }
      }
      // State tier: state-wide streams AND local streams of other cities in the
      // same state (iptv-org coverage is thin; the state is the honest fallback).
      if (place.subdivision !== null && e.allSubs.includes(place.subdivision)) {
        return 'subdivision';
      }
      if (place.country !== null && e.countryAreaIds.includes(`c/${place.country}`)) {
        return 'country';
      }
      return inRegion(e, place.country, places) ? 'region' : null;
    }
    case 'subdivision': {
      if (place.subdivision !== null && e.allSubs.includes(place.subdivision)) {
        return 'subdivision';
      }
      if (place.country !== null && e.countryAreaIds.includes(`c/${place.country}`)) {
        return 'country';
      }
      return inRegion(e, place.country, places) ? 'region' : null;
    }
    case 'country': {
      if (place.country !== null && e.allCountries.includes(place.country)) {
        return 'country';
      }
      return inRegion(e, place.country, places) ? 'region' : null;
    }
    case 'region': {
      const code = place.id.slice(2);
      if (e.regionCodes.includes(code) || e.allCountries.some((c) => place.members.includes(c))) {
        return 'region';
      }
      return null;
    }
  }
}

function inRegion(e: Entry, country: string | null, places: PlaceIndex): boolean {
  if (country === null) {
    return false;
  }
  return e.regionCodes.some((code) => places.byId.get(`r/${code}`)?.members.includes(country));
}

/** The user-selected filters. */
export interface Filters {
  readonly q: string;
  readonly cats: ReadonlySet<string>;
  readonly langs: ReadonlySet<string>;
  readonly quals: ReadonlySet<string>;
  readonly protos: ReadonlySet<string>;
  readonly flags: ReadonlySet<StreamFlag>;
}

/** An empty filter set. */
export const NO_FILTERS: Filters = {
  q: '',
  cats: new Set(),
  langs: new Set(),
  quals: new Set(),
  protos: new Set(),
  flags: new Set(),
};

/** The facets with multi-select values. */
export type FacetKey = 'cats' | 'langs' | 'quals' | 'protos' | 'flags';

/** Flags of an entry including the client-derived `already_added`. */
export function flagsOf(s: IndexStream, added: ReadonlySet<string>): readonly StreamFlag[] {
  return added.has(s.url) && !s.flags.includes('already_added')
    ? [...s.flags, 'already_added']
    : s.flags;
}

function anyOf<T>(selected: ReadonlySet<T>, values: readonly T[]): boolean {
  return selected.size === 0 || values.some((v) => selected.has(v));
}

/** Whether an entry passes the facet filters (optionally skipping one facet). */
export function passes(
  e: Entry,
  f: Filters,
  added: ReadonlySet<string>,
  textHits: ReadonlySet<string> | null,
  skip?: FacetKey,
): boolean {
  const s = e.s;
  const flags = flagsOf(s, added);
  // Blocklisted streams are hidden unless the operator explicitly asks for them.
  if (flags.includes('blocklisted') && !f.flags.has('blocklisted')) {
    return false;
  }
  if (textHits !== null && !textHits.has(s.k)) {
    return false;
  }
  if (skip !== 'cats' && !anyOf(f.cats, s.cats)) {
    return false;
  }
  if (skip !== 'langs' && !anyOf(f.langs, s.langs)) {
    return false;
  }
  if (skip !== 'quals' && !anyOf(f.quals, s.q === null ? [] : [s.q])) {
    return false;
  }
  if (skip !== 'protos' && !anyOf(f.protos, [s.proto])) {
    return false;
  }
  if (skip !== 'flags' && !anyOf(f.flags, flags)) {
    return false;
  }
  return true;
}

/** One facet option with its live count. */
export interface FacetOption {
  readonly value: string;
  readonly count: number;
}

/** Live counts for a facet, computed with every OTHER facet applied. */
export function facetCounts(
  entries: readonly Entry[],
  f: Filters,
  added: ReadonlySet<string>,
  textHits: ReadonlySet<string> | null,
  facet: FacetKey,
  inScope: (e: Entry) => boolean,
): FacetOption[] {
  const counts = new Map<string, number>();
  for (const e of entries) {
    if (!inScope(e) || !passes(e, f, added, textHits, facet)) {
      continue;
    }
    const values: readonly string[] =
      facet === 'cats'
        ? e.s.cats
        : facet === 'langs'
          ? e.s.langs
          : facet === 'quals'
            ? e.s.q === null
              ? []
              : [e.s.q]
            : facet === 'protos'
              ? [e.s.proto]
              : flagsOf(e.s, added);
    for (const v of values) {
      counts.set(v, (counts.get(v) ?? 0) + 1);
    }
  }
  // Keep selected options visible even at zero.
  for (const v of f[facet]) {
    if (!counts.has(v)) {
      counts.set(v, 0);
    }
  }
  return [...counts.entries()]
    .map(([value, count]) => ({ value, count }))
    .sort((a, b) => b.count - a.count || a.value.localeCompare(b.value));
}

/** Numeric quality for ordering (`1080p` → 1080, `480i` → 480). */
export function qualityRank(q: string | null): number {
  const n = q === null ? NaN : Number.parseInt(q, 10);
  return Number.isFinite(n) ? n : 0;
}

/** Build the full-text index over name, alt names, network and stream title. */
export function buildTextIndex(streams: readonly IndexStream[]): MiniSearch<{
  k: string;
  name: string;
  alt: string;
  net: string;
  title: string;
  feed: string;
}> {
  const ms = new MiniSearch<{ k: string; name: string; alt: string; net: string; title: string; feed: string }>({
    fields: ['name', 'alt', 'net', 'title', 'feed'],
    idField: 'k',
    searchOptions: { boost: { title: 3, feed: 3, name: 2, alt: 2 }, fuzzy: 0.2, prefix: true, combineWith: 'AND' },
    processTerm: (term) =>
      term
        .normalize('NFD')
        .replace(/\p{M}/gu, '')
        .toLowerCase(),
  });
  ms.addAll(
    streams.map((s) => ({
      k: s.k,
      name: s.name ?? '',
      alt: s.alt.join(' '),
      net: s.net ?? '',
      title: s.title,
      feed: [s.feed_name ?? '', s.feed ?? ''].join(' '),
    })),
  );
  return ms;
}

/** A channel's streams within one tier. */
export interface Group {
  readonly key: string;
  readonly tier: Tier | null;
  /** Leading label: the best stream's title (local stations are feeds of networks). */
  readonly name: string;
  /** Channel name (e.g. the network “NBC”), when it differs from the lead. */
  readonly channelName: string | null;
  /** Feed name (e.g. “KSHB-TV”), when present. */
  readonly feedName: string | null;
  readonly streams: readonly Entry[];
}

/** Group matching entries by channel feed (one local station) inside their best tier. */
export function groupEntries(
  list: readonly { readonly e: Entry; readonly tier: Tier | null }[],
): Group[] {
  const groups = new Map<string, { tier: Tier | null; streams: Entry[] }>();
  for (const { e, tier } of list) {
    const id = e.s.ch === null ? e.s.k : `${e.s.ch}@${e.s.feed ?? ''}`;
    const key = `${tier ?? '-'}|${id}`;
    const g = groups.get(key);
    if (g === undefined) {
      groups.set(key, { tier, streams: [e] });
    } else {
      g.streams.push(e);
    }
  }
  return [...groups.entries()]
    .map(([key, g]) => {
      const streams = g.streams.sort((a, b) => qualityRank(b.s.q) - qualityRank(a.s.q));
      const lead = streams[0]?.s;
      const name = lead?.title ?? '';
      const channelName = lead?.name != null && lead.name !== name ? lead.name : null;
      return { key, tier: g.tier, name, channelName, feedName: lead?.feed_name ?? null, streams };
    })
    .sort((a, b) => {
      const ta = a.tier === null ? 0 : TIERS.indexOf(a.tier);
      const tb = b.tier === null ? 0 : TIERS.indexOf(b.tier);
      return ta - tb || a.name.localeCompare(b.name);
    });
}

/** Flag emoji for a country code (regional-indicator pair). */
export function flagEmoji(country: string | null): string {
  if (country === null || !/^[A-Za-z]{2}$/.test(country)) {
    return '';
  }
  const up = country.toUpperCase();
  return String.fromCodePoint(
    0x1f1e6 + up.charCodeAt(0) - 65,
    0x1f1e6 + up.charCodeAt(1) - 65,
  );
}
