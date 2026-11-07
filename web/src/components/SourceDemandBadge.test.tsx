import { describe, expect, it } from 'vitest';
import { screen } from '@testing-library/react';

import { renderWithProviders } from '../test/render';
import { SourceDemandBadge } from './SourceDemandBadge';

describe('SourceDemandBadge', () => {
  it('reads idle for an absent status and names no fault', () => {
    renderWithProviders(<SourceDemandBadge demand={undefined} />);
    expect(screen.getByText('Idle')).toBeInTheDocument();
    expect(screen.getByText(/not running and costs nothing/)).toBeInTheDocument();
  });

  it('names who consumes a starting source', () => {
    renderWithProviders(
      <SourceDemandBadge
        demand={{
          id: 's', phase: 'starting', video_wanted: true, audio_wanted: false,
          video_running: true, audio_running: false,
          consumers: [{ kind: 'preview', label: 'whep-egress-1' }, { kind: 'audio_route', label: 'program' }],
        }}
      />,
    );
    expect(screen.getByText('Starting')).toBeInTheDocument();
    expect(screen.getByText('Consumed by: preview whep-egress-1, audio route program')).toBeInTheDocument();
  });

  it('shows a failed source as retrying with its reason', () => {
    renderWithProviders(
      <SourceDemandBadge
        demand={{
          id: 's', phase: 'failed', video_wanted: true, audio_wanted: false,
          video_running: false, audio_running: false,
          consumers: [{ kind: 'layout_cell', label: 'cell-1' }], detail: 'connection refused',
        }}
      />,
    );
    expect(screen.getByText('Failed, retrying')).toBeInTheDocument();
    expect(screen.getByText(/connection refused/)).toBeInTheDocument();
  });
});
