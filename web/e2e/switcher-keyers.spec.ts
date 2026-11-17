// K5 desk e2e (design note docs/architecture/switcher-keyers.md §10/§11.7):
// drives the built SwitcherPage in a real browser against a mocked engine
// (consistent with desk-viewport.spec.ts — no real engine is started; the API
// is mocked per-test with page.route).
import { test, expect } from '@playwright/test';

const DSK1 = {
  id: 'dsk1', family: 'dsk', fill: 'gfx-channel-1', key: null, type: 'linear',
  fly: null, mask: null, on_air: false, tie: false, ramp: null, rate: 15,
  loss: 'drop_to_off_air', stale: false, pending: false,
};

async function mockBase(page: import('@playwright/test').Page) {
  await page.route('**/api/v1/auth/status*', (route) => route.fulfill({ json: { auth_required: false, authenticated: true } }));
  await page.route('**/api/v1/sources', (route) => route.fulfill({ json: [
    { id: 'gfx-channel-1', name: 'GFX 1', body: { id: 'gfx-channel-1', kind: 'bars' } },
    { id: 'cam-1', name: 'Cam 1', body: { id: 'cam-1', kind: 'bars' } },
  ] }));
  await page.route('**/api/v1/patch', (route) => route.fulfill({ status: 404, json: { title: 'not found', status: 404 } }));
  await page.route('**/api/v1/preview/**/*.jpg', (route) => route.fulfill({ contentType: 'image/svg+xml', body: '<svg xmlns="http://www.w3.org/2000/svg" width="640" height="360"/>' }));
  await page.routeWebSocket('**/api/v1/ws*', (ws) => { ws.onMessage(() => { /* keep the mock connection open */ }); });
}

test('renders the DSK strip and keyer panel from a real build, with no arm strip (#388 not merged)', async ({ page }) => {
  await mockBase(page);
  await page.route('**/api/v1/switcher/me1', (route) => route.fulfill({ json: { program_source: 'cam-1', preview_source: 'cam-1', tick: 10, in_flight: false, keyers: [DSK1], tally: { program: [], preview: [] } } }));
  await page.goto('/switcher');
  await expect(page.getByTestId('keyer-strip-dsk1')).toBeVisible({ timeout: 10000 });
  await expect(page.getByTestId('keyer-panel-dsk1')).toBeVisible();
  await expect(page.getByTestId('keyer-lamp-dsk1')).toHaveAttribute('data-on-air', 'false');
  // The upstream arm strip (#388) is explicitly not built: no such control renders.
  await expect(page.getByText('Downstream keyers')).toBeVisible();
  await expect(page.locator('[data-testid^="keyer-arm-"]')).toHaveCount(0);
});

test('CUT plans then commands keyer_cut, and the lamp follows the next snapshot', async ({ page }) => {
  await mockBase(page);
  let onAir = false;
  await page.route('**/api/v1/switcher/me1', (route) => route.fulfill({ json: { program_source: 'cam-1', preview_source: 'cam-1', tick: 10, in_flight: false, keyers: [{ ...DSK1, on_air: onAir }], tally: { program: [], preview: [] } } }));
  await page.route('**/api/v1/switcher/plan', (route) => route.fulfill({ json: { can_take: true, class: 'class1', keyer: 'main/dsk1', on_air: false } }));
  await page.route('**/api/v1/switcher/commands/keyer_cut', (route) => { onAir = true; return route.fulfill({ status: 202, json: { operation_id: 'op-1', kind: 'keyer_cut' } }); });
  await page.goto('/switcher');
  const strip = page.getByTestId('keyer-strip-dsk1');
  await strip.getByRole('button', { name: 'Cut' }).click();
  await expect(page.getByTestId('keyer-lamp-dsk1')).toHaveAttribute('data-on-air', 'true', { timeout: 10000 });
});

test('a capacity plan refusal shows the detail and applies nothing', async ({ page }) => {
  await mockBase(page);
  await page.route('**/api/v1/switcher/me1', (route) => route.fulfill({ json: { program_source: 'cam-1', preview_source: 'cam-1', tick: 10, in_flight: false, keyers: [DSK1], tally: { program: [], preview: [] } } }));
  await page.route('**/api/v1/switcher/plan', (route) => route.fulfill({ json: { can_take: false, class: 'class1', keyer: 'main/dsk1', on_air: false, cost: { delta: 1.5, fits: false }, refusal: { type: 'capacity', detail: 'DSK budget exceeded' } } }));
  await page.goto('/switcher');
  const panel = page.getByTestId('keyer-panel-dsk1');
  await panel.getByRole('button', { name: 'Review & apply' }).click();
  await expect(panel.getByTestId('keyer-refusal-dsk1')).toHaveText('DSK budget exceeded');
});
