// Static Vite build and private in-process endpoint injection; no external dev server.
import { chromium } from '@playwright/test';
import { build, preview } from 'vite';
import { readFileSync, statSync, mkdtempSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { resolve } from 'node:path';
const root=fileURLToPath(new URL('..',import.meta.url));
const mode=process.argv[2];
let endpoint,hash;
if(mode==='local') {endpoint='https://localhost:4443';hash=process.argv[3];}
else if(mode==='cloudflare') {
 const file=process.argv[3];
 if(!file || (statSync(file).mode & 0o077)) throw new Error('PRIVATE_HANDOFF_REQUIRED');
 const handoff=JSON.parse(readFileSync(file,'utf8'));
 const token=handoff.tokens.find(t=>t.operations.includes('subscribe'))?.secret;
 if(!token)throw new Error('SUBSCRIBE_TOKEN_REQUIRED');
 endpoint=`https://draft-16.cloudflare.mediaoverquic.com/${token}`;
}else throw new Error('MODE_REQUIRED');
const outDir=mkdtempSync(resolve(root,'node_modules/.cache-moq-probe-'));
await build({configFile:false,root,logLevel:'silent',build:{outDir,emptyOutDir:false,rollupOptions:{input:resolve(root,'scripts/moq-cloudflare-page.html')}}});
const server=await preview({configFile:false,root,logLevel:'silent',build:{outDir},preview:{host:'::1',port:0,strictPort:true}});
const port=server.httpServer.address().port;
const browser=await chromium.launch({headless:true,args:['--no-sandbox','--enable-quic']});
try {
 const page=await browser.newPage();
 let pageErrors=0;page.on('pageerror',()=>{pageErrors++;});
 await page.addInitScript(()=>{console.debug=console.log=console.info=console.warn=console.error=()=>{};});
 await page.goto(`http://[::1]:${port}/scripts/moq-cloudflare-page.html`);
 await page.waitForFunction(()=>typeof window.startProbe==='function',{timeout:15000});
 await page.evaluate(options=>{void window.startProbe(options);},{endpoint,hash});
 let timedOut=false;
 try{await page.waitForFunction(()=>window.fixture.frames>=30||window.fixture.error,{timeout:45000});}catch{timedOut=true;}
 const result=await page.evaluate(()=>({frames:window.fixture.frames,uniqueTimestamps:new Set(window.fixture.timestamps).size,version:window.fixture.version,error:window.fixture.error,errorType:window.fixture.errorType,phase:window.fixture.phase}));
 console.log(JSON.stringify({mode,...result,timedOut,pageErrors}));
 if(result.error||result.frames<30||result.uniqueTimestamps<20||result.version!=='moq-transport-16')process.exitCode=1;
 await page.evaluate(()=>window.fixture.close?.());
}catch{console.log(JSON.stringify({mode,error:'HARNESS_BOOT_FAILED'}));process.exitCode=1;}
finally{await browser.close();await new Promise(r=>server.httpServer.close(r));}
