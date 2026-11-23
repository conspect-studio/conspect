import { test, expect } from '@playwright/test';

const sources = Array.from({ length: 14 }, (_, i) => ({
  id: `source-${i}`, name: `Broadcast source ${i}`, body: { kind: 'bars' },
}));

test.beforeEach(async ({ page }) => {
  await page.route('**/api/v1/**', (route) => route.fulfill({ json: [] }));
  await page.route('**/api/v1/auth/status*', (route) => route.fulfill({ json: { auth_required: false, authenticated: true } }));
  await page.route('**/api/v1/sources', (route) => route.fulfill({ json: sources }));
  await page.route('**/api/v1/layouts', (route) => route.fulfill({ json: [{ id: 'layout-with-long-identifier', name: 'News composition', body: { cells: [] } }] }));
});

for (const path of ['/', '/routing', '/sources', '/layouts', '/switcher']) {
  test(`${path} reflows at 320px without document scrolling`, async ({ page }) => {
    await page.setViewportSize({ width: 320, height: 800 });
    await page.goto(path);
    await expect(page.locator('#main-content')).toBeVisible();
    if (path === '/layouts') {
      const edit = page.getByRole('link', { name: 'Edit', exact: true });
      await expect(edit).toBeVisible();
      const box = await edit.boundingBox();
      expect(box!.x).toBeGreaterThanOrEqual(0);
      expect(box!.x + box!.width).toBeLessThanOrEqual(320);
    }
    await expect.poll(() => page.evaluate(() => document.documentElement.scrollWidth)).toBeLessThanOrEqual(320);
  });
}
