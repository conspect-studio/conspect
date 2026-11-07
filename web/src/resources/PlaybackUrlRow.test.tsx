// PlaybackUrlRow (ADR-0101 — "a link to copy the actual URL to paste into a
// tool like VLC"): the Copy button calls the clipboard with the exact URL
// (and separately with the exact `vlc <url>` command text), and operator
// notes render.
import { afterEach, describe, expect, it, vi } from 'vitest';
import { fireEvent, screen, waitFor } from '@testing-library/react';

import { renderWithProviders } from '../test/render';
import { PlaybackUrlRow } from './FormControls';

afterEach(() => vi.unstubAllGlobals());

describe('PlaybackUrlRow', () => {
  it('copies the exact playback URL via the clipboard', async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    vi.stubGlobal('navigator', { clipboard: { writeText } });
    renderWithProviders(
      <PlaybackUrlRow
        idPrefix="test-srt"
        label="SRT (caller)"
        url="srt://viewer.example:9000?mode=caller&latency=120"
        notes={[]}
      />,
    );
    fireEvent.click(screen.getByRole('button', { name: 'Copy SRT (caller)' }));
    await waitFor(() => {
      expect(writeText).toHaveBeenCalledWith(
        'srt://viewer.example:9000?mode=caller&latency=120',
      );
    });
  });

  it('copies the exact `vlc <url>` command text via its own Copy button', async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    vi.stubGlobal('navigator', { clipboard: { writeText } });
    renderWithProviders(
      <PlaybackUrlRow
        idPrefix="test-srt"
        label="SRT (caller)"
        url="srt://viewer.example:9000?mode=caller"
        notes={[]}
      />,
    );
    fireEvent.click(screen.getByRole('button', { name: 'Copy Open in VLC' }));
    await waitFor(() => {
      expect(writeText).toHaveBeenCalledWith('vlc srt://viewer.example:9000?mode=caller');
    });
  });

  it('renders every operator-facing note', () => {
    renderWithProviders(
      <PlaybackUrlRow
        idPrefix="test-push"
        label="Destination"
        url={undefined}
        notes={['pushes to srt://dest.example:9001']}
      />,
    );
    expect(screen.getByText('pushes to srt://dest.example:9001')).toBeInTheDocument();
  });

  it('shows no URL field when the entry carries none (a push destination)', () => {
    renderWithProviders(
      <PlaybackUrlRow idPrefix="test-push" label="Destination" url={undefined} notes={['note']} />,
    );
    expect(screen.queryByRole('textbox')).not.toBeInTheDocument();
    expect(screen.getByText('Destination')).toBeInTheDocument();
  });
});
