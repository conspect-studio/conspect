// K5 desk integration: the keyer strip/panel wired into SwitcherPage against
// the real plan→command flow (design note §10, K5): the shared submission
// gate, realtime refresh on `switcher.keyer`, stream freshness, the ramp clock.
import { afterAll, afterEach, beforeAll, describe, expect, it, vi } from 'vitest';
import type { Envelope } from '../realtime/envelope';
import type { RealtimeStatus } from '../realtime/connection';

const realtime = vi.hoisted(() => ({
  listeners: [] as { onEnvelope: (e: Envelope) => void; onStatus?: (s: RealtimeStatus) => void }[],
}));
vi.mock('../realtime/shared', () => ({
  subscribeRealtime: (_topic: string, onEnvelope: (e: Envelope) => void, onStatus?: (s: RealtimeStatus) => void) => {
    const entry = { onEnvelope, ...(onStatus ? { onStatus } : {}) };
    realtime.listeners.push(entry);
    onStatus?.('open');
    return () => { realtime.listeners.splice(realtime.listeners.indexOf(entry), 1); };
  },
}));
import { act, render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { i18n } from '@lingui/core';
import { I18nProvider } from '@lingui/react';
import { MemoryRouter } from 'react-router-dom';
import { messages } from '../locales/en/messages';
import { SwitcherPage } from './SwitcherPage';
import { getOperationTracker } from '../components/broadcast/operationTracker';

i18n.load('en', messages);
i18n.activate('en');

const DSK1 = {
  id: 'dsk1', family: 'dsk', armed: false, fill: 'gfx-channel-1', key: null, type: 'linear', clip: 0, gain: 1, invert: false,
  fly: null, mask: null, max_source: null, on_air: false, tie: false, ramp: null as null | { start_tick: number; rate: number; to_on: boolean }, rate: 15,
  loss: 'drop_to_off_air', loss_trigger: 'reconnecting', stale: false, pending: false,
};
const BASE_STATE = {
  program_source: 'cam-1', preview_source: 'cam-2', tick: 7, in_flight: false,
  canvas_width: 1280, canvas_height: 720, fps_num: 25, fps_den: 1, keyers: [DSK1] as typeof DSK1[], tally: { program: [], preview: [] },
  transition: { kind: 'cut' }, armed: { background: true, keys: [] as string[] },
};
let state = BASE_STATE;
let calls: { path: string; key: string | null; body: unknown }[] = [];
let planAnswer: unknown = { can_take: true, class: 'class1', keyer: 'main/dsk1', on_air: false };
let planGate: Promise<void> | undefined;
let getGate: Promise<void> | undefined;
let getFails = false;
let commandAnswer: () => Response = () => HttpResponse.json({ operation_id: 'op-x', kind: 'x' }, { status: 202 });

const server = setupServer(
  http.get('*/api/v1/switcher/me1', async () => {
    const snapshot = state;
    if (getGate) await getGate;
    if (getFails) return HttpResponse.json({ title: 'engine busy', status: 503 }, { status: 503 });
    return HttpResponse.json(snapshot);
  }),
  http.get('*/api/v1/patch', () => HttpResponse.json({ title: 'not found', status: 404 }, { status: 404 })),
  http.get('*/api/v1/sources', () => HttpResponse.json([{ id: 'gfx-channel-1', name: 'GFX 1', body: { kind: 'bars' } }, { id: 'cam-1', name: 'Cam 1', body: { kind: 'bars' } }])),
  http.post('*/api/v1/switcher/plan', async ({ request }) => {
    calls.push({ path: 'plan', key: request.headers.get('Idempotency-Key'), body: await request.json() });
    if (planGate) await planGate;
    return HttpResponse.json(planAnswer as object);
  }),
  http.post('*/api/v1/switcher/commands/:kind', async ({ request, params }) => {
    calls.push({ path: `commands/${String(params.kind)}`, key: request.headers.get('Idempotency-Key'), body: await request.json() });
    return commandAnswer();
  }),
);
beforeAll(() => { server.listen(); });
afterEach(() => {
  server.resetHandlers();
  calls = [];
  planAnswer = { can_take: true, class: 'class1', keyer: 'main/dsk1', on_air: false };
  planGate = undefined;
  getGate = undefined;
  getFails = false;
  commandAnswer = () => HttpResponse.json({ operation_id: 'op-x', kind: 'x' }, { status: 202 });
  state = BASE_STATE;
  vi.useRealTimers();
});
afterAll(() => { server.close(); });

let lastClient: QueryClient | undefined;
function page() {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false }, mutations: { retry: false } } });
  lastClient = client;
  return render(<QueryClientProvider client={client}><I18nProvider i18n={i18n}><MemoryRouter><SwitcherPage /></MemoryRouter></I18nProvider></QueryClientProvider>);
}
function emit(envelope: Partial<Envelope> & Pick<Envelope, 't' | 'data'>): void {
  act(() => { for (const l of [...realtime.listeners]) l.onEnvelope({ v: 1, topic: 'switcher', seq: 1, ts: 0, ...envelope }); });
}
function deferred(): { promise: Promise<void>; resolve: () => void } {
  let resolve = (): void => undefined;
  const promise = new Promise<void>((r) => { resolve = r; });
  return { promise, resolve };
}

describe('SwitcherPage keyer desk (K5)', () => {
  it('CUT plans before it commands keyer_cut, with an Idempotency-Key', async () => {
    page();
    const strip = await screen.findByTestId('keyer-strip-dsk1');
    await userEvent.click(within(strip).getByRole('button', { name: 'Cut' }));
    await waitFor(() => { expect(calls.map((c) => c.path)).toEqual(['plan', 'commands/keyer_cut']); });
    expect(calls[1]?.body).toMatchObject({ keyer: 'dsk1', on: true, action: 'keyer_cut' });
    expect(calls[1]?.key).toMatch(/[0-9a-f-]{36}/);
  });

  it('K5-05: repeated clicks across controls while a plan is in flight submit exactly one command', async () => {
    const gate = deferred();
    planGate = gate.promise;
    page();
    const strip = await screen.findByTestId('keyer-strip-dsk1');
    const cut = within(strip).getByRole('button', { name: 'Cut' });
    await userEvent.click(cut);
    await waitFor(() => { expect(cut).toHaveProperty('disabled', true); });
    expect(cut).toHaveAttribute('title', 'A switcher request is in flight');
    // Every switcher and keyer control is behind the same gate.
    const backgroundCut = screen.getAllByRole('button', { name: 'Cut' }).find((b) => !strip.contains(b));
    expect(backgroundCut).toHaveProperty('disabled', true);
    expect(screen.getByRole('button', { name: 'Review' })).toHaveProperty('disabled', true);
    expect(within(strip).getByRole('checkbox', { name: 'Tie' })).toHaveProperty('disabled', true);
    gate.resolve();
    await waitFor(() => { expect(calls.filter((c) => c.path.startsWith('commands/'))).toHaveLength(1); });
    expect(calls.filter((c) => c.path === 'plan')).toHaveLength(1);
  });

  it('K5-05: a keyer review in flight disables the background CUT too', async () => {
    const gate = deferred();
    planGate = gate.promise;
    page();
    await screen.findByTestId('keyer-panel-dsk1');
    await userEvent.click(screen.getByRole('checkbox', { name: 'Invert matte' }));
    await userEvent.click(screen.getByRole('button', { name: 'Review' }));
    const strip = screen.getByTestId('keyer-strip-dsk1');
    const backgroundCut = screen.getAllByRole('button', { name: 'Cut' }).find((b) => !strip.contains(b));
    await waitFor(() => { expect(backgroundCut).toHaveProperty('disabled', true); });
    gate.resolve();
    await waitFor(() => { expect(backgroundCut).toHaveProperty('disabled', false); });
    expect(calls.filter((c) => c.path.startsWith('commands/'))).toHaveLength(0);
  });

  it('K5-06: a source-loss switcher.keyer event (no operation outcome) updates the lamp', async () => {
    state = { ...BASE_STATE, keyers: [{ ...DSK1, on_air: true }] };
    page();
    await waitFor(() => { expect(screen.getByTestId('keyer-lamp-dsk1')).toHaveAttribute('data-on-air', 'true'); });
    state = { ...BASE_STATE, tick: 40, keyers: [{ ...DSK1, on_air: false }] };
    emit({ t: 'switcher.keyer', id: 'main/dsk1', data: { me: 'me1', program: 'main', keyer: 'main/dsk1', tick: 40, on_air: false, stale: false, reason: 'source-lost', fill: 'gfx-channel-1' } });
    await waitFor(() => { expect(screen.getByTestId('keyer-lamp-dsk1')).toHaveAttribute('data-on-air', 'false'); });
    expect(calls).toHaveLength(0);
  });

  it('K5-06: a stream that is not open shows a stale indicator', async () => {
    page();
    await screen.findByTestId('keyer-strip-dsk1');
    expect(screen.queryByTestId('keyer-stream-stale')).toBeNull();
    act(() => { for (const l of realtime.listeners) l.onStatus?.('reconnecting'); });
    expect(screen.getByTestId('keyer-stream-stale')).toBeTruthy();
  });

  it('K5-06: ramp progress advances between snapshots at the API cadence', async () => {
    state = { ...BASE_STATE, tick: 100, keyers: [{ ...DSK1, on_air: true, ramp: { start_tick: 100, rate: 50, to_on: true } }] };
    vi.useFakeTimers({ shouldAdvanceTime: true });
    page();
    const ramp = await screen.findByTestId('keyer-ramp-dsk1');
    const first = Number(ramp.getAttribute('data-weight'));
    expect(first).toBeLessThan(0.2);
    await act(async () => { await vi.advanceTimersByTimeAsync(1000); }); // 25 ticks at 25/1
    const later = Number(screen.getByTestId('keyer-ramp-dsk1').getAttribute('data-weight'));
    expect(later).toBeGreaterThan(first + 0.3);
    expect(calls).toHaveLength(0);
  });

  it('K5-07: a plan refusal submits no command', async () => {
    planAnswer = { can_take: false, class: 'class1', keyer: 'main/dsk1', on_air: false, cost: { delta: 1.5, fits: false }, refusal: { type: 'capacity', detail: 'DSK budget exceeded' } };
    page();
    const panel = await screen.findByTestId('keyer-panel-dsk1');
    await userEvent.click(within(panel).getByRole('checkbox', { name: 'Invert matte' }));
    await userEvent.click(within(panel).getByRole('button', { name: 'Review' }));
    expect(await within(panel).findByTestId('keyer-refusal-dsk1')).toHaveTextContent('DSK budget exceeded');
    expect(within(panel).getByRole('button', { name: 'Apply' })).toHaveProperty('disabled', true);
    expect(calls.map((c) => c.path)).toEqual(['plan']);
  });

  it('K5-07: plan passes, command 409 on-air-protected → fresh plan, explicit allow, one more command', async () => {
    page();
    const panel = await screen.findByTestId('keyer-panel-dsk1');
    await userEvent.selectOptions(within(panel).getByLabelText('Fill'), 'cam-1');
    await userEvent.click(within(panel).getByRole('button', { name: 'Review' }));
    await waitFor(() => { expect(within(panel).getByRole('button', { name: 'Apply' })).toHaveProperty('disabled', false); });
    commandAnswer = () => HttpResponse.json({ type: '/problems/on-air-protected', title: 'Keyer request refused', status: 409, detail: 'on air' }, { status: 409 });
    planAnswer = { can_take: false, class: 'class1', keyer: 'main/dsk1', on_air: true, refusal: { type: 'on-air-protected', detail: 'needs allow' } };
    await userEvent.click(within(panel).getByRole('button', { name: 'Apply' }));
    const confirm = await screen.findByRole('alertdialog', { name: 'Confirm on-air keyer change' });
    expect(calls.map((c) => c.path)).toEqual(['plan', 'commands/keyer_set', 'plan']);
    commandAnswer = () => HttpResponse.json({ operation_id: 'op-set', kind: 'keyer_set' }, { status: 202 });
    await userEvent.click(within(confirm).getByRole('button', { name: 'Allow on-air change' }));
    await waitFor(() => { expect(calls).toHaveLength(4); });
    expect(calls[3]?.body).toEqual({ me: 'me1', program: 'main', action: 'keyer_set', keyer: 'dsk1', fill: 'cam-1', on_air: 'allow' });
    expect(calls[3]?.key).not.toBe(calls[1]?.key);
  });

  it('renders nothing when the snapshot carries no keyers (no backend keyers deployed)', async () => {
    state = { ...BASE_STATE, keyers: [] };
    page();
    await screen.findByRole('button', { name: 'Cut' });
    expect(screen.queryByText('Keyers')).toBeNull();
    expect(screen.queryByTestId('arm-strip')).toBeNull();
  });
});

const USK1 = { ...DSK1, id: 'usk1', family: 'usk', fill: 'cam-1' };
const USK2 = { ...DSK1, id: 'usk2', family: 'usk', fill: 'gfx-channel-1', on_air: true };

describe('SwitcherPage arm strip (K3 #388)', () => {
  it('lights BKGD and KEY toggles from the armed view, with a lamp per KEY', async () => {
    state = { ...BASE_STATE, keyers: [DSK1, USK1, USK2], armed: { background: false, keys: ['usk2'] } };
    page();
    const bkgd = await screen.findByTestId('keyer-arm-background');
    expect(bkgd).toHaveAttribute('aria-pressed', 'false');
    expect(screen.getByTestId('keyer-arm-usk1')).toHaveAttribute('aria-pressed', 'false');
    expect(screen.getByTestId('keyer-arm-usk2')).toHaveAttribute('aria-pressed', 'true');
    expect(screen.getByTestId('keyer-arm-lamp-usk2')).toHaveAttribute('data-on-air', 'true');
    expect(screen.getByTestId('keyer-arm-lamp-usk1')).toHaveAttribute('data-on-air', 'false');
    expect(screen.queryByTestId('keyer-arm-dsk1')).toBeNull();
  });

  it('a toggle plans then commands arm with the full next set and an Idempotency-Key', async () => {
    state = { ...BASE_STATE, keyers: [DSK1, USK1], armed: { background: true, keys: [] } };
    page();
    await userEvent.click(await screen.findByTestId('keyer-arm-usk1'));
    await waitFor(() => { expect(calls.map((c) => c.path)).toEqual(['plan', 'commands/arm']); });
    expect(calls[1]?.body).toEqual({ me: 'me1', program: 'main', action: 'arm', background: true, keys: ['usk1'] });
    expect(calls[1]?.key).toMatch(/[0-9a-f-]{36}/);
    // Not lit optimistically: only the next armed view lights it.
    expect(screen.getByTestId('keyer-arm-usk1')).toHaveAttribute('aria-pressed', 'false');
  });

  it('an arm in flight holds the shared gate: a second toggle or CUT submits nothing more', async () => {
    const gate = deferred();
    planGate = gate.promise;
    state = { ...BASE_STATE, keyers: [DSK1, USK1], armed: { background: true, keys: [] } };
    page();
    await userEvent.click(await screen.findByTestId('keyer-arm-usk1'));
    const bkgd = screen.getByTestId('keyer-arm-background');
    await waitFor(() => { expect(bkgd).toHaveProperty('disabled', true); });
    expect(bkgd).toHaveAttribute('title', 'A switcher request is in flight');
    expect(bkgd.getAttribute('aria-describedby')).toBeTruthy();
    const backgroundCut = screen.getAllByRole('button', { name: 'Cut' }).find((b) => !screen.getByTestId('keyer-strip-dsk1').contains(b));
    expect(backgroundCut).toHaveProperty('disabled', true);
    gate.resolve();
    await waitFor(() => { expect(calls.filter((c) => c.path.startsWith('commands/'))).toHaveLength(1); });
  });

  it('a plan refusal (in-flight) shows its reason and commands nothing', async () => {
    planAnswer = { can_take: false, class: 'class1', keyer: 'me1', on_air: false, refusal: { type: 'in-flight', detail: 'the arm set cannot change during a transition' } };
    state = { ...BASE_STATE, keyers: [DSK1, USK1], armed: { background: true, keys: [] } };
    page();
    await userEvent.click(await screen.findByTestId('keyer-arm-usk1'));
    expect(await screen.findByTestId('arm-error')).toHaveTextContent('the arm set cannot change during a transition');
    expect(calls.map((c) => c.path)).toEqual(['plan']);
  });

  it('an unknown key (404 problem) shows the problem detail', async () => {
    server.use(http.post('*/api/v1/switcher/plan', () => HttpResponse.json(
      { type: '/problems/unknown-keyer', title: 'Keyer request refused', status: 404, detail: 'no upstream keyer "me1/usk1" is defined' }, { status: 404 })));
    state = { ...BASE_STATE, keyers: [DSK1, USK1], armed: { background: true, keys: [] } };
    page();
    await userEvent.click(await screen.findByTestId('keyer-arm-usk1'));
    expect(await screen.findByTestId('arm-error')).toHaveTextContent('no upstream keyer "me1/usk1" is defined');
  });

  it('a take-held refusal at the engine tick is shown as the operation failure reason', async () => {
    commandAnswer = () => HttpResponse.json({ operation_id: 'op-arm', kind: 'arm' }, { status: 202 });
    state = { ...BASE_STATE, keyers: [DSK1, USK1], armed: { background: true, keys: [] } };
    page();
    await userEvent.click(await screen.findByTestId('keyer-arm-usk1'));
    await waitFor(() => { expect(calls.map((c) => c.path)).toEqual(['plan', 'commands/arm']); });
    await screen.findByText(/op-arm/);
    const outcome = { t: 'switcher.operation', id: 'me1', corr: 'op-arm', data: { me: 'me1', program: 'main', operation_id: 'op-arm', tick: 8, completed: false,
      error: 'take-held: a take is held waiting for a fresh frame; arm after it lands or is refused' } } as const;
    // The app shell's engine-events hook feeds the shared tracker; here the test does.
    act(() => { if (lastClient) getOperationTracker(lastClient).onEnvelope({ v: 1, topic: 'switcher', seq: 2, ts: 0, ...outcome }); });
    emit(outcome);
    expect(await screen.findByText(/take-held/)).toBeTruthy();
  });

  it('a transition running disables every toggle with its reason', async () => {
    state = { ...BASE_STATE, in_flight: true, keyers: [DSK1, USK1], armed: { background: true, keys: [] } };
    page();
    const key = await screen.findByTestId('keyer-arm-usk1');
    expect(key).toHaveProperty('disabled', true);
    expect(key).toHaveAttribute('title', 'The arm set cannot change during a transition');
  });

  it('CUT after a key-only arm is enabled even with PGM == PVW', async () => {
    planAnswer = { can_take: true, class: 'class1', scene_version: 1 };
    state = { ...BASE_STATE, program_source: 'cam-1', preview_source: 'cam-1', keyers: [DSK1, USK1], armed: { background: false, keys: ['usk1'] } };
    page();
    await screen.findByTestId('keyer-arm-usk1');
    const strip = screen.getByTestId('keyer-strip-dsk1');
    const backgroundCut = screen.getAllByRole('button', { name: 'Cut' }).find((b) => !strip.contains(b) && !screen.getByTestId('keyer-strip-usk1').contains(b));
    expect(backgroundCut).toHaveProperty('disabled', false);
    await userEvent.click(backgroundCut!);
    await waitFor(() => { expect(calls.map((c) => c.path)).toEqual(['plan', 'commands/cut']); });
  });

  it('a USK strip offers CUT with family usk and no TIE/AUTO', async () => {
    state = { ...BASE_STATE, keyers: [DSK1, USK1], armed: { background: true, keys: [] } };
    page();
    const strip = await screen.findByTestId('keyer-strip-usk1');
    expect(within(strip).queryByRole('button', { name: 'Auto' })).toBeNull();
    expect(within(strip).queryByRole('checkbox', { name: 'Tie' })).toBeNull();
    await userEvent.click(within(strip).getByRole('button', { name: 'Cut' }));
    await waitFor(() => { expect(calls.map((c) => c.path)).toEqual(['plan', 'commands/keyer_cut']); });
    expect(calls[1]?.body).toMatchObject({ action: 'keyer_cut', keyer: 'usk1', family: 'usk', on: true });
  });
});

describe('arm and keyer editing after an outcome (review K5-R2-02)', () => {
  const applied = (op: string, tick: number) => {
    const outcome = { t: 'switcher.operation', id: 'me1', corr: op, data: { me: 'me1', program: 'main', operation_id: op, tick, completed: true } } as const;
    act(() => { if (lastClient) getOperationTracker(lastClient).onEnvelope({ v: 1, topic: 'switcher', seq: 2, ts: 0, ...outcome }); });
    emit(outcome);
  };

  it('stays gated until the post-outcome snapshot arrives, so a fast second toggle keeps the first key', async () => {
    commandAnswer = () => HttpResponse.json({ operation_id: 'op-arm', kind: 'arm' }, { status: 202 });
    state = { ...BASE_STATE, keyers: [DSK1, USK1, USK2], armed: { background: true, keys: [] } };
    page();
    await userEvent.click(await screen.findByTestId('keyer-arm-usk1'));
    await waitFor(() => { expect(calls.map((c) => c.path)).toEqual(['plan', 'commands/arm']); });
    await screen.findByText(/op-arm/);
    // The engine applied it; the refreshed snapshot is slow to come back.
    const slow = deferred();
    getGate = slow.promise;
    state = { ...BASE_STATE, tick: 8, keyers: [DSK1, { ...USK1, armed: true }, USK2], armed: { background: true, keys: ['usk1'] } };
    applied('op-arm', 8);
    await screen.findByText(/Switcher applied/i);
    const usk2 = screen.getByTestId('keyer-arm-usk2');
    expect(usk2).toHaveProperty('disabled', true);
    expect(usk2).toHaveAttribute('title', 'Refreshing the switcher state after the last operation');
    expect(within(screen.getByTestId('keyer-panel-dsk1')).getByRole('button', { name: 'Review' })).toHaveProperty('disabled', true);
    await userEvent.click(usk2);
    expect(calls.filter((c) => c.path === 'commands/arm')).toHaveLength(1);
    slow.resolve();
    await waitFor(() => { expect(screen.getByTestId('keyer-arm-usk1')).toHaveAttribute('aria-pressed', 'true'); });
    await waitFor(() => { expect(screen.getByTestId('keyer-arm-usk2')).toHaveProperty('disabled', false); });
    commandAnswer = () => HttpResponse.json({ operation_id: 'op-arm-2', kind: 'arm' }, { status: 202 });
    await userEvent.click(screen.getByTestId('keyer-arm-usk2'));
    await waitFor(() => { expect(calls.filter((c) => c.path === 'commands/arm')).toHaveLength(2); });
    expect(calls.filter((c) => c.path === 'commands/arm')[1]?.body).toMatchObject({ background: true, keys: ['usk1', 'usk2'] });
  });

  it('fails closed with a reason when the post-outcome refresh fails, until a later snapshot succeeds', async () => {
    commandAnswer = () => HttpResponse.json({ operation_id: 'op-arm', kind: 'arm' }, { status: 202 });
    state = { ...BASE_STATE, keyers: [DSK1, USK1, USK2], armed: { background: true, keys: [] } };
    page();
    await userEvent.click(await screen.findByTestId('keyer-arm-usk1'));
    await screen.findByText(/op-arm/);
    getFails = true;
    applied('op-arm', 8);
    await waitFor(() => {
      expect(screen.getByTestId('keyer-arm-usk2')).toHaveAttribute('title',
        'The switcher state could not be refreshed after the last operation; keyer and arm controls stay disabled until it is');
    });
    expect(screen.getByTestId('keyer-arm-usk2')).toHaveProperty('disabled', true);
    getFails = false;
    state = { ...BASE_STATE, tick: 9, keyers: [DSK1, { ...USK1, armed: true }, USK2], armed: { background: true, keys: ['usk1'] } };
    emit({ t: 'switcher.keyer', id: 'me1/usk1', data: { me: 'me1', program: 'main', keyer: 'me1/usk1', tick: 9, on_air: false, stale: false, fill: 'cam-1' } });
    await waitFor(() => { expect(screen.getByTestId('keyer-arm-usk2')).toHaveProperty('disabled', false); });
  });
});

describe('a DSK and a USK with the same id (review K5-R2-03)', () => {
  const DSK_KEY1 = { ...DSK1, id: 'key1', type: 'luma' };
  const USK_KEY1 = { ...DSK1, id: 'key1', family: 'usk', fill: 'cam-1', type: 'luma' };

  it('keeps labels, descriptions and drafts per family', async () => {
    state = { ...BASE_STATE, in_flight: true, keyers: [DSK_KEY1, USK_KEY1], armed: { background: true, keys: [] } };
    page();
    await waitFor(() => { expect(screen.getAllByTestId('keyer-panel-key1')).toHaveLength(2); });
    const [dskPanel, uskPanel] = screen.getAllByTestId('keyer-panel-key1');
    const [dskStrip, uskStrip] = screen.getAllByTestId('keyer-strip-key1');
    // Every label targets the control in its own panel.
    for (const panel of [dskPanel!, uskPanel!]) {
      for (const name of ['Fill', 'Type', 'Clip', 'Gain', 'Loss policy']) {
        expect(panel.contains(within(panel).getByLabelText(name)), name).toBe(true);
      }
    }
    // The USK CUT's description is its own (outside a transition), not the DSK's.
    const uskCut = within(uskStrip!).getByRole('button', { name: 'Cut' });
    const described = document.getElementById(uskCut.getAttribute('aria-describedby') ?? '');
    expect(uskStrip!.contains(described)).toBe(true);
    expect(described).toHaveTextContent('outside a transition');
    expect(within(dskStrip!).getByLabelText('Rate in frames for key1')).toBe(within(dskStrip!).getByRole('spinbutton'));
    // Independent drafts: editing the USK's clip leaves the DSK's alone.
    const uskClip = within(uskPanel!).getByLabelText('Clip');
    await userEvent.clear(uskClip);
    await userEvent.type(uskClip, '0.5');
    expect(uskClip).toHaveValue(0.5);
    expect(within(dskPanel!).getByLabelText('Clip')).toHaveValue(0);
    expect(screen.getAllByRole('region', { name: /key1 panel/ }).map((r) => r.getAttribute('aria-label'))).toEqual(['Keyer key1 panel', 'Upstream keyer key1 panel']);
  });
});
