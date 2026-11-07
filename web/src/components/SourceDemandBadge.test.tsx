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

  it('names who uses a starting source and where it is', () => {
    renderWithProviders(
      <SourceDemandBadge
        demand={{
          id: 's', phase: 'starting', video_wanted: true, audio_wanted: false,
          video_running: true, audio_running: false, activation: 'on_demand',
          starting_phase: 'awaiting_media',
          consumers: [{ kind: 'preview', label: 'whep-egress-1' }, { kind: 'audio_route', label: 'program' }],
        }}
      />,
    );
    expect(screen.getByText('Starting: waiting for first media')).toBeInTheDocument();
    expect(screen.getByText('In use by: preview whep-egress-1, audio route program')).toBeInTheDocument();
  });

  it('shows a failed source as retrying with its reason', () => {
    renderWithProviders(
      <SourceDemandBadge
        demand={{
          id: 's', phase: 'failed', video_wanted: true, audio_wanted: false,
          video_running: false, audio_running: false, activation: 'always_on',
          consumers: [{ kind: 'layout_cell', label: 'cell-1' }], detail: 'connection refused',
        }}
      />,
    );
    expect(screen.getByText('Failed, retrying')).toBeInTheDocument();
    expect(screen.getByText(/connection refused/)).toBeInTheDocument();
  });

  it('reads a ready always-on source as Ready, naming the setting', () => {
    renderWithProviders(
      <SourceDemandBadge
        demand={{
          id: 's', phase: 'ready', video_wanted: true, audio_wanted: true,
          video_running: true, audio_running: true, activation: 'always_on',
          consumers: [{ kind: 'always_on', label: 'always_on' }],
        }}
      />,
    );
    expect(screen.getByText('Ready')).toBeInTheDocument();
    expect(screen.getByText('In use by: Always on setting')).toBeInTheDocument();
    expect(screen.queryByText('Live')).toBeNull();
  });
});
