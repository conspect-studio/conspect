import { test, expect } from '@playwright/test';

// Real-browser layout guard for the Program monitor when live video cannot be
// negotiated (vitest runs css:false). Measures boxes; never copy.
const PNG = Buffer.from(
  'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==',
  'base64',
);
type Box = { x: number; y: number; width: number; height: number };
const overlap = (a: Box, b: Box): boolean =>
  a.x < b.x + b.width && b.x < a.x + a.width && a.y < b.y + b.height && b.y < a.y + a.height;

test.beforeEach(async ({ page }) => {
  await page.route('**/api/v1/**', (route) => route.fulfill({ json: [] }));
  await page.route('**/api/v1/auth/status*', (route) =>
    route.fulfill({ json: { auth_required: false, authenticated: true } }),
  );
  await page.route('**/api/v1/sources', (route) =>
    route.fulfill({ json: [{ id: 'cam', name: 'CAM', body: { id: 'cam', kind: 'bars' } }] }),
  );
  await page.route('**/api/v1/preview/capabilities', (route) =>
    route.fulfill({
      json: {
        webrtc: true,
        fallback: 'jpeg',
        scopes: { program: { whep: true }, inputs: { whep: false }, outputs: { whep: false } },
      },
    }),
  );
  await page.route('**/api/v1/preview/program/whep', (route) =>
    route.fulfill({ status: 500, contentType: 'text/plain', body: 'no' }),
  );
  await page.route('**/api/v1/preview/program.jpg', (route) =>
    route.fulfill({ status: 200, contentType: 'image/png', body: PNG }),
  );
});

for (const [width, height] of [
  [1440, 900],
  [1024, 768],
  [390, 844],
] as const) {
  test(`program fallback state is laid out cleanly at ${String(width)}px`, async ({ page }) => {
    await page.setViewportSize({ width, height });
    await page.goto('/');
    const badge = page.getByTestId('fallback-badge');
    await expect(badge).toBeVisible();
    const picture = page.locator('.bd-pgm .bd-picture');
    const img = picture.locator('img');
    await expect(img).toBeVisible();
    const pb = (await picture.boundingBox()) as Box;
    const ib = (await img.boundingBox()) as Box;
    const bb = (await badge.boundingBox()) as Box;
    const retry = page.getByRole('button', { name: 'Retry live preview' });
    await expect(retry).toBeVisible();
    const rb = (await retry.boundingBox()) as Box;
    // 16:9 and the still fills the monitor box.
    expect(Math.abs(pb.width / pb.height - 16 / 9)).toBeLessThan(0.03);
    expect(Math.abs(ib.width - pb.width)).toBeLessThan(2);
    expect(Math.abs(ib.height - pb.height)).toBeLessThan(2);
    const figure = (await page.locator('.bd-pgm').boundingBox()) as Box;
    expect(pb.width).toBeGreaterThan(figure.width - 4);
    // Retry sits inside the badge; the badge stays in the picture corner, clear of its centre.
    expect(rb.x).toBeGreaterThanOrEqual(bb.x - 1);
    expect(rb.x + rb.width).toBeLessThanOrEqual(bb.x + bb.width + 1);
    expect(bb.x + bb.width).toBeLessThanOrEqual(pb.x + pb.width + 1);
    const centre = { x: pb.x + pb.width * 0.4, y: pb.y + pb.height * 0.4, width: pb.width * 0.2, height: pb.height * 0.2 };
    expect(overlap(bb, centre)).toBe(false);
    // Diagnostics: one collapsed disclosure BELOW the picture, inside the monitor, not clipped.
    const disclosure = page.getByTestId('fallback-diagnostics');
    await expect(disclosure).toBeVisible();
    const db = (await disclosure.boundingBox()) as Box;
    expect(db.y).toBeGreaterThanOrEqual(pb.y + pb.height - 1);
    expect(overlap(db, pb)).toBe(false);
    expect(db.x).toBeGreaterThanOrEqual(figure.x - 1);
    expect(db.x + db.width).toBeLessThanOrEqual(figure.x + figure.width + 1);
    await expect(page.getByText('Safe technical details')).toBeHidden();
    await disclosure.locator('summary').first().click();
    await expect(page.getByText('Safe technical details')).toBeVisible();
    const after = (await disclosure.boundingBox()) as Box;
    expect(after.x + after.width).toBeLessThanOrEqual(figure.x + figure.width + 1);
    const scroll = await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth);
    expect(scroll).toBeLessThanOrEqual(0);
  });
}
