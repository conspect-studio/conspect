// One video-only WHEP wall session for the whole production desk.
import { defaultPcFactory } from './pcFactory';
import { postWhepOffer, releaseWhepSession } from './whepSession';

export interface WallWhepOptions {
  signal: AbortSignal;
  onFrame: (video: HTMLVideoElement) => void;
  onActive: () => void;
}

export async function playWallWhep({ signal, onFrame, onActive }: WallWhepOptions): Promise<void> {
  const pc = defaultPcFactory({});
  const video = document.createElement('video');
  video.muted = true;
  video.playsInline = true;
  pc.addTransceiver('video', { direction: 'recvonly' });
  let sessionUrl: string | undefined;
  let callback: number | undefined;
  let timeout: ReturnType<typeof setTimeout> | undefined;
  let connectTimeout: ReturnType<typeof setTimeout> | undefined;
  const progress = { lastFrame: performance.now() };
  let watchdog: ReturnType<typeof setInterval> | undefined;
  const stream = new MediaStream();
  const release = (): void => {
    if (callback !== undefined) video.cancelVideoFrameCallback(callback);
    if (timeout !== undefined) clearTimeout(timeout);
    if (connectTimeout !== undefined) clearTimeout(connectTimeout);
    if (watchdog !== undefined) clearInterval(watchdog);
    pc.onconnectionstatechange = null;
    pc.close();
    if (sessionUrl) { const url = sessionUrl; sessionUrl = undefined; void releaseWhepSession(url); }
    video.srcObject = null;
  };
  let failPeer: ((error: Error) => void) | undefined;
  const stop = new Promise<void>((resolve, reject) => {
    failPeer = reject;
    signal.addEventListener('abort', () => { release(); resolve(); }, { once: true });
    pc.onconnectionstatechange = () => {
      if (!signal.aborted && (pc.connectionState === 'failed' || pc.connectionState === 'closed')) reject(new Error(`wall WebRTC ${pc.connectionState}`));
    };
  });
  // Observe a peer failure even when it arrives before the signaling steps end.
  void stop.catch(() => undefined);
  const aborted = (): boolean => signal.aborted;
  const play = async (): Promise<void> => {
    try {
      if (aborted()) return;
      pc.ontrack = (event) => { if (event.track.kind === 'video') { stream.addTrack(event.track); video.srcObject = stream; } };
      const offer = await pc.createOffer();
      if (aborted()) return;
      await pc.setLocalDescription(offer);
      if (pc.iceGatheringState !== 'complete') {
        await new Promise<void>((resolve) => {
          const done = (): void => { pc.removeEventListener('icegatheringstatechange', check); if (timeout !== undefined) clearTimeout(timeout); resolve(); };
          const check = (): void => { if (pc.iceGatheringState === 'complete' || signal.aborted) done(); };
          pc.addEventListener('icegatheringstatechange', check);
          timeout = setTimeout(done, 2000);
          check();
        });
      }
      if (aborted() || !pc.localDescription) return;
      const answer = await postWhepOffer('/api/v1/preview/wall/whep', pc.localDescription.sdp);
      sessionUrl = answer.sessionUrl;
      if (aborted()) return;
      await pc.setRemoteDescription({ type: 'answer', sdp: answer.answerSdp });
      if (aborted()) return;
      connectTimeout = setTimeout(() => { failPeer?.(new Error('wall WebRTC media timeout')); }, 15000);
      watchdog = setInterval(() => {
        if (!signal.aborted && performance.now() - progress.lastFrame > 6000) failPeer?.(new Error('wall WebRTC frame stalled'));
      }, 1000);
      // The browser may need the media track before play resolves.
      void video.play().catch((error: unknown) => { failPeer?.(error instanceof Error ? error : new Error('wall WebRTC autoplay refused')); });
      onActive();
      const frame = (): void => {
        if (aborted()) return;
        progress.lastFrame = performance.now();
        if (connectTimeout !== undefined) { clearTimeout(connectTimeout); connectTimeout = undefined; }
        onFrame(video);
        callback = video.requestVideoFrameCallback(frame);
      };
      callback = video.requestVideoFrameCallback(frame);
      await stop;
    } finally { release(); }
  };
  await play();
}
