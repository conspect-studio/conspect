import { afterAll, afterEach, beforeAll, describe, expect, it, vi } from 'vitest';

vi.mock('../../realtime/shared', () => ({ subscribeRealtime: () => () => undefined }));
import { act, render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { i18n } from '@lingui/core';
import { I18nProvider } from '@lingui/react';
import { MemoryRouter } from 'react-router-dom';
import { messages } from '../../locales/en/messages';
import { CastProvider } from '../../cast';
import { ProductionDesk } from './ProductionDesk';
vi.mock('../../realtime/useEngineEvents', () => ({ useEngineEvents: () => ({ status: 'open' }), TILES_QUERY_KEY: ['tiles'] }));
vi.mock('../../preview/PreviewSurface', () => ({ PreviewSurface: () => null }));
vi.mock('../../preview/useJpegPreview', () => ({ useJpegPreview: () => undefined, useJpegStill: () => ({ url: undefined, state: 'loading' }) }));
import { getOperationTracker } from './operationTracker';
import type { Envelope } from '../../realtime/envelope';

i18n.load('en', messages);
i18n.activate('en');

const state = { program_source: 'cam-1', preview_source: 'cam-2', tick: 7, in_flight: false };
let commandCount = 0;
let polls = 0;
let frozenTick = false;
const server = setupServer(
  http.get('*/api/v1/switcher/me1', () => { polls += 1; return HttpResponse.json({ ...state, tick: frozenTick ? state.tick : state.tick + polls }); }),
  http.get('*/api/v1/layouts', () => HttpResponse.json([])),
  http.get('*/api/v1/alarms', () => HttpResponse.json([])),
  http.get('*/api/v1/preview/capabilities', () => HttpResponse.json({ webrtc: false })),
  http.get('*/api/v1/sources', () => HttpResponse.json([])),
  http.post('*/api/v1/switcher/plan', () => HttpResponse.json({ can_take: true, class: 'cut', scene_version: 1 })),
  http.post('*/api/v1/switcher/commands/cut', () => {
    commandCount += 1;
    return HttpResponse.json({ operation_id: `op-${String(commandCount)}`, kind: 'cut' }, { status: 202 });
  }),
);
beforeAll(() => { server.listen(); });
afterEach(() => { server.resetHandlers(); commandCount = 0; polls = 0; frozenTick = false; vi.useRealTimers(); });
afterAll(() => { server.close(); });

function page() {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false }, mutations: { retry: false } } });
  const view = render(<QueryClientProvider client={client}><I18nProvider i18n={i18n}><MemoryRouter><CastProvider><ProductionDesk /></CastProvider></MemoryRouter></I18nProvider></QueryClientProvider>);
  return { client, ...view };
}
function frame(id: string, data: unknown, override: Partial<Envelope> = {}): Envelope {
  return { v: 1, t: 'switcher.operation', topic: 'switcher', id: 'me1', seq: 1, ts: 1, corr: id, data, ...override };
}
function outcome(id: string, completed: boolean, error?: string) {
  return { operation_id: id, me: 'me1', program: 'main', tick: 8, completed, ...(error ? { error } : {}) };
}

// Tracker events are supplied as the shell-owned socket supplies them; no second socket.
// Confirmation needs two polls with an advancing tick (2 s refetch), hence the longer waits.
describe('ProductionDesk single-authority completion', { timeout: 15000 }, () => {
  it('keeps unsupported transitions disabled before the dominant CUT in DOM order; AUTO works', async () => {
    const { container } = page();
    await waitFor(() => { expect(screen.getByRole('button', { name: 'CUT' })).not.toBeDisabled(); }, { timeout: 6000 });
    const keys = [...container.querySelectorAll('.bd-takes button')];
    expect(keys.map((key) => key.textContent)).toEqual(['MIX', 'WIPE', 'DIP', 'AUTO', 'CUT · cam-2']);
    for (const key of keys.slice(0, 3)) expect(key).toBeDisabled();
    expect(screen.getByRole('button', { name: 'MIX' })).toHaveAttribute('aria-pressed', 'true');
    expect(screen.getByRole('button', { name: 'AUTO' })).not.toBeDisabled();
    expect(screen.getByRole('button', { name: 'AUTO' }).title).toContain('30 frames');
    expect(screen.getByRole('spinbutton', { name: 'Auto frames' })).not.toBeDisabled();
    expect(commandCount).toBe(0);
  });

  it('AUTO sends duration_frames from the rate field, after a passing plan', async () => {
    let body: unknown;
    server.use(http.post('*/api/v1/switcher/commands/auto', async ({ request }) => { body = await request.json(); commandCount += 1; return HttpResponse.json({ operation_id: 'op-auto', kind: 'auto' }, { status: 202 }); }));
    page();
    const auto = screen.getByRole('button', { name: 'AUTO' });
    await waitFor(() => { expect(auto).not.toBeDisabled(); }, { timeout: 6000 });
    await userEvent.click(auto);
    // The take lock is on by default: AUTO confirms once, like CUT.
    await userEvent.click(await screen.findByRole('button', { name: 'Confirm take' }));
    await waitFor(() => { expect(commandCount).toBe(1); });
    expect(body).toMatchObject({ me: 'me1', program: 'main', action: 'auto', duration_frames: 30 });
  });

  it('disables every key the engine would refuse, with the reason', async () => {
    server.use(
      http.get('*/api/v1/sources', () => HttpResponse.json([{ id: 'cam-1', name: 'CAM 1', body: { kind: 'srt', url: 'srt://[::1]:1' } }, { id: 'cam-2', name: 'CAM 2', body: { kind: 'srt', url: 'srt://[::1]:2' } }, { id: 'cam-3', name: 'CAM 3', body: { kind: 'srt', url: 'srt://[::1]:3' } }, { id: 'cam-4', name: 'CAM 4', body: { kind: 'srt', url: 'srt://[::1]:4' } }])),
      http.get('*/api/v1/sources/status', () => HttpResponse.json([{ id: 'cam-4', phase: 'failed', video_wanted: true, audio_wanted: false, video_running: false, audio_running: false, consumers: [], activation: 'on_demand' }, { id: 'cam-3', phase: 'idle', video_wanted: false, audio_wanted: false, video_running: false, audio_running: false, consumers: [], activation: 'on_demand' }])),
    );
    page();
    const pvw = await screen.findByRole('group', { name: 'PVW bus' });
    const pgm = screen.getByRole('group', { name: /PGM bus/ });
    await waitFor(() => { expect(within(pvw).getByRole('button', { name: /CAM 3/ })).not.toBeDisabled(); }, { timeout: 6000 });
    expect(within(pvw).getByRole('button', { name: /CAM 1/ }).title).toContain('Already on Program');
    expect(within(pvw).getByRole('button', { name: /CAM 1/ })).toBeDisabled();
    expect(within(pvw).getByRole('button', { name: /CAM 2/ })).toBeDisabled();
    expect(within(pgm).getByRole('button', { name: /CAM 1/ })).toBeDisabled();
    expect(within(pgm).getByRole('button', { name: /CAM 2/ })).not.toBeDisabled();
    // An idle on-demand source is selectable (the engine starts it); a failed one is not.
    expect(within(pgm).getByRole('button', { name: /CAM 3/ })).not.toBeDisabled();
    await waitFor(() => { expect(within(pvw).getByRole('button', { name: /CAM 4/ })).toBeDisabled(); });
    expect(within(pvw).getByRole('button', { name: /CAM 4/ }).title).toContain('Source failed');
  });

  it('pairs CUT with its real source and an alarm button; AUTO frames sit in a disclosure', async () => {
    const { container } = page();
    await waitFor(() => { expect(screen.getByRole('button', { name: 'CUT' })).not.toBeDisabled(); }, { timeout: 6000 });
    expect(screen.getByRole('button', { name: 'CUT' })).toHaveTextContent('CUT · cam-2');
    expect(container.querySelector('.bd-heading-side .bd-alarm-btn')).toHaveAttribute('href', '/alarms');
    expect(container.querySelector('.bd-more')).toContainElement(screen.getByRole('spinbutton', { name: 'Auto frames' }));
    expect(container.querySelector('.bd-takes>label')).toBeNull();
  });

  it('marks ON AIR — PROGRAM only for engine-confirmed state; tally glyphs follow real buses', async () => {
    server.use(http.get('*/api/v1/sources', () => HttpResponse.json([{ id: 'cam-1', name: 'CAM 1', body: { kind: 'srt', url: 'srt://[::1]:1' } }, { id: 'cam-2', name: 'CAM 2', body: { kind: 'srt', url: 'srt://[::1]:2' } }])));
    page();
    expect(screen.getByText('○ PROGRAM — unconfirmed')).toBeInTheDocument();
    expect(screen.queryByText('● ON AIR — PROGRAM')).toBeNull();
    await screen.findByText('● ON AIR — PROGRAM', undefined, { timeout: 6000 });
    expect(screen.getByText('● PGM')).toBeInTheDocument();
    expect(screen.getByText('▲ PVW')).toBeInTheDocument();
  });

  it('does not confirm ON AIR from a same-tick poll, and CUT stays disabled', async () => {
    frozenTick = true;
    server.use(http.get('*/api/v1/sources', () => HttpResponse.json([{ id: 'cam-1', name: 'CAM 1', body: { kind: 'srt', url: 'srt://[::1]:1' } }])));
    page();
    await waitFor(() => { expect(polls).toBeGreaterThanOrEqual(1); });
    await new Promise((r) => setTimeout(r, 2600));
    expect(polls).toBeGreaterThanOrEqual(2);
    expect(screen.getByText('○ PROGRAM — unconfirmed')).toBeInTheDocument();
    expect(screen.queryByText('● ON AIR — PROGRAM')).toBeNull();
    expect(screen.getByRole('button', { name: /CUT/ })).toBeDisabled();
  }, 10000);

  it('a cached remount alone does not confirm; advancement after remount does', async () => {
    frozenTick = true;
    const { client, unmount } = page();
    await waitFor(() => { expect(client.getQueryData(['switcher', 'me1'])).toBeTruthy(); });
    unmount();
    render(<QueryClientProvider client={client}><I18nProvider i18n={i18n}><MemoryRouter><CastProvider><ProductionDesk /></CastProvider></MemoryRouter></I18nProvider></QueryClientProvider>);
    expect(screen.getByText('○ PROGRAM — unconfirmed')).toBeInTheDocument();
    frozenTick = false;
    await screen.findByText('● ON AIR — PROGRAM', undefined, { timeout: 6000 });
  }, 10000);

  it('blocks a double-activation of Confirm take for the life of the command', async () => {
    page();
    await waitFor(() => { expect(screen.getByRole('button', { name: 'CUT' })).not.toBeDisabled(); }, { timeout: 6000 });
    await userEvent.click(screen.getByRole('button', { name: 'CUT' }));
    const confirm = screen.getByRole('button', { name: 'Confirm take' });
    await userEvent.dblClick(confirm);
    await waitFor(() => { expect(commandCount).toBe(1); });
    expect(screen.getByRole('button', { name: 'CUT' })).toBeDisabled();
    expect(screen.queryByText(/Engine state confirmed/)).toBeNull();
    await userEvent.click(screen.getByRole('button', { name: 'CUT' }));
    expect(commandCount).toBe(1);
  });

  it('two synchronous Confirm clicks in one act produce exactly one POST', async () => {
    page();
    await waitFor(() => { expect(screen.getByRole('button', { name: 'CUT' })).not.toBeDisabled(); }, { timeout: 6000 });
    await userEvent.click(screen.getByRole('button', { name: 'CUT' }));
    const confirm = screen.getByRole('button', { name: 'Confirm take' });
    act(() => { confirm.click(); confirm.click(); });
    await waitFor(() => { expect(commandCount).toBe(1); });
    await new Promise((r) => setTimeout(r, 200));
    expect(commandCount).toBe(1);
  });

  it('two synchronous source clicks produce exactly one POST', async () => {
    server.use(
      // cam-1 is on Program, so its PVW key is (honestly) disabled; cam-3 is a legal PVW pick.
      http.get('*/api/v1/sources', () => HttpResponse.json([{ id: 'cam-1', name: 'CAM 1', body: { kind: 'srt', url: 'srt://[::1]:1' } }, { id: 'cam-3', name: 'CAM 3', body: { kind: 'srt', url: 'srt://[::1]:3' } }])),
      http.post('*/api/v1/switcher/commands/select_preview', () => { commandCount += 1; return HttpResponse.json({ operation_id: `op-${String(commandCount)}`, kind: 'select_preview' }, { status: 202 }); }),
    );
    page();
    const pvw = await screen.findByRole('group', { name: 'PVW bus' });
    expect(await within(pvw).findByRole('button', { name: /CAM 1/ })).toBeDisabled();
    const btn = await within(pvw).findByRole('button', { name: /CAM 3/ });
    await waitFor(() => { expect(btn).not.toBeDisabled(); }, { timeout: 6000 });
    act(() => { btn.click(); btn.click(); });
    await waitFor(() => { expect(commandCount).toBe(1); });
    await new Promise((r) => setTimeout(r, 200));
    expect(commandCount).toBe(1);
  });

  it('exposes Open alarms as a 48px button-class link', async () => {
    page();
    expect(await screen.findByRole('link', { name: 'Open alarms' })).toHaveClass('bd-btn');
  });

  it('reads only active system alarms and renders the truthful empty summary', async () => {
    let filter = '';
    server.use(http.get('*/api/v1/alarms', ({ request }) => { filter = new URL(request.url).search; return HttpResponse.json([]); }));
    page();
    await screen.findByText('No active system alarms reported');
    expect(new URLSearchParams(filter).get('active')).toBe('true');
    expect(new URLSearchParams(filter).get('scope')).toBe('system');
    expect(screen.getByRole('region', { name: 'Active system alarm summary' })).not.toHaveTextContent('DEMO');
    expect(screen.getByRole('link', { name: 'Open alarms' })).toHaveAttribute('href', '/alarms');
  });

  it('does not present failed alarm reads as an all-clear', async () => {
    server.use(http.get('*/api/v1/alarms', () => HttpResponse.json({ title: 'Unavailable' }, { status: 503 })));
    page();
    await screen.findByText('Alarm summary unavailable');
    expect(screen.queryByText('No active system alarms reported')).toBeNull();
  });

  it('shows at most three actual system alarm records without a synthetic count', async () => {
    server.use(http.get('*/api/v1/alarms', () => HttpResponse.json(Array.from({ length: 4 }, (_, i) => ({ id: `alarm-${String(i)}`, kind: 'SignalLoss', severity: 'major', scope: { kind: 'system' }, ack: {}, dwell: 0, raised_at: 0, latched: false })))));
    page();
    await waitFor(() => { expect(screen.getAllByRole('listitem')).toHaveLength(3); });
    expect(screen.getAllByText('major')).toHaveLength(3);
  });
  it('does not clear awaiting state on unrelated topic, M/E, corr or media loading', async () => {
    const { client } = page();
    await waitFor(() => { expect(screen.getByRole('button', { name: 'CUT' })).not.toBeDisabled(); }, { timeout: 6000 });
    await userEvent.click(screen.getByRole('button', { name: 'CUT' }));
    expect(commandCount).toBe(0);
    await userEvent.click(screen.getByRole('button', { name: 'Confirm take' }));
    await waitFor(() => { expect(commandCount).toBe(1); });
    const tracker = getOperationTracker(client);
    for (const event of [
      frame('op-1', outcome('op-1', true), { topic: 'outputs' }),
      frame('op-1', outcome('op-1', true), { id: 'me2' }),
      frame('wrong', outcome('wrong', true)),
      frame('op-1', { player: 'me1', state: { kind: 'loading' } }, { t: 'media.player_state' }),
    ]) act(() => { tracker.onEnvelope(event); });
    expect(screen.getByText(/waiting for the applied event/i)).toBeTruthy();
    expect(screen.getByRole('button', { name: 'CUT' })).toHaveProperty('disabled', true);
    act(() => { tracker.onEnvelope(frame('op-1', outcome('op-1', false, 'engine rejected'))); });
    await waitFor(() => { expect(screen.getByText(/Switcher failed/i)).toBeTruthy(); });
    expect(screen.getByRole('button', { name: 'CUT' })).toHaveProperty('disabled', false);
  });

  it('keeps unknown outcome visibly unresolved and prevents a second cut', async () => {
    const { client } = page();
    await waitFor(() => { expect(screen.getByRole('button', { name: 'CUT' })).not.toBeDisabled(); }, { timeout: 6000 });
    await userEvent.click(screen.getByRole('button', { name: 'CUT' }));
    expect(commandCount).toBe(0);
    await userEvent.click(screen.getByRole('button', { name: 'Confirm take' }));
    await waitFor(() => { expect(commandCount).toBe(1); });
    act(() => { getOperationTracker(client).onGap(); });
    await waitFor(() => { expect(screen.getByText(/Switcher outcome unknown/i)).toBeTruthy(); });
    expect(screen.getByRole('button', { name: 'CUT' })).toHaveProperty('disabled', true);
    expect(commandCount).toBe(1);
  });

  it('accepts exact completed true output-tick event and allows the next cut', async () => {
    const { client } = page();
    await waitFor(() => { expect(screen.getByRole('button', { name: 'CUT' })).not.toBeDisabled(); }, { timeout: 6000 });
    await userEvent.click(screen.getByRole('button', { name: 'CUT' }));
    expect(commandCount).toBe(0);
    await userEvent.click(screen.getByRole('button', { name: 'Confirm take' }));
    await waitFor(() => { expect(commandCount).toBe(1); });
    act(() => { getOperationTracker(client).onEnvelope(frame('op-1', outcome('op-1', true))); });
    await waitFor(() => { expect(screen.getByText(/Switcher applied/i)).toBeTruthy(); });
    expect(screen.getByRole('button', { name: 'CUT' })).toHaveProperty('disabled', false);
  });
});
