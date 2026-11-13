import { test, expect } from '@playwright/test';

const names = ['ABC (AU) News', 'Clock Melbourne', 'BBC News', 'CNN', 'DW News', 'Fox News', 'MS NOW', 'Timer', 'Times Square', 'ABC (US) News', 'TWC', 'Sydney Harbour'];
for (const count of [12, 24]) {
  for (const [width, height] of [[1920, 1080], [1600, 900], [1366, 768]]) {
    test(`${count} sources fit ${width}x${height} without scrolling`, async ({ page }) => {
      await page.setViewportSize({ width, height });
      await page.route('**/api/v1/**', (route) => route.fulfill({ json: [] }));
      await page.route('**/api/v1/auth/status*', (route) => route.fulfill({ json: { auth_required: false, authenticated: true } }));
      await page.route('**/api/v1/sources', (route) => route.fulfill({ json: Array.from({ length: count }, (_, i) => ({ id: `s${i}`, name: names[i % 12], body: { id: `s${i}`, kind: 'bars' } })) }));
      await page.route('**/api/v1/preview/capabilities', (route) => route.fulfill({ json: { webrtc: false, fallback: 'jpeg', scopes: {} } }));
      // A recognisable test chart, not a fabricated live source. All still/live labels remain real.
      await page.route('**/api/v1/preview/**/*.jpg', (route) => route.fulfill({ contentType: 'image/svg+xml', body: '<svg xmlns="http://www.w3.org/2000/svg" width="640" height="360"><path fill="#192635" d="M0 0h640v360H0z"/><path fill="#31485b" d="M0 0h160v360H0z"/><path fill="#516b76" d="M160 0h160v360H160z"/><path fill="#84958a" d="M320 0h160v360H320z"/><text x="320" y="180" fill="white" text-anchor="middle" font-size="24">PREVIEW TEST CHART</text></svg>' }));
      await page.goto('/');
      const tiles = page.locator('.bd-wall-tile');
      await expect(tiles).toHaveCount(count);
      await expect(page.locator('.bd-pgm img')).toBeVisible();
      expect(await page.evaluate(() => document.documentElement.scrollHeight <= innerHeight)).toBe(true);
      expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
      for (const item of await page.locator('.bd-wall-tile, .bd-bus button, .bd-cut, .bd-auto, .bd-pgm, .bd-pvw').all()) {
        const box = await item.boundingBox();
        expect(box).not.toBeNull();
        expect(box!.y).toBeGreaterThanOrEqual(0);
        expect(box!.y + box!.height).toBeLessThanOrEqual(height);
        expect(box!.width).toBeGreaterThanOrEqual(44);
        expect(box!.height).toBeGreaterThanOrEqual(44);
      }
      for (const label of await tiles.locator('b').all()) {
        expect(await label.evaluate((el) => el.scrollHeight <= el.clientHeight && el.scrollWidth <= el.clientWidth)).toBe(true);
      }
      if (process.env.DESK_SCREENSHOTS && count === 12) await page.screenshot({ path: `${process.env.DESK_SCREENSHOTS}/desk-${width}x${height}.png`, fullPage: true });
    });
  }
}
