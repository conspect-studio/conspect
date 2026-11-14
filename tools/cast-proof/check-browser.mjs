// Preflight: can this Chrome play what a Chromecast plays? (H.264 High + AAC-LC in fMP4 through MSE.) Some
// Chromium builds (chrome-headless-shell, codec-free distro builds) lack H.264/AAC: then every playback check
// would fail for a reason that says nothing about the stream. Exit 0 = supported, 1 = not (with the reason).
//   node check-browser.mjs        browser: CHROME_PATH, else the newest Playwright Chromium (lib.mjs)
import { launchBrowser, CHROME } from './lib.mjs';

const TYPES = ['video/mp4; codecs="avc1.640028"', 'video/mp4; codecs="avc1.4d401f"', 'audio/mp4; codecs="mp4a.40.2"'];
let browser; let code = 1;
try {
  browser = await launchBrowser();
  const page = await browser.newPage();
  const res = await page.evaluate((types) => types.map((t) => [t, !!(window.MediaSource && MediaSource.isTypeSupported(t))]), TYPES);
  const missing = res.filter(([, ok]) => !ok).map(([t]) => t);
  const version = await browser.version();
  if (missing.length) console.error(`browser ${version} (${CHROME}) cannot play ${missing.join(', ')} through MediaSource: use a Chrome with H.264 + AAC (Playwright chromium, Chrome for Testing: npx @puppeteer/browsers install chrome@stable, or Debian's chromium) via CHROME_PATH`);
  else { console.log(`browser ${version} (${CHROME}): H.264 + AAC through MediaSource supported`); code = 0; }
} catch (e) {
  console.error(`browser ${CHROME} did not start: ${e && e.message || e}`);
} finally {
  if (browser) await browser.close().catch(() => {});
}
process.exit(code);
