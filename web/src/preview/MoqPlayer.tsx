import { useEffect, useRef, type JSX } from 'react';
import { MoqLeaseRefused, MoqReleaseUnacknowledged, MoqUnavailable, startMoqPlaybackWithRefresh, type MoqLeaseApi } from './moqPlayback';
import { abrDatasetEntries, abrSnapshot, type AbrPersistedState, type AbrState } from './moqAbr';

/**
 * After an unacknowledged release the server may hold the lease until its
 * deadline (the server clamps a lease to 60 s). 'released' is reported this
 * long after the failure, never earlier.
 */
export const MOQ_UNACKNOWLEDGED_HOLD_MS = 60_000;

// Serialize replacements, including React StrictMode's effect cleanup/remount.
// Each mounted consumer owns its lane. A failed release keeps THIS lane closed
// for the hold below and never blocks another player.

export function MoqPlayer({ api, label, audio = false, className, onUnavailable, onFatal, onLeaseState, onAbr, abrResume }: {
  readonly api: MoqLeaseApi;
  readonly label: string;
  readonly audio?: boolean;
  readonly className?: string | undefined;
  /**
   * MoQ cannot play here: `reason` says why (`unsupported`, a typed lease refusal,
   * `session-failed`, or `release-unacknowledged`).
   */
  readonly onUnavailable: (reason: string) => void;
  /** The lease release was NOT acknowledged: the server may hold the lease until its deadline. */
  readonly onFatal?: (() => void) | undefined;
  readonly onLeaseState?: ((state: 'negotiating' | 'active' | 'released') => void) | undefined;
  /** Per-consumer ABR state: requested vs effective rendition and the reason. */
  readonly onAbr?: ((state: AbrState) => void) | undefined;
  /**
   * Adaptation to carry across a session refresh: pass abrSnapshot() of the last reported
   * state so a fresh lease keeps its target, backoff and compatibility verdict.
   */
  readonly abrResume?: AbrPersistedState | undefined;
}): JSX.Element {
  const canvas = useRef<HTMLCanvasElement>(null);
  const lane = useRef<Promise<void>>(Promise.resolve());
  const adaptation = useRef<AbrPersistedState | undefined>(abrResume);
  const callbacks = useRef({ onUnavailable, onFatal, onLeaseState, onAbr });
  useEffect(() => { callbacks.current = { onUnavailable, onFatal, onLeaseState, onAbr }; });
  useEffect(() => {
    const target = canvas.current;
    if (!target) return;
    const controller = new AbortController();
    const isAborted = (): boolean => controller.signal.aborted;
    const task = lane.current.then(async () => {
      if (controller.signal.aborted) return;
      callbacks.current.onLeaseState?.('negotiating');
      try {
        await startMoqPlaybackWithRefresh({ api, canvas: target, audio, signal: controller.signal,
          ...(adaptation.current ? { resume: adaptation.current } : {}),
          onActive: () => callbacks.current.onLeaseState?.('active'),
          // Exposed twice on purpose: `data-abr-*` on the player element for the DOM/e2e
          // harness, and the callback for callers that render it themselves.
          onAbr: (state) => {
            adaptation.current = abrSnapshot(state);
            for (const [key, value] of Object.entries(abrDatasetEntries(state))) {
              target.setAttribute(`data-${key}`, value);
            }
            callbacks.current.onAbr?.(state);
          } });
        callbacks.current.onLeaseState?.('released');
      } catch (error) {
        // The transport ladder is MoQ → WebRTC → stills. EVERY MoQ failure hands
        // the surface a reason and lets it try the next transport; none ends in a
        // dead view. The surface shows the reason to the user.
        const reason = error instanceof MoqLeaseRefused ? error.reason
          : error instanceof MoqUnavailable ? 'unsupported'
          : error instanceof MoqReleaseUnacknowledged ? 'release-unacknowledged'
          : 'session-failed';
        // Only an unacknowledged release can leave a lease on the server. Say so
        // (onFatal) and do not claim 'released' until the server's lease
        // deadline has certainly passed. The hold belongs to this task, not to
        // the component: it survives an unmount, so a consumer that waits for
        // 'released' (for example to open another preview) always gets it.
        if (reason === 'release-unacknowledged') {
          if (!isAborted()) { callbacks.current.onFatal?.(); callbacks.current.onUnavailable(reason); }
          await new Promise<void>((resolve) => { setTimeout(resolve, MOQ_UNACKNOWLEDGED_HOLD_MS); });
          callbacks.current.onLeaseState?.('released');
          return;
        }
        // Every other failure holds no lease (refused, unsupported, or the
        // release was acknowledged after the session failed).
        callbacks.current.onLeaseState?.('released');
        if (!isAborted()) callbacks.current.onUnavailable(reason);
      }
    });
    lane.current = task;
    void task.catch(() => { /* reported through onUnavailable above */ });
    return () => { controller.abort(); };
  }, [api, audio, abrResume]);
  return <canvas ref={canvas} aria-label={label} role="img" className={className} data-testid="moq-player" />;
}
