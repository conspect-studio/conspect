import { describe, expect, it } from 'vitest';
import { screen } from '@testing-library/react';

import { renderWithProviders } from '../../test/render';
import type { AudioLoudnessState } from '../../realtime/useAudioLoudness';
import {
  AudioMasterRegion,
  AudioPhoneMaster,
  AudioSurface,
} from './AudioSurface';

const none: AudioLoudnessState = {
  status: 'open',
  current: undefined,
  displayMomentary: undefined,
  heldPeakDbtp: undefined,
};
const live: AudioLoudnessState = {
  ...none,
  current: {
    program: 0,
    short_term: -17.84,
    target_lufs: -23,
    ceiling_dbtp: -1,
    tolerance_lu: 1,
    sampled_hz: 1,
  },
};

describe('AudioSurface', () => {
  it('is namespaced and shows an honest dash without loudness telemetry', () => {
    renderWithProviders(
      <AudioSurface sub="sub" loudness={none}>
        <p>body</p>
      </AudioSurface>,
    );
    const root = document.querySelector('[data-approved-page="audio"]');
    expect(root).not.toBeNull();
    expect(screen.getByRole('heading', { level: 1 })).toHaveTextContent('Audio mixer');
    expect(screen.getByTestId('audio-loudness-readout')).toHaveTextContent('— LUFS');
  });

  it('shows only real measured loudness', () => {
    renderWithProviders(
      <AudioSurface sub="sub" loudness={live}>
        <p>body</p>
      </AudioSurface>,
    );
    expect(screen.getByTestId('audio-loudness-readout')).toHaveTextContent('-17.8 LUFS');
  });

  it('keeps master and monitor regions with specific unavailable reasons', () => {
    renderWithProviders(<AudioMasterRegion />);
    expect(screen.getByTestId('audio-master-unavailable')).toHaveTextContent(/unavailable/i);
    expect(screen.getByTestId('audio-monitor-unavailable')).toHaveTextContent(/LISTEN.*unavailable/i);
    expect(screen.queryByRole('button')).toBeNull();
  });

  it('phone master offers no invented mute control', () => {
    renderWithProviders(<AudioPhoneMaster loudness={none} />);
    expect(screen.queryByRole('button')).toBeNull();
    expect(screen.getByTestId('audio-phone-master')).toHaveTextContent(/master mute unavailable/);
  });

  it('hosts the live program meters inside the PROGRAM region', () => {
    renderWithProviders(
      <AudioMasterRegion>
        <p data-testid="hosted">meters</p>
      </AudioMasterRegion>,
    );
    expect(screen.getByTestId('audio-master-region')).toContainElement(
      screen.getByTestId('hosted'),
    );
  });
});
