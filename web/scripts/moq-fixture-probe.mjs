import { chromium } from '@playwright/test';
const browser = await chromium.launch({headless:true, args:['--no-sandbox', '--enable-quic', '--autoplay-policy=no-user-gesture-required']});
try {
 const page = await browser.newPage();
 page.on('console', m => console.log(m.type(), m.text()));
 page.on('pageerror', e => console.error(e));
 await page.goto('http://[::1]:5179/src/preview/moq-fixture.html?hash='+process.argv[2]+(process.argv.includes('--audio')?'&audio=1':''));
 await page.waitForFunction((audio) => (window.fixture?.frames >= 30 && (!audio || window.fixture.audioFrames >= 40)) || window.fixture?.error, process.argv.includes('--audio'), {timeout:30000});
 const result = await page.evaluate(() => ({...window.fixture, close:undefined}));
 console.log(JSON.stringify(result));
 if(result.error || result.frames < 30 || new Set(result.timestamps).size < 20) process.exitCode=1;
 await page.evaluate(() => window.fixture.close?.());
} finally {await browser.close();}
