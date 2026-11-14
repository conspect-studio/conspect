// #419 item 4: a `youtube` source failing because the video is not live
// (engine reason "not a live stream (status: …)") shows the VT-clip redirect
// hint with the disabled "Import as VT clip" action — never an invented
// import endpoint.
import { describe, expect, it, vi } from 'vitest';
import { screen, within } from '@testing-library/react';
import { MemoryRouter } from 'react-router-dom';

import { SourcesSurface } from './SourcesSurface';
import type { SourceDemand } from '../../api/sourceStatus';
import type { SourceView } from '../../resources/types';
import { renderWithProviders } from '../../test/render';

const src = (id: string, name: string, rawKind: string): SourceView => ({
  id,
  name,
  kind: 'hls',
  rawKind,
  editable: true,
  locator: `https://youtube.test/${id}`,
  eventObservation: 'not_supported',
});

const demand = (id: string, detail: string | undefined): SourceDemand => ({
  id,
  phase: 'failed',
  consumers: [],
  video_running: false,
  audio_running: false,
  video_wanted: false,
  audio_wanted: false,
  activation: 'on_demand',
  ...(detail !== undefined ? { detail } : {}),
});

function mount(rows: readonly SourceView[], status: ReadonlyMap<string, SourceDemand>): void {
  renderWithProviders(
    <MemoryRouter>
      <SourcesSurface
        rows={rows}
        demand={status}
        tileFor={() => undefined}
        linkFor={() => undefined}
        onEdit={vi.fn()}
        onDelete={vi.fn()}
      />
    </MemoryRouter>,
  );
}

describe('SourcesSurface — YouTube not-a-live-stream hint', () => {
  it('shows the VT-clip hint for a youtube source whose reason starts with "not a live stream"', () => {
    const rows = [src('rec', 'Recorded', 'youtube')];
    const status = new Map([['rec', demand('rec', 'not a live stream (status: was_live)')]]);
    mount(rows, status);
    const box = screen.getByTestId('source-failure-box-rec');
    expect(within(box).getByText(/YouTube sources must be live/)).toBeInTheDocument();
    const button = within(box).getByRole('button', { name: 'Import as VT clip' });
    expect(button).toBeDisabled();
    expect(button).toHaveAccessibleDescription(/not available on this server yet.*#423/i);
  });

  it('does not show the hint for an unrelated youtube failure reason', () => {
    const rows = [src('x', 'X', 'youtube')];
    const status = new Map([['x', demand('x', 'ingest ended remotely')]]);
    mount(rows, status);
    const box = screen.getByTestId('source-failure-box-x');
    expect(within(box).queryByText(/YouTube sources must be live/)).toBeNull();
  });

  it('does not show the hint for a non-youtube source, even with a matching reason', () => {
    const rows = [src('y', 'Y', 'hls')];
    const status = new Map([['y', demand('y', 'not a live stream (status: not_live)')]]);
    mount(rows, status);
    const box = screen.getByTestId('source-failure-box-y');
    expect(within(box).queryByText(/YouTube sources must be live/)).toBeNull();
  });
});
