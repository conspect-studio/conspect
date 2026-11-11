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

import { safeStats, safeStates, failureStage } from './diagnostics';
import type { FailureStage, PreviewDiagnostics } from './diagnostics';

import { Button } from '../components/ui/button';
import { defaultPcFactory } from './pcFactory';
import type { PeerConnectionFactory } from './pcFactory';
import { WhepSignalingError, postWhepOffer, releaseWhepSession } from './whepSession';

/** The player's connection lifecycle, surfaced for the surrounding surface. */
export type WhepPlayerStatus = 'connecting' | 'playing' | 'failed';

/** How long to wait for ICE gathering to complete before POSTing anyway. */
const GATHER_TIMEOUT_MS = 2000;

/** How long inbound bytes may stay flat after connect before we call it stalled. */
const STALL_TIMEOUT_MS = 6000;

/** The watchdog poll cadence. */
const STATS_POLL_MS = 1000;

/**
 * Releases (DELETE) still in flight from earlier players in this page. The
 * engine admits one live viewer and frees the slot only on DELETE, so a new
 * offer must never overlap this client's previous session's release (a
 * remount / strict-mode double effect otherwise gets a 503 from itself).
 */
const pendingReleases = new Set<Promise<void>>();

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
  onSessionReleased,
  onLeaseState,
  onBusy,
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
  readonly onFatal?: ((diagnostics: PreviewDiagnostics) => void) | undefined;
  /** Overridable fetch (tests inject a mock; default is global fetch). */
  readonly fetchImpl?: typeof fetch | undefined;
  /** Fires after the teardown request completes, for single-focus handoff. */
  readonly onSessionReleased?: (() => void) | undefined;
  /** Reports the negotiation barrier and actual lease lifecycle. */
  readonly onLeaseState?: ((state: 'negotiating' | 'active' | 'released') => void) | undefined;
  /** Called on a 503; `capacity` is true for the `whep-capacity` problem type. */
  readonly onBusy?: ((capacity: boolean) => void) | undefined;
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
  const onBusyRef = useRef(onBusy);
  useEffect(() => {
    onStatusRef.current = onStatus;
    onFatalRef.current = onFatal;
    onBusyRef.current = onBusy;
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
    let statsPending = false;
    let statsEpoch = 0;
    const startedAt = Date.now();
    let connectedAt: number | undefined;
    let signalingMs: number | null = null;
    let httpStatus: number | null = null;
    let media = safeStats();
    let failed = false;
    let connectTimer: number | undefined;
    const effectiveFetch: typeof fetch = async (input, init) => {
      const response = await (fetchImpl ?? fetch)(input, init);
      if (init?.method === 'POST') {
        httpStatus = response.status;
        signalingMs = Date.now() - startedAt;
      }
      return response;
    };
    // Only releases begun BEFORE this player existed (never a later one: that
    // would deadlock against its own wait on us).
    const earlierReleases = [...pendingReleases];
    const pc = pcFactory(rtcConfig ?? {});
    onLeaseState?.('negotiating');
    let releaseStarted: Promise<void> | undefined;
    const release = (): Promise<void> => {
      if (releaseStarted !== undefined) return releaseStarted;
      const run = (async (): Promise<void> => {
        if (sessionUrl === undefined) await negotiationDone;
        if (sessionUrl !== undefined) {
          const url = sessionUrl;
          sessionUrl = undefined;
          await releaseWhepSession(url, effectiveFetch);
        }
        onLeaseState?.('released');
        onSessionReleased?.();
      })();
      releaseStarted = run;
      pendingReleases.add(run);
      void run.finally(() => { pendingReleases.delete(run); });
      return run;
    };

    const setStatusOnce = (next: WhepPlayerStatus): void => {
      if (aborted()) {
        return;
      }
      setStatus(next);
      onStatusRef.current?.(next);
    };

    const fail = (stage: FailureStage = 'signaling'): void => {
      if (aborted() || failed) {
        return;
      }
      failed = true;
      window.clearTimeout(connectTimer);

      setStatusOnce('failed');
      onFatalRef.current?.({ stage, httpStatus, elapsedMs: Date.now() - startedAt,
        signalingMs, connectedMs: connectedAt === undefined ? null : Date.now() - connectedAt,
        ...safeStates(pc), media });
    };

    const onIceChange = (): void => {
      if (pc.iceConnectionState === 'failed') fail('ice-failed');
    };
    pc.addEventListener('iceconnectionstatechange', onIceChange);
    // One outstanding stats request across both connection phases. A phase
    // transition invalidates pending evidence, but never starts a second request.
    const diagnosticTimer = window.setInterval(() => {
      if (aborted() || failed || statsPending) return;
      statsPending = true;
      const epoch = statsEpoch;
      void pc.getStats().then((report) => {
        if (aborted() || failed || epoch !== statsEpoch) return;
        media = safeStats(report);
        if (media.dtls === 'failed') { fail('dtls-failed'); return; }
        if (connectedAt !== undefined) checkProgress();
      }).catch(() => undefined).finally(() => { statsPending = false; });
    }, STATS_POLL_MS);

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
      if (aborted() || failed) return;
      if (pc.connectionState === 'connected') {
        window.clearTimeout(connectTimer);
        if (connectedAt === undefined) {
          statsEpoch += 1;
          connectedAt = Date.now();
          lastProgressAt = connectedAt;
          lastDecodeAt = connectedAt;
        }
        setStatusOnce('playing');
        const video = videoRef.current;
        if (video !== null) {
          // Autoplay needs muted + inline; both are set on the element, but set
          // them again defensively before play() (Safari).
          video.muted = true;
          video.playsInline = true;
          // play() returns a Promise in modern browsers, `undefined` in some
          // impls. Report rejection without copying browser error text.
          try {
            const played: unknown = video.play();
            if (played instanceof Promise) {
              played.catch(() => { fail('media-play-error'); });
            }
          } catch {
            fail('media-play-error');
          }
        }

      } else if (pc.connectionState === 'failed') {
        fail(pc.iceConnectionState === 'failed' ? 'ice-failed' : media.dtls === 'failed' ? 'dtls-failed' : 'peer-failed');
      }
    };

    // The getStats watchdog: catch the silent-blackness failure mode where the
    // connection reports `connected` but no media flows.
    let lastBytes = 0;
    let lastFrames = 0;
    let lastDecodeAt = Date.now();
    let lastProgressAt = Date.now();
    const checkProgress = (): void => {
      const bytes = media.bytesReceived;
      if (media.framesDecoded !== null && media.framesDecoded > lastFrames) {
        lastFrames = media.framesDecoded;
        lastDecodeAt = Date.now();
      }
      if (bytes !== null && bytes > lastBytes) {
        lastBytes = bytes;
        lastProgressAt = Date.now();
      } else if (Date.now() - lastProgressAt > STALL_TIMEOUT_MS) {
        fail(failureStage(media));
      }
      if (media.framesDecoded !== null && Date.now() - lastDecodeAt > STALL_TIMEOUT_MS) {
        fail(failureStage(media));
      }
    };

    const negotiate = async (): Promise<void> => {
      const offer = await pc.createOffer();
      await pc.setLocalDescription(offer);
      await awaitIceGathering(pc);
      // Serialise: wait for any earlier session's DELETE (not our own).
      await Promise.all(earlierReleases);
      if (aborted()) {
        return;
      }
      const local = pc.localDescription;
      if (local === null) {
        fail();
        return;
      }
      const answer = await postWhepOffer(endpoint, local.sdp, effectiveFetch);
      sessionUrl = answer.sessionUrl;
      if (aborted()) return;
      onLeaseState?.('active');
      connectTimer = window.setTimeout(() => {
        fail(media.dtls === 'failed' ? 'dtls-failed' :
          pc.iceConnectionState === 'connected' || pc.iceConnectionState === 'completed' ? 'connection-timeout' : 'ice-timeout');
      }, 15000);
      await pc.setRemoteDescription({ type: 'answer', sdp: answer.answerSdp });
    };

    const negotiationDone = negotiate().catch((error: unknown) => {
      if (error instanceof WhepSignalingError && error.status === 503 && !aborted()) {
        onBusyRef.current?.(error.isCapacity);
      }
      fail(httpStatus === 503 ? 'http-capacity' : httpStatus !== null && (httpStatus < 200 || httpStatus >= 300) ? 'http-error' : 'signaling');
    });

    // Tear down promptly on a tab close / navigation, not only on unmount.
    const onPageHide = (): void => {
      void release();
    };
    window.addEventListener('pagehide', onPageHide);

    return (): void => {
      abort.abort();
      window.clearInterval(diagnosticTimer);
      pc.removeEventListener('iceconnectionstatechange', onIceChange);
      window.clearTimeout(connectTimer);
      window.removeEventListener('pagehide', onPageHide);

      // A POST may be in flight at unmount. Await its late-answer DELETE too;
      // the next focus must not race the lease held by that late answer.
      void release();
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
