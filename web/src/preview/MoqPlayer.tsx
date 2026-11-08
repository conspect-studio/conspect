import { useEffect, useRef, type JSX } from 'react';
import { MoqUnavailable, startMoqPlayback, type MoqLeaseApi } from './moqPlayback';

// Serialize replacements, including React StrictMode's effect cleanup/remount.
// A failed release intentionally poisons this lane until page reload.
let lane: Promise<void> = Promise.resolve();
export function MoqPlayer({ api, label, audio = false, className, onUnavailable, onFatal, onLeaseState }: {
  readonly api: MoqLeaseApi;
  readonly label: string;
  readonly audio?: boolean;
  readonly className?: string | undefined;
  readonly onUnavailable: () => void;
  readonly onFatal: () => void;
  readonly onLeaseState?: ((state: 'negotiating' | 'active' | 'released') => void) | undefined;
}): JSX.Element {
  const canvas = useRef<HTMLCanvasElement>(null);
  const callbacks = useRef({ onUnavailable, onFatal, onLeaseState });
  useEffect(() => { callbacks.current = { onUnavailable, onFatal, onLeaseState }; });
  useEffect(() => {
    const target = canvas.current;
    if (!target) return;
    const controller = new AbortController();
    const isAborted = (): boolean => controller.signal.aborted;
    const task = lane.then(async () => {
      if (controller.signal.aborted) return;
      callbacks.current.onLeaseState?.('negotiating');
      try {
        await startMoqPlayback({ api, canvas: target, audio, signal: controller.signal,
          onActive: () => callbacks.current.onLeaseState?.('active') });
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
    lane = task;
    void task.catch(() => { /* No retries or fallback on unclassified failure. */ });
    return () => { controller.abort(); };
  }, [api, audio]);
  return <canvas ref={canvas} aria-label={label} role="img" className={className} data-testid="moq-player" />;
}
