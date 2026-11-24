import { test, expect } from '@playwright/test';

function luminance(rgb: number[]): number {
  const linear = rgb.map((v) => { const s = v / 255; return s <= 0.04045 ? s / 12.92 : ((s + 0.055) / 1.055) ** 2.4; });
  return linear[0]! * 0.2126 + linear[1]! * 0.7152 + linear[2]! * 0.0722;
}

test('navigation has current-page semantics, readable contrast and touch targets', async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 900 });
  await page.route('**/api/v1/**', (route) => route.fulfill({ json: [] }));
  await page.route('**/api/v1/auth/status*', (route) => route.fulfill({ json: { auth_required: false, authenticated: true } }));
  await page.goto('/sources');
  const nav = page.getByRole('navigation', { name: 'Core screens' });
  await expect(nav.getByRole('link', { name: /Sources/ })).toHaveAttribute('aria-current', 'page');
  for (const link of await nav.getByRole('link').all()) {
    const result = await link.evaluate((el) => {
      const color = getComputedStyle(el).color;
      let node: Element | null = el;
      let background = 'rgba(0, 0, 0, 0)';
      while (node && background === 'rgba(0, 0, 0, 0)') {
        background = getComputedStyle(node).backgroundColor;
        node = node.parentElement;
      }
      return { color, background, width: el.getBoundingClientRect().width, height: el.getBoundingClientRect().height };
    });
    const foreground = luminance(result.color.match(/[\d.]+/g)!.slice(0, 3).map(Number));
    const background = luminance(result.background.match(/[\d.]+/g)!.slice(0, 3).map(Number));
    expect((Math.max(foreground, background) + 0.05) / (Math.min(foreground, background) + 0.05)).toBeGreaterThanOrEqual(4.5);
    expect(result.width).toBeGreaterThanOrEqual(44);
    expect(result.height).toBeGreaterThanOrEqual(44);
  }
});

test('each wall tile has a source-specific accessible name, never just UNKNOWN', async ({ page }) => {
  await page.route('**/api/v1/**', (route) => route.fulfill({ json: [] }));
  await page.route('**/api/v1/auth/status*', (route) => route.fulfill({ json: { auth_required: false, authenticated: true } }));
  await page.route('**/api/v1/sources', (route) => route.fulfill({ json: [{ id: 'camera-one', name: 'Camera one', body: { kind: 'bars' } }] }));
  await page.goto('/');
  await expect(page.locator('.bd-wall-tile')).toHaveAccessibleName('Preview Camera one');
});
