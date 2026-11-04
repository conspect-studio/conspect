import { useEffect, useRef, useState } from 'react';
import type { JSX } from 'react';
import Hls from 'hls.js';
import { Trans } from '@lingui/react/macro';

/** Plays the existing encoded output. No new encode or JPEG polling is requested. */
export function HlsPlayer(props: { readonly url: string; readonly label: string }): JSX.Element {
  return <HlsSession key={props.url} {...props} />;
}

function HlsSession({ url, label }: { readonly url: string; readonly label: string }): JSX.Element {
  const ref = useRef<HTMLVideoElement>(null);
  const [failedUrl, setFailedUrl] = useState<string>();
  const failed = failedUrl === url;
  useEffect(() => {
    const video = ref.current;
    if (video === null) return;
    if (video.canPlayType('application/vnd.apple.mpegurl')) {
      video.src = url;
      return () => { video.removeAttribute('src'); video.load(); };
    }
    if (!Hls.isSupported()) {
      queueMicrotask(() => { setFailedUrl(url); });
      return;
    }
    const hls = new Hls({ maxBufferLength: 12, backBufferLength: 0 });
    let stopped = false;
    const stop = (): void => {
      if (stopped) return;
      stopped = true;
      hls.destroy();
      video.pause();
      video.removeAttribute('src');
      video.load();
    };
    hls.on(Hls.Events.ERROR, (_event, data) => {
      if (data.fatal && !stopped) { setFailedUrl(url); stop(); }
    });
    hls.loadSource(url);
    hls.attachMedia(video);
    return stop;
  }, [url]);
  return (
    <div className="space-y-2">
      <video key={url} ref={ref} aria-label={label} controls autoPlay muted playsInline
        className="aspect-video w-full bg-black object-contain" onError={() => { setFailedUrl(url); }} />
      <p className="px-3 text-xs text-muted-foreground"><Trans>HLS output — delayed by the output buffer, not a low-latency preview.</Trans></p>
      {failed ? (
        <p role="status" className="px-3 text-sm"><Trans>Playback unavailable. The output may not be active, or this browser may not support its codec.</Trans></p>
      ) : null}
    </div>
  );
}
