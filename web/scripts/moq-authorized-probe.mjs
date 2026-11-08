import {chromium} from '@playwright/test';
import {readFile} from 'node:fs/promises';
import {spawn} from 'node:child_process';
const root=process.argv[2];
const privateFile=root+'/tools/moq-fixture/local/admission.json';
const browser=await chromium.launch({headless:true});
let server;
try {
 const page=await browser.newPage();
 await page.goto('http://[::1]:5179/src/preview/moq-authorized-fixture.html');
 server=spawn(root+'/tools/moq-fixture/target/debug/examples/authorized',[
 root+'/tools/moq-fixture/local/cert.pem',root+'/tools/moq-fixture/local/key.pem',privateFile,root+'/tools/moq-fixture/local/fixture.ivf']);
 let serverEvidence='';
 await new Promise((resolve,reject)=>{
  server.stderr.on('data',chunk=>{const text=chunk.toString();serverEvidence+=text;if(text.includes('fixture ready'))resolve();});
  server.once('error',reject);server.once('exit',code=>{if(code!==0)reject(new Error('fixture startup failed'));});
 });
 const admission=JSON.parse(await readFile(privateFile,'utf8'));
 const result=await page.evaluate(async(args)=>await window.runAuthorized(args),{...admission,hash:'33731ee286ad85e0c9ab54ce7d97a3227df48a307d604ca2bcd98ecf01793862'});
 await new Promise(resolve=>server.once('exit',resolve));
 const demandReleased=serverEvidence.includes('demand_released=true');
 console.log(JSON.stringify({browserVersion:browser.version(),...result,demandReleased},null,2));
 if(result.frames<30||new Set(result.timestamps).size<30||!result.closed||result.closedAfterMs>10000||result.framesAfterDrain!==result.framesAfterQuiet||!demandReleased)process.exitCode=1;
}finally{server?.kill();await browser.close();}
