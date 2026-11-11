import { test, expect } from '@playwright/test';
import { existsSync } from 'node:fs';
import { resolve } from 'node:path';

const outputs = [{ id: 'program', name: 'Program HLS', body: { id: 'program', kind: 'hls', path: '/srv/index.m3u8', codec: 'h264' } }];
test.beforeEach(async ({ page }) => {
  await page.route('**/api/v1/**', route => route.fulfill({ json: [] }));
  await page.route('**/api/v1/auth/status*', route => route.fulfill({ json: { auth_required: false, authenticated: true } }));
  await page.route('**/api/v1/sources', route => route.fulfill({ json: [{ id: 'camera', name: 'North camera', body: { id: 'camera', kind: 'bars' } }] }));
  await page.route('**/api/v1/outputs', route => route.fulfill({ json: outputs }));
  await page.route('**/api/v1/preview/capabilities', route => route.fulfill({ json: { webrtc: false, fallback: 'jpeg', scopes: { program: { whep: false }, inputs: { whep: false }, outputs: { whep: false } } } }));
  await page.route('**/api/v1/preview/inputs', route => route.fulfill({ json: [] }));
});
test('source preview is opt-in and clearly identifies still fallback', async ({ page }) => {
  let polls = 0;
  await page.route('**/api/v1/preview/inputs/camera.jpg', route => { polls++; return route.fulfill({ status: 503 }); });
  await page.goto('/sources');
  await expect(page.getByText('North camera')).toBeVisible();
  expect(polls).toBe(0);
  await page.getByRole('button', { name: 'Details: North camera' }).click();
  await page.getByRole('button', { name: 'Preview: North camera' }).click();
  await expect(page.getByRole('dialog')).toBeVisible();
  await expect(page.getByText(/Still preview \(~1 fps\)/)).toBeVisible();
  await expect.poll(() => polls).toBeGreaterThan(0);
  await expect(page.getByRole('dialog')).toBeVisible();
  // Phone width: the approved responsive layout may remount rows on resize, so
  // close and reopen the preview from the Details panel.
  await page.keyboard.press('Escape');
  await page.setViewportSize({ width: 390, height: 844 });
  const phoneDetails = page.getByRole('button', { name: 'Details: North camera' });
  if ((await phoneDetails.getAttribute('aria-expanded')) !== 'true') await phoneDetails.click();
  await page.getByRole('button', { name: 'Preview: North camera' }).click();
  await expect(page.getByRole('dialog')).toBeVisible();
  await page.screenshot({ path: '../../.scratch/webui-live-workflows/source-preview-mobile.png', fullPage: true });
  await page.keyboard.press('Escape');
  await expect(page.getByRole('dialog')).not.toBeVisible();
});
test('program decodes actual buffered HLS video when a local fixture is supplied', async ({ page }) => {
  const fixture = process.env.CONSPECT_HLS_FIXTURE;
  test.skip(!fixture || !existsSync(resolve(fixture, 'index.m3u8')), 'Set CONSPECT_HLS_FIXTURE to a generated HLS test fixture.');
  await page.route('**/hls/program/*', route => {
    const name = new URL(route.request().url()).pathname.split('/').at(-1) ?? '';
    return route.fulfill({ path: resolve(fixture!, name), contentType: name.endsWith('.m3u8') ? 'application/vnd.apple.mpegurl' : 'video/mp2t' });
  });
  await page.goto('/monitoring');
  await page.getByLabel('Playback transport').selectOption('program');
  const video = page.getByLabel('Program HLS output');
  await expect.poll(() => video.evaluate((element: HTMLVideoElement) => element.videoWidth)).toBe(320);
  await expect.poll(() => video.evaluate((element: HTMLVideoElement) => element.getVideoPlaybackQuality().totalVideoFrames)).toBeGreaterThan(10);
  await page.screenshot({ path: '../../.scratch/webui-live-workflows/program-video.png', fullPage: true });
});
test('program offers encoded HLS video and output playlist link', async ({ page }) => {
  await page.route('**/hls/program/index.m3u8', route => route.fulfill({ status: 404 }));
  await page.goto('/monitoring');
  await page.getByLabel('Playback transport').selectOption('program');
  await expect(page.getByLabel('Program HLS output')).toBeVisible();
  await expect(page.getByText(/HLS output — delayed/)).toBeVisible();
  await page.screenshot({ path: '../../.scratch/webui-live-workflows/program-preview.png', fullPage: true });
  await page.goto('/outputs');
  await expect(page.getByRole('link', { name: 'Open HLS playlist' })).toHaveAttribute('href', '/hls/program/index.m3u8');
});
