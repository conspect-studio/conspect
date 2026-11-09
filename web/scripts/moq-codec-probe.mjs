import {chromium} from '@playwright/test';
const browser=await chromium.launch({headless:true});
try {
 const page=await browser.newPage();
 await page.goto('http://[::1]:5179/src/preview/moq-fixture.html');
 const result=await page.evaluate(async()=>{
  const video={};
  for(const codec of ['vp8','avc1.42001f','hvc1.1.6.L93.B0','hev1.1.6.L93.B0']) {
   try {video[codec]=await VideoDecoder.isConfigSupported({codec,codedWidth:320,codedHeight:180});}
   catch(error){video[codec]={supported:false,error:String(error)};}
  }
  const audio={};
  for(const codec of ['opus','mp4a.40.2']) {
   try {audio[codec]=await AudioDecoder.isConfigSupported({codec,sampleRate:48000,numberOfChannels:2});}
   catch(error){audio[codec]={supported:false,error:String(error)};}
  }
  return {video,audio,userAgent:navigator.userAgent,nativeAudioDecoder:typeof AudioDecoder,nativeAudioEncoder:typeof AudioEncoder};
 });
 console.log(JSON.stringify({browserVersion:browser.version(),...result},null,2));
} finally {await browser.close();}
