// K5 desk e2e (design note docs/architecture/switcher-keyers.md §10/§11.7):
// drives the built SwitcherPage in a real browser against a mocked engine
// (consistent with desk-viewport.spec.ts — no real engine is started; the API
// is mocked per-test with page.route). The mock behaves like K4: plan before
// command, an Idempotency-Key on every command, a 202, then — at the "engine
// tick" — a `switcher.operation` outcome and a `switcher.keyer` event on the
// realtime socket, after which the GET snapshot has changed.
import { test, expect } from '@playwright/test';
import type { Page, WebSocketRoute } from '@playwright/test';

const DSK1 = {
  id: 'dsk1', family: 'dsk', armed: false, fill: 'gfx-channel-1', key: null, type: 'linear', clip: 0, gain: 1, invert: false,
  fly: null, mask: null, max_source: null, on_air: false, tie: false, ramp: null, rate: 15,
  loss: 'drop_to_off_air', loss_trigger: 'reconnecting', stale: false, pending: false,
};

interface Engine {
  tick: number;
  dsk1: typeof DSK1;
  calls: { path: string; key: string | null; body: Record<string, unknown> }[];
  socket?: WebSocketRoute;
  seq: number;
}

function snapshot(engine: Engine) {
  return {
    program_source: 'cam-1', preview_source: 'cam-1', tick: engine.tick, in_flight: false,
    canvas_width: 1280, canvas_height: 720, fps_num: 25, fps_den: 1,
    keyers: [engine.dsk1], tally: { program: engine.dsk1.on_air ? ['cam-1', 'gfx-channel-1'] : ['cam-1'], preview: [] },
    transition: { kind: 'cut' },
  };
}

function send(engine: Engine, t: string, id: string, data: Record<string, unknown>, corr?: string): void {
  engine.seq += 1;
  engine.socket?.send(JSON.stringify({ v: 1, t, topic: 'switcher', id, seq: engine.seq, ts: engine.seq, ...(corr ? { corr } : {}), data }));
}

async function mockEngine(page: Page, plan: (body: Record<string, unknown>) => Record<string, unknown>): Promise<Engine> {
  const engine: Engine = { tick: 10, dsk1: { ...DSK1 }, calls: [], seq: 0 };
  await page.route('**/api/v1/auth/status*', (route) => route.fulfill({ json: { auth_required: false, authenticated: true } }));
  await page.route('**/api/v1/sources', (route) => route.fulfill({ json: [
    { id: 'gfx-channel-1', name: 'GFX 1', body: { id: 'gfx-channel-1', kind: 'bars' } },
    { id: 'cam-1', name: 'Cam 1', body: { id: 'cam-1', kind: 'bars' } },
  ] }));
  await page.route('**/api/v1/patch', (route) => route.fulfill({ status: 404, json: { title: 'not found', status: 404 } }));
  await page.route('**/api/v1/preview/**/*.jpg', (route) => route.fulfill({ contentType: 'image/svg+xml', body: '<svg xmlns="http://www.w3.org/2000/svg" width="640" height="360"/>' }));
  await page.routeWebSocket('**/api/v1/ws*', (ws) => { engine.socket = ws; ws.onMessage(() => { /* client frames ignored */ }); });
  await page.route('**/api/v1/switcher/me1', (route) => route.fulfill({ json: snapshot(engine) }));
  await page.route('**/api/v1/switcher/plan', async (route) => {
    const body = route.request().postDataJSON() as Record<string, unknown>;
    engine.calls.push({ path: 'plan', key: route.request().headers()['idempotency-key'] ?? null, body });
    await route.fulfill({ json: plan(body) });
  });
  await page.route('**/api/v1/switcher/commands/*', async (route) => {
    const kind = route.request().url().split('/').pop() ?? '';
    const body = route.request().postDataJSON() as Record<string, unknown>;
    const key = route.request().headers()['idempotency-key'] ?? null;
    engine.calls.push({ path: `commands/${kind}`, key, body });
    const op = `op-${String(engine.calls.length)}`;
    await route.fulfill({ status: 202, json: { operation_id: op, kind } });
    // The engine tick: apply, then the outcome and the keyer event.
    setTimeout(() => {
      engine.tick += 1;
      if (kind === 'keyer_cut') engine.dsk1 = { ...engine.dsk1, on_air: body.on === true };
      if (kind === 'keyer_set' && body.fill !== undefined) engine.dsk1 = { ...engine.dsk1, fill: String(body.fill) };
      send(engine, 'switcher.operation', 'me1', { me: 'me1', program: 'main', operation_id: op, tick: engine.tick, completed: true }, op);
      send(engine, 'switcher.keyer', 'main/dsk1', {
        me: 'me1', program: 'main', keyer: 'main/dsk1', tick: engine.tick, on_air: engine.dsk1.on_air, stale: false,
        reason: kind === 'keyer_set' ? 'definition' : 'operator', fill: engine.dsk1.fill,
      });
    }, 50);
  });
  return engine;
}

const PASS = () => ({ can_take: true, class: 'class1', keyer: 'main/dsk1', on_air: false, cost: { delta: 0.1, fits: true } });

test('renders the DSK strip and keyer panel from a real build, with no arm strip (#388 not merged)', async ({ page }) => {
  await mockEngine(page, PASS);
  await page.goto('/switcher');
  await expect(page.getByTestId('keyer-strip-dsk1')).toBeVisible({ timeout: 10000 });
  await expect(page.getByTestId('keyer-panel-dsk1')).toBeVisible();
  await expect(page.getByTestId('keyer-lamp-dsk1')).toHaveAttribute('data-on-air', 'false');
  await expect(page.locator('[data-testid^="keyer-arm-"]')).toHaveCount(0);
});

test('CUT plans before the command, sends an Idempotency-Key, and the lamp follows the outcome/keyer events', async ({ page }) => {
  const engine = await mockEngine(page, PASS);
  await page.goto('/switcher');
  const strip = page.getByTestId('keyer-strip-dsk1');
  await expect(strip).toBeVisible({ timeout: 10000 });
  await strip.getByRole('button', { name: 'Cut' }).click();
  await expect(page.getByTestId('keyer-lamp-dsk1')).toHaveAttribute('data-on-air', 'true', { timeout: 10000 });
  expect(engine.calls.map((c) => c.path)).toEqual(['plan', 'commands/keyer_cut']);
  expect(engine.calls[0]?.key).toBeNull();
  expect(engine.calls[1]?.key).toMatch(/^[0-9a-f-]{36}$/);
  expect(engine.calls[1]?.body).toMatchObject({ me: 'me1', program: 'main', action: 'keyer_cut', keyer: 'dsk1', on: true });
});

test('a source-loss switcher.keyer event with no operation drops the lamp', async ({ page }) => {
  const engine = await mockEngine(page, PASS);
  engine.dsk1 = { ...engine.dsk1, on_air: true };
  await page.goto('/switcher');
  await expect(page.getByTestId('keyer-lamp-dsk1')).toHaveAttribute('data-on-air', 'true', { timeout: 10000 });
  await expect.poll(() => engine.socket !== undefined).toBe(true);
  engine.dsk1 = { ...engine.dsk1, on_air: false };
  engine.tick += 5;
  send(engine, 'switcher.keyer', 'main/dsk1', { me: 'me1', program: 'main', keyer: 'main/dsk1', tick: engine.tick, on_air: false, stale: false, reason: 'source-lost', fill: 'gfx-channel-1' });
  await expect(page.getByTestId('keyer-lamp-dsk1')).toHaveAttribute('data-on-air', 'false', { timeout: 10000 });
  expect(engine.calls).toHaveLength(0);
});

test('a reviewed keyer_set is applied separately and the next snapshot shows it', async ({ page }) => {
  const engine = await mockEngine(page, PASS);
  await page.goto('/switcher');
  const panel = page.getByTestId('keyer-panel-dsk1');
  await expect(panel).toBeVisible({ timeout: 10000 });
  await panel.getByLabel('Fill').selectOption('cam-1');
  await panel.getByRole('button', { name: 'Review' }).click();
  await expect(panel.getByTestId('keyer-plan-dsk1')).toContainText('Class 1');
  expect(engine.calls.map((c) => c.path)).toEqual(['plan']);
  await panel.getByRole('button', { name: 'Apply' }).click();
  await expect(panel.getByLabel('Fill')).toHaveValue('cam-1');
  await expect.poll(() => engine.dsk1.fill).toBe('cam-1');
  expect(engine.calls.map((c) => c.path)).toEqual(['plan', 'commands/keyer_set']);
  expect(engine.calls[1]?.body).toEqual({ me: 'me1', program: 'main', action: 'keyer_set', keyer: 'dsk1', fill: 'cam-1' });
  expect(engine.calls[1]?.key).toMatch(/^[0-9a-f-]{36}$/);
});

test('a capacity plan refusal shows the detail and submits no command', async ({ page }) => {
  const engine = await mockEngine(page, () => ({ can_take: false, class: 'class1', keyer: 'main/dsk1', on_air: false, cost: { delta: 1.5, fits: false }, refusal: { type: 'capacity', detail: 'DSK budget exceeded' } }));
  await page.goto('/switcher');
  const panel = page.getByTestId('keyer-panel-dsk1');
  await expect(panel).toBeVisible({ timeout: 10000 });
  await panel.getByRole('checkbox', { name: 'Invert matte' }).check();
  await panel.getByRole('button', { name: 'Review' }).click();
  await expect(panel.getByTestId('keyer-refusal-dsk1')).toHaveText('DSK budget exceeded');
  await expect(panel.getByRole('button', { name: 'Apply' })).toBeDisabled();
  expect(engine.calls.map((c) => c.path)).toEqual(['plan']);
});
