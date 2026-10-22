// Unit tests for the devices API bindings: record → view projections (typed
// field guards, never `as`-casts), the honest sync-tier fold (weakest member,
// never over-claimed — managed-devices.md §8), the engine-monotonic last-seen
// age math, and the typed action calls (set-mode declared impact, discovery
// scan, projection endpoints) against an MSW double of the control plane.
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';

import {
  driverSyncTier,
  lastSeenAgeSeconds,
  listDiscovered,
  listOutputTargets,
  listSourceCandidates,
  fetchDeviceStatus,
  scanDevices,
  setDeviceMode,
  toDeviceView,
  toSyncGroupView,
  weakestMemberTier,
} from './api';

const server = setupServer();

beforeAll(() => {
  server.listen({ onUnhandledRequest: 'error' });
});
afterEach(() => {
  server.resetHandlers();
});
afterAll(() => {
  server.close();
});

describe('toDeviceView', () => {
  it('projects a stored device record with typed field guards', () => {
    const view = toDeviceView({
      id: 'dev-foyer',
      name: 'Foyer decoder',
      body: {
        id: 'dev-foyer',
        driver: 'zowietek',
        address: 'http://[fd00:db8::42]',
        desired_mode: 'decoder',
      },
    });
    expect(view).toEqual({
      id: 'dev-foyer',
      name: 'Foyer decoder',
      driver: 'zowietek',
      rawDriver: 'zowietek',
      address: 'http://[fd00:db8::42]',
      desiredMode: 'decoder',
      editable: true,
    });
  });

  it('keeps an unknown driver honest: raw tag shown, never editable', () => {
    const view = toDeviceView({
      id: 'dev-x',
      name: 'X',
      body: { id: 'dev-x', driver: 'vendorzz' },
    });
    expect(view.driver).toBe('unknown');
    expect(view.rawDriver).toBe('vendorzz');
    expect(view.editable).toBe(false);
    expect(view.address).toBeUndefined();
  });
});

describe('toSyncGroupView', () => {
  it('projects members and skew with guards', () => {
    const view = toSyncGroupView({
      id: 'wall',
      name: 'Lobby wall',
      body: {
        id: 'wall',
        target_skew_ms: 80,
        members: [
          { device: 'dev-a', offset_ms: 0 },
          { device: 'dev-b', offset_ms: 120 },
        ],
      },
    });
    expect(view.id).toBe('wall');
    expect(view.targetSkewMs).toBe(80);
    expect(view.members).toEqual([
      { device: 'dev-a', offsetMs: 0 },
      { device: 'dev-b', offsetMs: 120 },
    ]);
    expect(view.editable).toBe(true);
  });

  it('drops malformed member rows instead of fabricating them', () => {
    const view = toSyncGroupView({
      id: 'g',
      name: 'G',
      body: {
        id: 'g',
        target_skew_ms: 50,
        members: [{ device: 'dev-a' }, { nope: true }, 'junk'],
      },
    });
    expect(view.members).toEqual([{ device: 'dev-a', offsetMs: 0 }]);
  });
});

describe('sync tiers (honest, weakest member)', () => {
  it('maps drivers onto their published tier', () => {
    expect(driverSyncTier('displaynode')).toBe('frame-accurate');
    expect(driverSyncTier('zowietek')).toBe('bounded-skew');
    expect(driverSyncTier('cast')).toBe('none');
    // Unknown drivers are never over-claimed.
    expect(driverSyncTier('unknown')).toBe('none');
  });

  it('a group claims the weakest member tier, never more', () => {
    expect(weakestMemberTier(['displaynode', 'displaynode'])).toBe('frame-accurate');
    expect(weakestMemberTier(['displaynode', 'zowietek'])).toBe('bounded-skew');
    expect(weakestMemberTier(['zowietek', 'cast'])).toBe('none');
    expect(weakestMemberTier([])).toBe('none');
  });
});

describe('lastSeenAgeSeconds', () => {
  it('computes the age from the engine-monotonic clock reference', () => {
    // Engine ts 100 s (ns) observed at wall 1_000 ms; now it is wall 3_000 ms,
    // so the engine clock reads ~102 s. last_seen at 90 s ⇒ 12 s ago.
    const age = lastSeenAgeSeconds(
      90_000_000_000,
      { engineTs: 100_000_000_000, wallMs: 1_000 },
      3_000,
    );
    expect(age).toBe(12);
  });

  it('returns undefined without a clock reference (never fabricates)', () => {
    expect(lastSeenAgeSeconds(90, undefined, 1_000)).toBeUndefined();
  });
});

describe('typed action calls', () => {
  it('setDeviceMode posts the mode and returns the declared impact', async () => {
    let posted: unknown;
    server.use(
      http.post('*/api/v1/devices/dev-a/set-mode', async ({ request }) => {
        posted = await request.json();
        return HttpResponse.json(
          {
            operation_id: 'op-3',
            impact: 'dev',
            detail: 'device dev-a restarts its pipeline',
          },
          { status: 202 },
        );
      }),
    );
    const accepted = await setDeviceMode('dev-a', 'encoder');
    expect(posted).toEqual({ mode: 'encoder' });
    expect(accepted.operation_id).toBe('op-3');
    expect(accepted.impact).toBe('dev');
    expect(accepted.detail).toContain('restarts');
  });

  it('scanDevices returns the 202 scan envelope (operation id + note)', async () => {
    server.use(
      http.post('*/api/v1/discovery/devices/scan', () =>
        HttpResponse.json(
          {
            operation_id: 'op-scan',
            budget_ms: 4000,
            note: 'untrusted inventory; confirm-adopt only',
            service_types: ['_googlecast._tcp'],
          },
          { status: 202 },
        ),
      ),
    );
    const accepted = await scanDevices();
    expect(accepted.operation_id).toBe('op-scan');
    expect(accepted.note).toContain('untrusted');
    expect(accepted.budget_ms).toBe(4000);
  });

  it('listDiscovered parses the untrusted inventory defensively', async () => {
    server.use(
      http.get('*/api/v1/discovery/devices', () =>
        HttpResponse.json([
          {
            key: 'zowietek-control|box|_zowie._tcp',
            name: 'box',
            host: 'box.local.',
            driver_kind: 'zowietek-control',
            service_type: '_zowie._tcp',
            port: 80,
            primary_address: '[fd00::42]:80',
            endpoints: [{ address: '[fd00::42]:80', family: 'ipv6' }],
            last_seen_unix_ns: 7,
            txt: [],
          },
          { junk: true },
        ]),
      ),
    );
    const rows = await listDiscovered();
    expect(rows).toHaveLength(1);
    const row = rows[0];
    expect(row?.driverKind).toBe('zowietek-control');
    expect(row?.primaryAddress).toBe('[fd00::42]:80');
    expect(row?.endpoints[0]?.family).toBe('ipv6');
  });

  it('listSourceCandidates carries the unverified flag and optional url', async () => {
    server.use(
      http.get('*/api/v1/devices/dev-a/source-candidates', () =>
        HttpResponse.json([
          { id: 'main', kind: 'rtsp', url: 'rtsp://[fd00::1]:554/main', unverified: false },
          { id: 'aux', kind: 'rtsp', url: null, unverified: true },
        ]),
      ),
    );
    const candidates = await listSourceCandidates('dev-a');
    expect(candidates).toEqual([
      { id: 'main', kind: 'rtsp', url: 'rtsp://[fd00::1]:554/main', unverified: false },
      { id: 'aux', kind: 'rtsp', url: undefined, unverified: true },
    ]);
  });

  it('listOutputTargets parses decode slots', async () => {
    server.use(
      http.get('*/api/v1/devices/dev-a/output-targets', () =>
        HttpResponse.json([{ id: 'slot-0', kind: 'rtsp', label: 'Decode slot 0' }]),
      ),
    );
    const targets = await listOutputTargets('dev-a');
    expect(targets).toEqual([{ id: 'slot-0', kind: 'rtsp', label: 'Decode slot 0' }]);
  });

  it('fetchDeviceStatus parses the snapshot and treats 404 as not-adopted', async () => {
    server.use(
      http.get('*/api/v1/devices/dev-a/status', () =>
        HttpResponse.json({
          device_id: 'dev-a',
          state: 'UNREACHABLE',
          mode: 'decoder',
          last_seen_ts: 90,
          temperature_c: 47.5,
        }),
      ),
      http.get('*/api/v1/devices/dev-gone/status', () =>
        HttpResponse.json(
          { title: 'not found', status: 404 },
          { status: 404 },
        ),
      ),
    );
    const status = await fetchDeviceStatus('dev-a');
    expect(status?.state).toBe('UNREACHABLE');
    expect(status?.temperature_c).toBe(47.5);
    expect(await fetchDeviceStatus('dev-gone')).toBeUndefined();
  });
});
