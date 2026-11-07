import { describe, expect, it } from 'vitest';

import index from '../test/fixtures/iptv-org/index.json';
import placesFile from '../test/fixtures/iptv-org/places.json';
import type { IndexStream, Place } from '../api/catalogue';
import {
  NO_FILTERS,
  buildEntries,
  buildPlaceIndex,
  buildTextIndex,
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

describe('locality tiers (Kansas City MO)', () => {
  const kc = places.byId.get('ct/USMKC');
  if (kc === undefined) {
    throw new Error('fixture');
  }
  const tierFor = (title: string, radius: number): string | null => {
    const e = entries.find((x) => x.s.title === title);
    return e === undefined ? 'missing' : tierOf(e, kc, radius, places);
  };
  it('ranks city → nearby → subdivision → country', () => {
    expect(tierFor('KSHB 41 (Kansas City MO)', 150)).toBe('city');
    expect(tierFor('KCKS-LD 25.3', 25)).toBe('nearby');
    expect(tierFor('PBS Sedalia British Columbia MO (KMOS)', 150)).toBe('nearby');
    expect(tierFor('AccuWeather Now', 150)).toBe('country');
  });
  it('radius decides nearby vs not (Sedalia ~125 km, Wichita ~287 km)', () => {
    const sedalia = haversineKm(39.0997, -94.5786, 38.7045, -93.2283);
    expect(sedalia).toBeGreaterThan(100);
    expect(sedalia).toBeLessThan(150);
    expect(tierFor('PBS Sedalia British Columbia MO (KMOS)', 150)).toBe('nearby');
    // Out of radius it widens to the state tier (same subdivision) instead of vanishing.
    expect(tierFor('PBS Sedalia British Columbia MO (KMOS)', 50)).toBe('subdivision');
    const wichita = haversineKm(39.0997, -94.5786, 37.6872, -97.3301);
    expect(wichita).toBeGreaterThan(250);
    expect(wichita).toBeLessThan(300);
    expect(tierFor('WPS TV', 150)).toBeNull();
    expect(tierFor('WPS TV', 300)).toBe('nearby');
  });
  it('a place with no local stations still gets state and national tiers', () => {
    const tampa = places.byId.get('ct/USTPA');
    if (tampa === undefined) {
      throw new Error('fixture');
    }
    const tiers = entries.map((e) => tierOf(e, tampa, 150, places));
    expect(tiers).not.toContain('city');
    expect(tiers).not.toContain('nearby');
    expect(tiers).toContain('subdivision'); // TBCN is a Florida stream
    expect(tiers).toContain('country');
  });
  it('groups by feed, leading with the stream title; channel (network) is secondary', () => {
    const charlotte = places.byId.get('ct/USCLT');
    if (charlotte === undefined) {
      throw new Error('fixture');
    }
    const list = entries
      .map((e) => ({ e, tier: tierOf(e, charlotte, 150, places) }))
      .filter((x) => x.tier !== null);
    const groups = groupEntries(list);
    const wsoc = groups.find((g) => g.feedName === 'WSOC-TV');
    expect(wsoc?.streams).toHaveLength(3);
    expect(wsoc?.streams[0]?.s.q).toBe('1080p'); // best quality first
    expect(wsoc?.name).toBe('WSOC-DT1');
    expect(wsoc?.channelName).toBe('ABC');
    expect(groups[0]?.tier).toBe('city');
  });
});

describe('text search indexes feed names and stream titles', () => {
  const idx = buildTextIndex(streams);
  it('finds KSHB (a feed of the NBC channel) by feed name and by title', () => {
    const hit = streams.find((s) => s.title.startsWith('KSHB 41'));
    expect(hit?.feed_name).toBe('KSHB-TV');
    expect(idx.search('KSHB-TV').map((h) => h.id)).toContain(hit?.k);
    expect(idx.search('kshb 41').map((h) => h.id)).toContain(hit?.k);
    expect(idx.search('KSHBTV').map((h) => h.id)).toContain(hit?.k);
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
  it('maps a stream with headers to an hls body with origin and playlist_metadata', () => {
    const s = streams.find((x) => x.ch === 'AXNBlack.us');
    if (s === undefined) {
      throw new Error('fixture');
    }
    expect(isAddable(s)).toBe(true);
    const body = sourceFormToBody(
      toSourceForm(s, new Set(), 'notify', new Date('2026-11-07T05:00:00Z'), new Map([['movies', 'Movies']])),
    );
    expect(body).toMatchObject({
      id: 'axnblack-us-romania',
      kind: 'hls',
      url: s.url,
      user_agent: s.ua,
      referrer: s.ref,
      playlist_metadata: { tvg_id: 'AXNBlack.us@Romania', name: s.title },
      origin: {
        provider: 'iptv-org',
        channel: 'AXNBlack.us',
        feed: 'Romania',
        stream_url: s.url,
        linked_at: '2026-11-07T05:00:00Z',
        follow: 'notify',
      },
    });
    const group = (body.playlist_metadata as { group?: string }).group;
    expect(group).toBe(s.cats[0] === 'movies' ? 'Movies' : s.cats[0]);
  });
  it('names a local station by its title, not the network', () => {
    const s = streams.find((x) => x.title.startsWith('KSHB 41'));
    if (s === undefined) {
      throw new Error('fixture');
    }
    const body = sourceFormToBody(toSourceForm(s, new Set(), 'notify', new Date(), new Map([['general', 'General']])));
    expect(body).toMatchObject({
      display_name: 'KSHB 41 (Kansas City MO)',
      playlist_metadata: { tvg_id: 'NBC.us@KSHBTV', group: 'General' },
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
