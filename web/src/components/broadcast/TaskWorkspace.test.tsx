// TaskWorkspace tests — the responsive task-fold contract from the design
// reviews: the task, its primary control and the compact STATUS stay in the
// first fold at phone widths; secondary detail collapses behind a 48px
// Details disclosure on narrow viewports and sits in flow on wide ones (the
// Library "Details" pattern). The status line wraps — the round-3 S2 defect
// was a nowrap + overflow-hidden producer line clipped at 390px.
// jsdom has no layout: the responsive split is pinned as utility classes
// (hidden / md:block); physical fold checks belong to the browser notes.
import { describe, expect, it, vi } from 'vitest';
import { fireEvent, screen } from '@testing-library/react';

import { renderWithProviders } from '../../test/render';
import { TaskWorkspace } from './TaskWorkspace';

describe('TaskWorkspace', () => {
  it('renders the task title, the always-visible status and the primary content', () => {
    renderWithProviders(
      <TaskWorkspace title="Audition" status={<span>OUTPUT LIVE</span>}>
        <button type="button">Play</button>
      </TaskWorkspace>,
    );
    expect(screen.getByText('Audition')).toBeInTheDocument();
    expect(screen.getByText('OUTPUT LIVE')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Play' })).toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Details' })).toBeNull();
  });

  it('keeps the status out of the details panel and wrap-safe', () => {
    const { container } = renderWithProviders(
      <TaskWorkspace
        title="VT 1"
        status={<span>OUTPUT LIVE</span>}
        details={<p>Full metadata</p>}
        detailsOpen={false}
        onDetailsToggle={vi.fn()}
      >
        <button type="button">Play</button>
      </TaskWorkspace>,
    );
    const statusRow = container.querySelector<HTMLElement>(
      '[data-testid="task-workspace-status"]',
    );
    expect(statusRow).not.toBeNull();
    expect(statusRow?.className).toMatch(/\bmin-w-0\b/);
    expect(statusRow?.className).toMatch(/break-words/);
    expect(statusRow?.className).not.toMatch(/whitespace-nowrap|overflow-hidden|truncate/);
    // The details panel must not swallow the status line.
    const toggle = screen.getByRole('button', { name: 'Details' });
    const panel = document.getElementById(toggle.getAttribute('aria-controls') ?? '');
    expect(panel).not.toBeNull();
    expect(panel?.contains(statusRow ?? document.body)).toBe(false);
  });

  it('discloses details behind a 48px toggle that reports its state', () => {
    const onToggle = vi.fn();
    renderWithProviders(
      <TaskWorkspace
        title="VT 1"
        details={<p>Full metadata</p>}
        detailsOpen={false}
        onDetailsToggle={onToggle}
      >
        <button type="button">Play</button>
      </TaskWorkspace>,
    );
    const toggle = screen.getByRole('button', { name: 'Details' });
    expect(toggle.className).toMatch(/\bh-12\b/);
    expect(toggle.className).toMatch(/\bmin-w-12\b/);
    // The toggle only exists on narrow viewports; the panel is in flow on
    // wide ones (hidden / md:block is the Tailwind responsive contract).
    expect(toggle.className).toMatch(/\bmd:hidden\b/);
    expect(toggle).toHaveAttribute('aria-expanded', 'false');

    const panel = document.getElementById(toggle.getAttribute('aria-controls') ?? '');
    expect(panel?.className).toMatch(/(^| )hidden( |$)/);
    expect(panel?.className).toMatch(/\bmd:block\b/);

    fireEvent.click(toggle);
    expect(onToggle).toHaveBeenCalledOnce();
    expect(onToggle).toHaveBeenCalledWith(true);
  });

  it('shows the details panel when open', () => {
    renderWithProviders(
      <TaskWorkspace
        title="VT 1"
        details={<p>Full metadata</p>}
        detailsOpen
        onDetailsToggle={vi.fn()}
      >
        <button type="button">Play</button>
      </TaskWorkspace>,
    );
    const toggle = screen.getByRole('button', { name: 'Details' });
    expect(toggle).toHaveAttribute('aria-expanded', 'true');
    const panel = document.getElementById(toggle.getAttribute('aria-controls') ?? '');
    expect(panel?.className).toMatch(/\bblock\b/);
    expect(panel?.className).not.toMatch(/(^| )hidden( |$)/);
    expect(screen.getByText('Full metadata')).toBeInTheDocument();
  });
});
