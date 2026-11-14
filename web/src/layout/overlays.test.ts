import { describe, expect, it } from 'vitest';
import {
  DEFAULT_LAYOUT_OVERLAYS,
  overlaysFromBody,
  summarizeOverlays,
  withOverlays,
} from './overlays';
import type { LayoutOverlays } from './overlays';

describe('overlaysFromBody', () => {
  it('returns the engine default when the body is not a record', () => {
    expect(overlaysFromBody(undefined)).toEqual(DEFAULT_LAYOUT_OVERLAYS);
    expect(overlaysFromBody(null)).toEqual(DEFAULT_LAYOUT_OVERLAYS);
    expect(overlaysFromBody('nope')).toEqual(DEFAULT_LAYOUT_OVERLAYS);
    expect(overlaysFromBody(42)).toEqual(DEFAULT_LAYOUT_OVERLAYS);
  });

  it('returns the engine default when overlays is absent', () => {
    expect(overlaysFromBody({ canvas: {} })).toEqual(DEFAULT_LAYOUT_OVERLAYS);
  });

  it('returns the engine default when overlays is not an object', () => {
    expect(overlaysFromBody({ overlays: 'nope' })).toEqual(DEFAULT_LAYOUT_OVERLAYS);
    expect(overlaysFromBody({ overlays: null })).toEqual(DEFAULT_LAYOUT_OVERLAYS);
    expect(overlaysFromBody({ overlays: [] })).toEqual(DEFAULT_LAYOUT_OVERLAYS);
  });

  it('reads every present boolean key', () => {
    expect(
      overlaysFromBody({
        overlays: {
          labels: false,
          meters: false,
          flags: false,
          tally: false,
          clock: false,
          safe_area: true,
        },
      }),
    ).toEqual({
      labels: false,
      meters: false,
      flags: false,
      tally: false,
      clock: false,
      safe_area: true,
    });
  });

  it('defaults a missing individual key while honouring the rest', () => {
    expect(overlaysFromBody({ overlays: { meters: false } })).toEqual({
      ...DEFAULT_LAYOUT_OVERLAYS,
      meters: false,
    });
  });

  it('defaults a non-boolean value for a key (tolerant parsing)', () => {
    expect(overlaysFromBody({ overlays: { meters: 'off', clock: 1, flags: null } })).toEqual(
      DEFAULT_LAYOUT_OVERLAYS,
    );
  });

  it('ignores unknown keys inside overlays', () => {
    expect(overlaysFromBody({ overlays: { bogus: true } })).toEqual(DEFAULT_LAYOUT_OVERLAYS);
  });
});

describe('withOverlays', () => {
  it('omits the overlays key when the value equals the default', () => {
    const body = { canvas: { width: 1 }, overlays: { meters: false } };
    const result = withOverlays(body, DEFAULT_LAYOUT_OVERLAYS);
    expect(result).toEqual({ canvas: { width: 1 } });
    expect('overlays' in result).toBe(false);
  });

  it('writes exactly the six keys when non-default', () => {
    const o: LayoutOverlays = { ...DEFAULT_LAYOUT_OVERLAYS, meters: false };
    const result = withOverlays({ canvas: { width: 1 } }, o);
    expect(result.overlays).toEqual({
      labels: true,
      meters: false,
      flags: true,
      tally: true,
      clock: true,
      safe_area: false,
    });
    expect(Object.keys(result.overlays as object).sort()).toEqual(
      ['clock', 'flags', 'labels', 'meters', 'safe_area', 'tally'],
    );
  });

  it('writes safe_area true as a non-default case', () => {
    const o: LayoutOverlays = { ...DEFAULT_LAYOUT_OVERLAYS, safe_area: true };
    const result = withOverlays({}, o);
    expect(result.overlays).toEqual({
      labels: true,
      meters: true,
      flags: true,
      tally: true,
      clock: true,
      safe_area: true,
    });
  });

  it('preserves every other root key verbatim', () => {
    const body = { canvas: { width: 1 }, schema_version: 1, cells: [{ id: 'a' }] };
    const result = withOverlays(body, { ...DEFAULT_LAYOUT_OVERLAYS, clock: false });
    expect(result.canvas).toEqual({ width: 1 });
    expect(result.schema_version).toBe(1);
    expect(result.cells).toEqual([{ id: 'a' }]);
  });

  it('does not mutate the input body', () => {
    const body = { canvas: { width: 1 } };
    withOverlays(body, { ...DEFAULT_LAYOUT_OVERLAYS, clock: false });
    expect(body).toEqual({ canvas: { width: 1 } });
  });

  it('round-trips through overlaysFromBody', () => {
    const o: LayoutOverlays = {
      labels: false,
      meters: true,
      flags: false,
      tally: true,
      clock: false,
      safe_area: true,
    };
    const body = withOverlays({}, o);
    expect(overlaysFromBody(body)).toEqual(o);
  });
});

describe('summarizeOverlays', () => {
  it('reports { kind: "all" } when every layer is on', () => {
    const allOn: LayoutOverlays = {
      labels: true,
      meters: true,
      flags: true,
      tally: true,
      clock: true,
      safe_area: true,
    };
    expect(summarizeOverlays(allOn)).toEqual({ kind: 'all' });
  });

  it('reports { kind: "clean" } when every layer is off', () => {
    const allOff: LayoutOverlays = {
      labels: false,
      meters: false,
      flags: false,
      tally: false,
      clock: false,
      safe_area: false,
    };
    expect(summarizeOverlays(allOff)).toEqual({ kind: 'clean' });
  });

  it('reports the on-count out of six for the engine default (5 on, safe_area off)', () => {
    expect(summarizeOverlays(DEFAULT_LAYOUT_OVERLAYS)).toEqual({ kind: 'count', on: 5, total: 6 });
  });

  it('reports the on-count out of six otherwise', () => {
    expect(summarizeOverlays({ ...DEFAULT_LAYOUT_OVERLAYS, meters: false })).toEqual({
      kind: 'count',
      on: 4,
      total: 6,
    });
    expect(
      summarizeOverlays({ ...DEFAULT_LAYOUT_OVERLAYS, meters: false, flags: false }),
    ).toEqual({ kind: 'count', on: 3, total: 6 });
  });
});
