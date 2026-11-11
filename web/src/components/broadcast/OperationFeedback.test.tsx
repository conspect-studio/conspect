// OperationFeedback tests — the feedback/undo contract from the
// correction-4 reviews:
//   * accepted ≠ applied: the pending line says the request was accepted and
//     is WAITING for the applied event; it never claims applied (safety S3).
//   * a timeout is UNKNOWN, never silently "failed" (the round-2 "presumed not
//     applied" wording is obsolete); the component owns no timer, so a pending
//     operation can never age itself into a false failure.
//   * the correlation id is displayed so the outcome can be reconciled later.
//   * the region has minimum height but grows for long copy. Position-critical
//     controls belong before it; never clip Undo to fake a stable height.
//   * no localStorage authority, no fetch: the component is presentational.
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { fireEvent, screen } from '@testing-library/react';

import { renderWithProviders } from '../../test/render';
import { OperationFeedback } from './OperationFeedback';

const PHASES = [
  'idle',
  'pending',
  'applied',
  'failed',
  'cancelled',
  'unknown',
] as const;

function regionOf(container: HTMLElement): HTMLElement | null {
  return container.querySelector<HTMLElement>('[data-phase]');
}

describe('OperationFeedback — phase wording', () => {
  it('idle keeps the reserved slot but paints nothing', () => {
    const { container } = renderWithProviders(
      <OperationFeedback phase="idle" operationLabel="Mute CAM 3" />,
    );
    const region = regionOf(container);
    expect(region?.dataset.phase).toBe('idle');
    expect(region?.textContent.trim()).toBe('');
  });

  it('pending says accepted and waiting — never applied', () => {
    const { container } = renderWithProviders(
      <OperationFeedback
        phase="pending"
        operationLabel="Mute CAM 3"
        correlationId="op-2028"
      />,
    );
    const region = regionOf(container);
    expect(region?.dataset.phase).toBe('pending');
    expect(region).toHaveTextContent(
      /Mute CAM 3 accepted — waiting for the applied event\./,
    );
    expect(region?.textContent).not.toMatch(/applied(?! event)/);
  });

  it('applied says applied and offers a 48px Undo', () => {
    const { container } = renderWithProviders(
      <OperationFeedback
        phase="applied"
        operationLabel="Mute CAM 3"
        onUndo={vi.fn()}
      />,
    );
    expect(regionOf(container)).toHaveTextContent(/Mute CAM 3 applied\./);
    const undo = screen.getByRole('button', { name: 'Undo Mute CAM 3' });
    expect(undo.className).toMatch(/\bh-12\b/);
    expect(undo.className).toMatch(/\bmin-w-12\b/);
  });

  it('calls onUndo from the Undo key and honours a custom label', () => {
    const onUndo = vi.fn();
    renderWithProviders(
      <OperationFeedback
        phase="applied"
        operationLabel="Mute CAM 3"
        onUndo={onUndo}
        undoLabel="Undo mute (CAM 3)"
      />,
    );
    fireEvent.click(screen.getByRole('button', { name: 'Undo mute (CAM 3)' }));
    expect(onUndo).toHaveBeenCalledOnce();
  });

  it('shows the undo key as busy while the undo itself is pending', () => {
    renderWithProviders(
      <OperationFeedback
        phase="applied"
        operationLabel="Mute CAM 3"
        onUndo={vi.fn()}
        undoPending
      />,
    );
    const undo = screen.getByRole('button', { name: 'Undo Mute CAM 3' });
    expect(undo).toBeDisabled();
    expect(undo).toHaveAttribute('aria-busy', 'true');
    expect(undo.querySelector('.animate-spin')).not.toBeNull();
  });

  it('unknown says the outcome is unknown — never failed', () => {
    const { container } = renderWithProviders(
      <OperationFeedback
        phase="unknown"
        operationLabel="Save VT 2"
        correlationId="core-save-9f21"
      />,
    );
    const region = regionOf(container);
    expect(region?.dataset.phase).toBe('unknown');
    expect(region).toHaveTextContent(
      /Save VT 2 outcome unknown\./,
    );
    expect(region?.textContent).not.toMatch(/failed/i);
  });

  it('shows a supplied verified cause only for unknown, not a default timeout claim', () => {
    const { container, rerender } = renderWithProviders(
      <OperationFeedback phase="unknown" operationLabel="Save VT 2" unknownReason="Connection lost" />,
    );
    expect(regionOf(container)).toHaveTextContent(/Connection lost/);
    rerender(<OperationFeedback phase="unknown" operationLabel="Save VT 2" />);
    expect(regionOf(container)?.textContent).not.toMatch(/Connection lost|timed out/i);
  });

  it('failed names the failure without promising rollback and persists until dismissed', () => {
    const onDismiss = vi.fn();
    const { container } = renderWithProviders(
      <OperationFeedback
        phase="failed"
        operationLabel="Save VT 2"
        failureReason="slot held by an unresolved operation"
        onDismiss={onDismiss}
      />,
    );
    const region = regionOf(container);
    expect(region).toHaveTextContent(/Save VT 2 failed\./);
    expect(region?.textContent).not.toMatch(/nothing (?:was )?changed|rolled back/i);
    expect(region).toHaveTextContent(/slot held by an unresolved operation/);
    fireEvent.click(screen.getByRole('button', { name: 'Dismiss' }));
    expect(onDismiss).toHaveBeenCalledOnce();
  });

  it('cancelled does not assert that effects were rolled back', () => {
    const { container } = renderWithProviders(
      <OperationFeedback phase="cancelled" operationLabel="Save VT 2" />,
    );
    expect(regionOf(container)).toHaveTextContent(/Save VT 2 cancelled\./);
    expect(regionOf(container)?.textContent).not.toMatch(/nothing (?:was )?changed|rolled back/i);
  });

  it('displays the correlation id on pending and unknown phases', () => {
    const { container } = renderWithProviders(
      <OperationFeedback
        phase="pending"
        operationLabel="Load"
        correlationId="op-2028"
      />,
    );
    expect(regionOf(container)).toHaveTextContent(/op-2028/);
  });
});

describe('OperationFeedback — stability and honesty', () => {
  it('never turns a pending operation into a failure by itself', () => {
    vi.useFakeTimers();
    try {
      const { container } = renderWithProviders(
        <OperationFeedback phase="pending" operationLabel="Mute CAM 3" />,
      );
      vi.advanceTimersByTime(60_000);
      const region = regionOf(container);
      expect(region?.dataset.phase).toBe('pending');
      expect(region).toHaveTextContent(/accepted — waiting/);
      expect(region?.textContent).not.toMatch(/timed out|failed/i);
    } finally {
      vi.useRealTimers();
    }
  });

  it('keeps a minimum empty slot without clipping long content', () => {
    for (const phase of PHASES) {
      const { container, unmount } = renderWithProviders(
        <OperationFeedback phase={phase} operationLabel="Mute CAM 3" />,
      );
      const region = regionOf(container);
      expect(region?.className, `phase ${phase}`).toMatch(/\bmin-h-24\b/);
      expect(region?.className, `phase ${phase}`).not.toMatch(/overflow-hidden/);
      // The 68px fixed-slot defect: no fixed height, no max height.
      expect(region?.className, `phase ${phase}`).not.toMatch(/\bh-\[/);
      expect(region?.className, `phase ${phase}`).not.toMatch(/max-h-/);
      // Long feedback lines wrap instead of widening the document.
      expect(region?.className, `phase ${phase}`).toMatch(/\bmin-w-0\b/);
      expect(region?.className, `phase ${phase}`).toMatch(/break-words/);
      unmount();
    }
  });
});

describe('OperationFeedback — no hidden authority', () => {
  let setItem: ReturnType<typeof vi.spyOn>;
  let fetchMock: ReturnType<typeof vi.fn>;

  beforeEach(() => {
    setItem = vi.spyOn(Storage.prototype, 'setItem');
    fetchMock = vi.fn();
    vi.stubGlobal('fetch', fetchMock);
  });

  afterEach(() => {
    setItem.mockRestore();
    vi.unstubAllGlobals();
  });

  it('writes no localStorage and performs no fetch through a full lifecycle', () => {
    const onUndo = vi.fn();
    const onDismiss = vi.fn();
    const { rerender } = renderWithProviders(
      <OperationFeedback
        phase="pending"
        operationLabel="Mute CAM 3"
        correlationId="op-1"
      />,
    );
    rerender(
      <OperationFeedback
        phase="applied"
        operationLabel="Mute CAM 3"
        correlationId="op-1"
        onUndo={onUndo}
      />,
    );
    fireEvent.click(screen.getByRole('button', { name: 'Undo Mute CAM 3' }));
    rerender(
      <OperationFeedback
        phase="failed"
        operationLabel="Mute CAM 3"
        failureReason="denied"
        onDismiss={onDismiss}
      />,
    );
    fireEvent.click(screen.getByRole('button', { name: 'Dismiss' }));
    expect(onUndo).toHaveBeenCalledOnce();
    expect(onDismiss).toHaveBeenCalledOnce();
    expect(setItem).not.toHaveBeenCalled();
    expect(fetchMock).not.toHaveBeenCalled();
  });
});
