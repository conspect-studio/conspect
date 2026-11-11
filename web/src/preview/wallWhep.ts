// One video-only WHEP wall session for the whole production desk.
import { defaultPcFactory } from './pcFactory';
import { postWhepOffer, releaseWhepSession } from './whepSession';

export interface WallWhepOptions {
  signal: AbortSignal;
  onFrame: (video: HTMLVideoElement) => void;
  onActive: () => void;
}

/** One off-DOM video: present each decoded frame to the store's shared canvas. */
export async function playWallWhep({ signal, onFrame, onActive }: WallWhepOptions): Promise<void> {
  if (signal.aborted) return;
  const pc = defaultPcFactory({});
  const video = document.createElement('video');
  video.muted = true;
  video.playsInline = true;
  video.autoplay = true;
  pc.addTransceiver('video', { direction: 'recvonly' });
  let sessionUrl: string | undefined;
  let callback: number | undefined;
  let gatherTimer: ReturnType<typeof setTimeout> | undefined;
  let watchdog: ReturnType<typeof setInterval> | undefined;
  let lastFrame = performance.now();
  let trackAttached = false;
  let done = false;
  const stream = new MediaStream();
  let resolveEnd: (() => void) | undefined;
  let rejectEnd: ((reason: Error) => void) | undefined;
  const ended = new Promise<void>((resolve, reject) => { resolveEnd = resolve; rejectEnd = reject; });
  // Catch an early peer failure even when signaling is still in progress.
  void ended.catch(() => undefined);
  const dispose = (): void => {
    if (done) return;
    done = true;
    if (callback !== undefined) video.cancelVideoFrameCallback(callback);
    if (gatherTimer !== undefined) clearTimeout(gatherTimer);
    if (watchdog !== undefined) clearInterval(watchdog);
    pc.ontrack = null;
    pc.onconnectionstatechange = null;
    pc.close();
    video.srcObject = null;
    if (sessionUrl) { const url = sessionUrl; sessionUrl = undefined; void releaseWhepSession(url); }
  };
  const abort = (): void => { dispose(); resolveEnd?.(); };
  signal.addEventListener('abort', abort, { once: true });
  const fail = (error: Error): void => { if (!signal.aborted && !done) rejectEnd?.(error); };
  pc.onconnectionstatechange = () => {
    if (pc.connectionState === 'failed' || pc.connectionState === 'closed') fail(new Error(`wall WebRTC ${pc.connectionState}`));
  };
  const onPresentedFrame = (): void => {
    if (signal.aborted || done) return;
    lastFrame = performance.now();
    onFrame(video);
    callback = video.requestVideoFrameCallback(onPresentedFrame);
  };
  pc.ontrack = (event): void => {
    if (event.track.kind !== 'video' || done || trackAttached) return;
    trackAttached = true;
    stream.addTrack(event.track);
    video.srcObject = stream;
    void video.play().then(() => {
      if (!done && !signal.aborted) callback = video.requestVideoFrameCallback(onPresentedFrame);
    }, (error: unknown) => { fail(error instanceof Error ? error : new Error('wall WebRTC autoplay refused')); });
  };
  const aborted = (): boolean => signal.aborted;
  try {
    const offer = await pc.createOffer();
    if (aborted()) return;
    await pc.setLocalDescription(offer);
    if (pc.iceGatheringState !== 'complete') {
      await new Promise<void>((resolve) => {
        const finish = (): void => {
          pc.removeEventListener('icegatheringstatechange', check);
          signal.removeEventListener('abort', finish);
          if (gatherTimer !== undefined) clearTimeout(gatherTimer);
          resolve();
        };
        const check = (): void => { if (pc.iceGatheringState === 'complete') finish(); };
        pc.addEventListener('icegatheringstatechange', check);
        signal.addEventListener('abort', finish, { once: true });
        gatherTimer = setTimeout(finish, 2000);
        check();
      });
    }
    if (aborted() || !pc.localDescription) return;
    const answer = await postWhepOffer('/api/v1/preview/wall/whep', pc.localDescription.sdp);
    sessionUrl = answer.sessionUrl;
    if (aborted()) return;
    await pc.setRemoteDescription({ type: 'answer', sdp: answer.answerSdp });
    if (aborted()) return;
    onActive();
    watchdog = setInterval(() => {
      if (!aborted() && performance.now() - lastFrame > 6000) fail(new Error('wall WebRTC frame stalled'));
    }, 1000);
    await ended;
  } finally {
    signal.removeEventListener('abort', abort);
    dispose();
  }
}
