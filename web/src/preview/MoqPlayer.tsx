import { useEffect, useRef, type JSX } from 'react';
import { MoqUnavailable, startMoqPlaybackWithRefresh, type MoqLeaseApi } from './moqPlayback';
import { abrDatasetEntries, abrSnapshot, type AbrPersistedState, type AbrState } from './moqAbr';

// Serialize replacements, including React StrictMode's effect cleanup/remount.
// A failed release intentionally poisons this lane until page reload.
// Each mounted consumer owns its lane. A failed release must not poison another player.

export function MoqPlayer({ api, label, audio = false, className, onUnavailable, onFatal, onLeaseState, onAbr, abrResume }: {
  readonly api: MoqLeaseApi;
  readonly label: string;
  readonly audio?: boolean;
  readonly className?: string | undefined;
  readonly onUnavailable: () => void;
  readonly onFatal: () => void;
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
        if (error instanceof MoqUnavailable) {
          callbacks.current.onLeaseState?.('released');
          if (!isAborted()) callbacks.current.onUnavailable();
          return;
        }
        if (!isAborted()) callbacks.current.onFatal();
        throw error;
      }
    });
    lane.current = task;
    void task.catch(() => { /* No retries or fallback on unclassified failure. */ });
    return () => { controller.abort(); };
  }, [api, audio, abrResume]);
  return <canvas ref={canvas} aria-label={label} role="img" className={className} data-testid="moq-player" />;
}
