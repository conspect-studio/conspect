import { Origin, Path } from '@moq/net';
import { Player } from '@moq/watch';
import { connectMoq } from '../src/preview/moqConnection.ts';
window.fixture = {frames:0,timestamps:[],version:null,error:null,phase:'module-loaded'};
window.startProbe = async ({endpoint,hash}) => {
 const origin = new Origin.Producer();
 try {
  window.fixture.phase = 'connecting';
  const session = await connectMoq(new URL(endpoint), AbortSignal.timeout(20000), origin, hash ? {serverCertificateHashes:[{value:hash}]} : {});
  window.fixture.version = session.version;
  window.fixture.phase = 'connected';
  const original = CanvasRenderingContext2D.prototype.drawImage;
  CanvasRenderingContext2D.prototype.drawImage = function(image,...args) {
   if(image instanceof VideoFrame){window.fixture.frames++;window.fixture.timestamps.push(image.timestamp);}
   return original.call(this,image,...args);
  };
  const player = new Player({origin,name:Path.from('fixture'),canvas:document.querySelector('canvas'),enabled:true,announced:false,muted:true,paused:false,visible:'always'});
  window.fixture.close = () => {player.close();session.close();origin.close();};
 } catch(error) {
  window.fixture.error = 'CONNECT_FAILED';
  window.fixture.errorType = error?.constructor?.name;
 }
};
