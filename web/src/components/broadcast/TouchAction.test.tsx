// TouchAction tests — the 48px-target contract from the broadcast design
// reviews: every action control is at least 48×48; the pending spinner is
// anchored inside its OWN button (correction-4 comfort N7 — the spinner once
// overlapped the neighbouring key); and a disabled action always says WHY
// (round-2 "blocked states disable unsafe mutations and the banner says why").
// jsdom cannot measure hit rectangles: these tests pin the DOM contract
// (utility classes, aria wiring, event behaviour); the physical 48px
// measurement belongs to the browser regression notes in README.md.
import { describe, expect, it, vi } from 'vitest';
import { fireEvent, screen } from '@testing-library/react';

import { renderWithProviders } from '../../test/render';
import { TouchAction } from './TouchAction';

describe('TouchAction', () => {
  it('renders a button carrying the 48px minimum-target classes', () => {
    renderWithProviders(<TouchAction onPress={vi.fn()}>Play</TouchAction>);
    const btn = screen.getByRole('button', { name: 'Play' });
    expect(btn.className).toMatch(/\bh-12\b/);
    expect(btn.className).toMatch(/\bmin-w-12\b/);
  });

  it('fires onPress when enabled and pressed', () => {
    const onPress = vi.fn();
    renderWithProviders(<TouchAction onPress={onPress}>Play</TouchAction>);
    fireEvent.click(screen.getByRole('button', { name: 'Play' }));
    expect(onPress).toHaveBeenCalledOnce();
  });

  it('never fires when disabled', () => {
    const onPress = vi.fn();
    renderWithProviders(
      <TouchAction onPress={onPress} disabled>
        Stop
      </TouchAction>,
    );
    const btn = screen.getByRole('button', { name: 'Stop' });
    expect(btn).toBeDisabled();
    fireEvent.click(btn);
    expect(onPress).not.toHaveBeenCalled();
  });

  it('shows its own pending spinner inside the button and blocks re-press', () => {
    const onPress = vi.fn();
    renderWithProviders(
      <TouchAction onPress={onPress} pending>
        Mute
      </TouchAction>,
    );
    const btn = screen.getByRole('button', { name: 'Mute' });
    expect(btn).toBeDisabled();
    expect(btn).toHaveAttribute('aria-busy', 'true');
    // The spinner must be a descendant of the button itself — never a
    // sibling, never the neighbour key (N7).
    const spinner = btn.querySelector('.animate-spin');
    expect(spinner).not.toBeNull();
    expect(spinner?.getAttribute('aria-hidden')).toBe('true');
    fireEvent.click(btn);
    expect(onPress).not.toHaveBeenCalled();
  });

  it('renders no spinner when not pending', () => {
    renderWithProviders(<TouchAction onPress={vi.fn()}>Play</TouchAction>);
    const btn = screen.getByRole('button', { name: 'Play' });
    expect(btn.querySelector('.animate-spin')).toBeNull();
    expect(btn.getAttribute('aria-busy')).toBeNull();
  });

  it('exposes the disabled reason as visible text tied by aria-describedby', () => {
    renderWithProviders(
      <TouchAction onPress={vi.fn()} disabled disabledReason="Telemetry is stale">
        Stop
      </TouchAction>,
    );
    const btn = screen.getByRole('button', { name: 'Stop' });
    const reason = screen.getByText('Telemetry is stale');
    const describedBy = btn.getAttribute('aria-describedby');
    expect(describedBy).toBeTruthy();
    expect(document.getElementById(describedBy ?? '')).toBe(reason);
  });

  it('shows no reason line when enabled', () => {
    renderWithProviders(
      <TouchAction onPress={vi.fn()} disabledReason="Telemetry is stale">
        Play
      </TouchAction>,
    );
    expect(screen.queryByText('Telemetry is stale')).toBeNull();
  });
});
