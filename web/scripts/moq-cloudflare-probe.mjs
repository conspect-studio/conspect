// Verification only: never print the private endpoint or browser errors.
import { chromium } from '@playwright/test';
import { readFileSync, statSync } from 'node:fs';
const file = process.argv[2];
if (!file || (statSync(file).mode & 0o077)) throw new Error('PRIVATE_HANDOFF_REQUIRED');
const handoff = JSON.parse(readFileSync(file, 'utf8'));
const token = handoff.tokens.find(t => t.operations.includes('subscribe'))?.secret;
if (!token) throw new Error('SUBSCRIBE_TOKEN_REQUIRED');
const browser = await chromium.launch({headless:true,args:['--no-sandbox','--enable-quic']});
try {
 const page = await browser.newPage();
 await page.addInitScript(endpoint => {
  console.debug = console.log = console.info = console.warn = console.error = () => {};
  window.cfEndpoint = endpoint;
 }, `https://draft-16.cloudflare.mediaoverquic.com/${token}`);
 let html = readFileSync(new URL('../src/preview/moq-fixture.html', import.meta.url),'utf8');
 html = html.replace("new URL('https://localhost:4443')", 'new URL(window.cfEndpoint)')
  .replace("{serverCertificateHashes: [{value: query.get('hash')}]}", '{}')
  .replace('window.fixture.error = String(error)', "window.fixture.error = 'CF_BROWSER_CONNECT_OR_DECODE_FAILED'");
 await page.route('**/src/preview/moq-fixture.html', route => route.fulfill({status:200,contentType:'text/html',body:html}));
 await page.goto('http://[::1]:5179/src/preview/moq-fixture.html');
 let timedOut = false;
 try { await page.waitForFunction(() => window.fixture?.frames >= 30 || window.fixture?.error, {timeout:45000}); } catch { timedOut = true; }
 const result = await page.evaluate(() => ({frames:window.fixture?.frames ?? 0,uniqueTimestamps:new Set(window.fixture?.timestamps ?? []).size,version:window.fixture?.version,error:window.fixture?.error}));
 console.log(JSON.stringify({...result,timedOut}));
 if(result.error || result.frames < 30 || result.uniqueTimestamps < 20 || result.version !== 'moq-transport-16') process.exitCode=1;
 await page.evaluate(() => window.fixture?.close?.());
} catch {console.log('CF_BROWSER_PROBE_FAILED');process.exitCode=1;} finally {await browser.close();}
