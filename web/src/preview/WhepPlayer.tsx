// <WhepPlayer> — a small, fully testable WHEP client (ADR-W023 §1).
//
// Browser-native WebRTC: negotiate a WHEP session (POST offer → answer), attach
// the inbound MediaStream to a <video>, and tear the session down cleanly on
// unmount and `pagehide`. Load-bearing design points:
//   - Injected `pcFactory` — jsdom has no RTCPeerConnection; injection lets
//     vitest drive a scripted fake through the whole state space without a
//     browser, and keeps the component free of module-level globals.
//   - `recvonly` transceivers for video AND audio before createOffer(), so the
//     offer always carries both m-lines (the server answers `inactive` for
//     audio-less scopes).
//   - `muted` + `playsInline` set before play() — required by autoplay policies
//     (Safari rejects unmuted autoplay). Audio is opt-in via an unmute control.
//   - POST on ICE-gathering-complete OR a 2 s timeout, whichever first
//     (vanilla-ICE endpoint: the client must send a COMPLETE offer).
//   - A getStats() watchdog: inbound-rtp bytesReceived unchanged for ~6 s after
//     connect ⇒ a media-path stall that connectionState never reports.
//   - Teardown DELETEs the session (keepalive) on unmount AND pagehide.
import { useEffect, useRef, useState } from 'react';
import type { JSX } from 'react';
import { useLingui } from '@lingui/react/macro';
import { Volume2, VolumeX } from 'lucide-react';

import { Button } from '../components/ui/button';
import { defaultPcFactory } from './pcFactory';
import type { PeerConnectionFactory } from './pcFactory';
import { deleteWhepSession, postWhepOffer } from './whepSession';

/** The player's connection lifecycle, surfaced for the surrounding surface. */
export type WhepPlayerStatus = 'connecting' | 'playing' | 'failed';

/** How long to wait for ICE gathering to complete before POSTing anyway. */
const GATHER_TIMEOUT_MS = 2000;

/** How long inbound bytes may stay flat after connect before we call it stalled. */
const STALL_TIMEOUT_MS = 6000;

/** The watchdog poll cadence. */
const STATS_POLL_MS = 1000;

/**
 * Wait for ICE gathering to complete, or `GATHER_TIMEOUT_MS`, whichever first —
 * the endpoint is vanilla-ICE (no trickle), so the offer must be complete.
 */
function awaitIceGathering(pc: RTCPeerConnection): Promise<void> {
  if (pc.iceGatheringState === 'complete') {
    return Promise.resolve();
  }
  return new Promise<void>((resolve) => {
    const done = (): void => {
      pc.removeEventListener('icegatheringstatechange', onChange);
      window.clearTimeout(timer);
      resolve();
    };
    const onChange = (): void => {
      if (pc.iceGatheringState === 'complete') {
        done();
      }
    };
    const timer = window.setTimeout(done, GATHER_TIMEOUT_MS);
    pc.addEventListener('icegatheringstatechange', onChange);
  });
}

/** Read a numeric `bytesReceived` off a stats entry, or 0 when absent. */
function bytesReceivedOf(stat: RTCStats): number {
  // `RTCStats` is the base type; `bytesReceived` lives on the inbound-rtp
  // refinement. Read it structurally without a narrowing assertion.
  const record: Record<string, unknown> = { ...stat };
  const bytes = record.bytesReceived;
  return typeof bytes === 'number' ? bytes : 0;
}

/** Sum the `bytesReceived` over every inbound-rtp report in a stats snapshot. */
function inboundBytes(report: RTCStatsReport): number {
  let total = 0;
  report.forEach((stat: RTCStats) => {
    if (stat.type === 'inbound-rtp') {
      total += bytesReceivedOf(stat);
    }
  });
  return total;
}

/**
 * The WHEP player. Renders a muted, inline `<video>` and drives the full
 * negotiate/connect/teardown lifecycle. Calls `onStatus` on each transition and
 * `onFatal` once when the session fails (so the surface can degrade to JPEG).
 */
export function WhepPlayer({
  endpoint,
  label,
  audio = false,
  className,
  pcFactory = defaultPcFactory,
  rtcConfig,
  onStatus,
  onFatal,
  fetchImpl,
}: {
  /** The WHEP endpoint to POST the offer to (e.g. `/api/v1/whep/{id}`). */
  readonly endpoint: string;
  /** Accessible name for the video element. */
  readonly label: string;
  /** Whether to offer/keep an audio m-line; audio still starts muted. */
  readonly audio?: boolean;
  readonly className?: string | undefined;
  /** Overridable PC factory (tests inject a fake; default is real). */
  readonly pcFactory?: PeerConnectionFactory;
  /** ICE configuration (STUN/TURN). Defaults to host-only (no iceServers). */
  readonly rtcConfig?: RTCConfiguration | undefined;
  /** Lifecycle callback — connecting → playing, or → failed. */
  readonly onStatus?: ((status: WhepPlayerStatus) => void) | undefined;
  /** Called once when the session fails (fatal): the surface degrades to JPEG. */
  readonly onFatal?: (() => void) | undefined;
  /** Overridable fetch (tests inject a mock; default is global fetch). */
  readonly fetchImpl?: typeof fetch | undefined;
}): JSX.Element {
  const { t } = useLingui();
  const videoRef = useRef<HTMLVideoElement>(null);
  const [muted, setMuted] = useState(true);
  const [status, setStatus] = useState<WhepPlayerStatus>('connecting');

  // Keep the latest callbacks in refs so the effect can stay mounted-once
  // without re-running when a parent re-renders with new closures. Synced in an
  // effect (never assigned during render).
  const onStatusRef = useRef(onStatus);
  const onFatalRef = useRef(onFatal);
  useEffect(() => {
    onStatusRef.current = onStatus;
    onFatalRef.current = onFatal;
  });

  useEffect(() => {
    // Disposal is signalled via an AbortController: its `signal.aborted` getter
    // is a genuinely-mutable boolean the async negotiate closure reads correctly
    // after each await (a plain `let` is narrowed to its literal by flow
    // analysis).
    const abort = new AbortController();
    // A call (not a property read) so flow analysis never narrows it to a
    // literal across an await boundary.
    const aborted = (): boolean => abort.signal.aborted;
    let sessionUrl: string | undefined;
    let statsTimer: number | undefined;
    const effectiveFetch = fetchImpl ?? fetch;
    const pc = pcFactory(rtcConfig ?? {});

    const setStatusOnce = (next: WhepPlayerStatus): void => {
      if (aborted()) {
        return;
      }
      setStatus(next);
      onStatusRef.current?.(next);
    };

    const fail = (): void => {
      if (aborted()) {
        return;
      }
      setStatusOnce('failed');
      onFatalRef.current?.();
    };

    // recvonly video + audio so the offer always carries both m-lines.
    pc.addTransceiver('video', { direction: 'recvonly' });
    pc.addTransceiver('audio', { direction: 'recvonly' });

    const stream = new MediaStream();
    pc.ontrack = (event): void => {
      stream.addTrack(event.track);
      const video = videoRef.current;
      if (video !== null && video.srcObject !== stream) {
        video.srcObject = stream;
      }
    };

    pc.onconnectionstatechange = (): void => {
      if (pc.connectionState === 'connected') {
        setStatusOnce('playing');
        const video = videoRef.current;
        if (video !== null) {
          // Autoplay needs muted + inline; both are set on the element, but set
          // them again defensively before play() (Safari).
          video.muted = true;
          video.playsInline = true;
          // play() returns a Promise in modern browsers, `undefined` in some
          // impls, and THROWS in jsdom (not implemented). A failed autoplay is
          // non-fatal — the surface still shows pixels once the user interacts.
          try {
            const played: unknown = video.play();
            if (played instanceof Promise) {
              played.catch(() => undefined);
            }
          } catch {
            // jsdom / unsupported: ignore.
          }
        }
        startStallWatchdog();
      } else if (pc.connectionState === 'failed') {
        fail();
      }
    };

    // The getStats watchdog: catch the silent-blackness failure mode where the
    // connection reports `connected` but no media flows.
    let lastBytes = 0;
    let lastProgressAt = Date.now();
    const startStallWatchdog = (): void => {
      if (statsTimer !== undefined) {
        return;
      }
      lastProgressAt = Date.now();
      statsTimer = window.setInterval(() => {
        void pc
          .getStats()
          .then((report) => {
            if (aborted()) {
              return;
            }
            const bytes = inboundBytes(report);
            if (bytes > lastBytes) {
              lastBytes = bytes;
              lastProgressAt = Date.now();
            } else if (Date.now() - lastProgressAt > STALL_TIMEOUT_MS) {
              fail();
            }
          })
          .catch(() => {
            // A failed getStats is not, on its own, a stall signal.
          });
      }, STATS_POLL_MS);
    };

    const negotiate = async (): Promise<void> => {
      const offer = await pc.createOffer();
      await pc.setLocalDescription(offer);
      await awaitIceGathering(pc);
      if (aborted()) {
        return;
      }
      const local = pc.localDescription;
      if (local === null) {
        fail();
        return;
      }
      const answer = await postWhepOffer(endpoint, local.sdp, effectiveFetch);
      if (aborted()) {
        // Raced an unmount: release the session we just opened.
        deleteWhepSession(answer.sessionUrl, effectiveFetch);
        return;
      }
      sessionUrl = answer.sessionUrl;
      await pc.setRemoteDescription({ type: 'answer', sdp: answer.answerSdp });
    };

    void negotiate().catch(() => {
      fail();
    });

    // Tear down promptly on a tab close / navigation, not only on unmount.
    const onPageHide = (): void => {
      if (sessionUrl !== undefined) {
        deleteWhepSession(sessionUrl, effectiveFetch);
      }
    };
    window.addEventListener('pagehide', onPageHide);

    return (): void => {
      abort.abort();
      window.removeEventListener('pagehide', onPageHide);
      if (statsTimer !== undefined) {
        window.clearInterval(statsTimer);
      }
      if (sessionUrl !== undefined) {
        deleteWhepSession(sessionUrl, effectiveFetch);
      }
      pc.ontrack = null;
      pc.onconnectionstatechange = null;
      pc.close();
    };
    // The session is opened ONCE per endpoint; callbacks ride refs so a parent
    // re-render does not retrigger negotiation.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [endpoint]);

  return (
    <div className={`relative ${className ?? ''}`} data-testid="whep-player" data-status={status}>
      {/* eslint-disable-next-line jsx-a11y/media-has-caption -- live preview has no caption track */}
      <video
        ref={videoRef}
        muted={muted}
        playsInline
        autoPlay
        aria-label={label}
        className="size-full bg-black object-contain"
      />
      {audio ? (
        <Button
          type="button"
          variant="secondary"
          size="icon"
          className="absolute bottom-2 right-2"
          aria-pressed={!muted}
          aria-label={muted ? t`Unmute ${label}` : t`Mute ${label}`}
          onClick={(): void => {
            const next = !muted;
            setMuted(next);
            const video = videoRef.current;
            if (video !== null) {
              video.muted = next;
            }
          }}
        >
          {muted ? (
            <VolumeX className="size-4" aria-hidden="true" />
          ) : (
            <Volume2 className="size-4" aria-hidden="true" />
          )}
        </Button>
      ) : null}
    </div>
  );
}
