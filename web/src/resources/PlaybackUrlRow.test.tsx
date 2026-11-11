// PlaybackUrlRow (ADR-0101 — "a link to copy the actual URL to paste into a
// tool like VLC"): the Copy button calls the clipboard with the exact URL
// (and separately with the exact server-derived `vlcCommand` text — NOT a
// client-rebuilt `vlc <url>` for an SRT row, since VLC's `srt://` access does
// not read the ffmpeg-style `latency=`/`streamid=` URL query the way
// ffmpeg/ffplay do), and operator notes render.
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
        url="srt://viewer.example:9000?mode=caller&latency=120000"
        vlcCommand="vlc --srt-latency=120 srt://viewer.example:9000"
        notes={[]}
      />,
    );
    fireEvent.click(screen.getByRole('button', { name: 'Copy SRT (caller)' }));
    await waitFor(() => {
      expect(writeText).toHaveBeenCalledWith(
        'srt://viewer.example:9000?mode=caller&latency=120000',
      );
    });
  });

  it('copies the server-derived VLC command for an SRT row (not a rebuilt `vlc <url>`)', async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    vi.stubGlobal('navigator', { clipboard: { writeText } });
    renderWithProviders(
      <PlaybackUrlRow
        idPrefix="test-srt"
        label="SRT (caller)"
        url="srt://viewer.example:9000?mode=caller&latency=120000"
        vlcCommand="vlc --srt-latency=120 srt://viewer.example:9000"
        notes={[]}
      />,
    );
    fireEvent.click(screen.getByRole('button', { name: 'Copy Open in VLC' }));
    await waitFor(() => {
      expect(writeText).toHaveBeenCalledWith('vlc --srt-latency=120 srt://viewer.example:9000');
    });
  });

  it('falls back to `vlc <url>` when no vlcCommand is supplied', async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    vi.stubGlobal('navigator', { clipboard: { writeText } });
    renderWithProviders(
      <PlaybackUrlRow
        idPrefix="test-hls"
        label="HLS playlist"
        url="https://viewer.example/hls/program/multiview.m3u8"
        vlcCommand={undefined}
        notes={[]}
      />,
    );
    fireEvent.click(screen.getByRole('button', { name: 'Copy Open in VLC' }));
    await waitFor(() => {
      expect(writeText).toHaveBeenCalledWith(
        'vlc https://viewer.example/hls/program/multiview.m3u8',
      );
    });
  });

  it('renders every operator-facing note', () => {
    renderWithProviders(
      <PlaybackUrlRow
        idPrefix="test-push"
        label="Destination"
        url={undefined}
        vlcCommand={undefined}
        notes={['pushes to srt://dest.example:9001']}
      />,
    );
    expect(screen.getByText('pushes to srt://dest.example:9001')).toBeInTheDocument();
  });

  it('shows no URL field when the entry carries none (a push destination)', () => {
    renderWithProviders(
      <PlaybackUrlRow
        idPrefix="test-push"
        label="Destination"
        url={undefined}
        vlcCommand={undefined}
        notes={['note']}
      />,
    );
    expect(screen.queryByRole('textbox')).not.toBeInTheDocument();
    expect(screen.getByText('Destination')).toBeInTheDocument();
  });
});
