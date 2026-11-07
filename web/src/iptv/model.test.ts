import { describe, expect, it } from 'vitest';

import index from '../test/fixtures/iptv-org/index.json';
import placesFile from '../test/fixtures/iptv-org/places.json';
import type { IndexStream, Place } from '../api/catalogue';
import {
  NO_FILTERS,
  buildEntries,
  buildPlaceIndex,
  facetCounts,
  groupEntries,
  haversineKm,
  passes,
  searchPlaces,
  tierOf,
} from './model';
import { isAddable, toSourceForm } from './toSource';
import { sourceFormToBody } from '../resources/forms';

const streams = index.streams as unknown as IndexStream[];
const places = buildPlaceIndex(placesFile.places as unknown as Place[]);
const entries = buildEntries(streams, places);

describe('place search', () => {
  it('finds Wichita first, typo-tolerant, local before Wichita Falls', () => {
    expect(searchPlaces(places, 'wichita')[0]?.id).toBe('ct/USICT');
    expect(searchPlaces(places, 'Wichta, KS')[0]?.id).toBe('ct/USICT');
    const ids = searchPlaces(places, 'wichita').map((p) => p.id);
    expect(ids).toContain('ct/USSPS');
    expect(ids.indexOf('ct/USICT')).toBeLessThan(ids.indexOf('ct/USSPS'));
  });
});

describe('locality tiers (Wichita)', () => {
  const wichita = places.byId.get('ct/USICT');
  if (wichita === undefined) {
    throw new Error('fixture');
  }
  const tierFor = (title: string, radius: number): string | null => {
    const e = entries.find((x) => x.s.title === title);
    return e === undefined ? 'missing' : tierOf(e, wichita, radius, places);
  };
  it('ranks city → nearby → subdivision → country', () => {
    expect(tierFor('KAKE 10 News', 150)).toBe('city');
    expect(tierFor('KCKS-LD 25.3', 300)).toBe('nearby');
  });
  it('radius decides nearby vs not', () => {
    const d = haversineKm(37.6872, -97.3301, 39.1142, -94.6275);
    expect(d).toBeGreaterThan(250);
    expect(d).toBeLessThan(300);
    expect(tierFor('KCKS-LD 25.3', 150)).not.toBe('nearby');
    expect(tierFor('KCKS-LD 25.3', 300)).toBe('nearby');
  });
  it('state-wide and national fall into their tiers', () => {
    expect(tierFor('Kansas Statewide TV', 150)).toBe('subdivision');
    expect(tierFor('AccuWeather Now', 150)).toBe('country');
  });
  it('groups by channel inside tiers', () => {
    const list = entries
      .map((e) => ({ e, tier: tierOf(e, wichita, 150, places) }))
      .filter((x) => x.tier !== null);
    const groups = groupEntries(list);
    const kake = groups.find((g) => g.name === 'KAKE');
    expect(kake?.streams).toHaveLength(2);
    expect(kake?.streams[0]?.s.q).toBe('720p'); // best quality first
    expect(groups[0]?.tier).toBe('city');
  });
});

describe('facets', () => {
  const none = new Set<string>();
  it('hides blocklisted by default and counts facets with other facets applied', () => {
    expect(
      entries.filter((e) => passes(e, NO_FILTERS, none, null)).some((e) => e.s.flags.includes('blocklisted')),
    ).toBe(false);
    const f = { ...NO_FILTERS, protos: new Set(['hls']) };
    const cats = facetCounts(entries, f, none, null, 'cats', () => true);
    const news = cats.find((c) => c.value === 'news');
    expect(news?.count).toBeGreaterThan(0);
    const flags = facetCounts(entries, NO_FILTERS, none, null, 'flags', () => true);
    expect(flags.find((c) => c.value === 'geo_blocked')?.count).toBe(
      entries.filter((e) => e.s.flags.includes('geo_blocked') && !e.s.flags.includes('blocklisted')).length,
    );
  });
});

describe('toSourceForm', () => {
  it('maps a stream with headers to an hls body with origin', () => {
    const s = streams.find((x) => x.k === 's:kake000001');
    if (s === undefined) {
      throw new Error('fixture');
    }
    expect(isAddable(s)).toBe(true);
    const body = sourceFormToBody(
      toSourceForm(s, new Set(), 'notify', new Date('2026-11-07T05:00:00Z')),
    );
    expect(body).toMatchObject({
      id: 'kake-us-sd',
      kind: 'hls',
      url: 'https://example.test/kake/index.m3u8',
      user_agent: 'Mozilla/5.0 Test',
      referrer: 'https://example.test/',
      origin: {
        provider: 'iptv-org',
        channel: 'KAKE.us',
        feed: 'SD',
        stream_url: 'https://example.test/kake/index.m3u8',
        linked_at: '2026-11-07T05:00:00Z',
        follow: 'notify',
      },
    });
  });
  it('refuses unsupported protocols and de-dupes ids', () => {
    const mm = streams.find((x) => x.flags.includes('unsupported'));
    expect(mm !== undefined && isAddable(mm)).toBe(false);
    const s = streams[0];
    if (s === undefined) {
      throw new Error('fixture');
    }
    const first = toSourceForm(s, new Set()).id;
    expect(toSourceForm(s, new Set([first])).id).toBe(`${first}-2`);
  });
});
