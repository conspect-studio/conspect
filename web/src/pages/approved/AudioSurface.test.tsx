import { afterEach, describe, expect, it, vi } from 'vitest';
import { act, screen } from '@testing-library/react';

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
  receivedAt: Date.now(),
  current: {
    program: 0,
    short_term: -17.84,
    target_lufs: -23,
    ceiling_dbtp: -1,
    tolerance_lu: 1,
    sampled_hz: 1,
  },
};

afterEach(() => {
  vi.useRealTimers();
});

describe('loudness freshness', () => {
  const sample = (over: object) => ({
    program: 0,
    target_lufs: -23,
    ceiling_dbtp: -1,
    tolerance_lu: 1,
    sampled_hz: 1,
    ...over,
  });

  it('shows Unknown after disconnect, not the last sample', () => {
    const closed: AudioLoudnessState = { ...live, status: 'closed' };
    renderWithProviders(<AudioPhoneMaster loudness={closed} />);
    expect(screen.getByTestId('audio-phone-master')).toHaveTextContent('— LUFS');
    expect(screen.getByTestId('audio-phone-master')).not.toHaveTextContent('-17.8');
  });

  it('goes Unknown when the stream stalls past the deadline', () => {
    vi.useFakeTimers();
    renderWithProviders(
      <AudioSurface sub="sub" loudness={{ ...live, receivedAt: Date.now() }}>
        <p>body</p>
      </AudioSurface>,
    );
    expect(screen.getByTestId('audio-loudness-readout')).toHaveTextContent('-17.8 LUFS');
    act(() => {
      vi.advanceTimersByTime(7000);
    });
    expect(screen.getByTestId('audio-loudness-readout')).toHaveTextContent('— LUFS');
  });

  it('never shows momentary under the short-term label', () => {
    const momentaryOnly: AudioLoudnessState = {
      ...none,
      receivedAt: Date.now(),
      current: sample({ momentary: -20.2 }),
    };
    renderWithProviders(<AudioPhoneMaster loudness={momentaryOnly} />);
    expect(screen.getByTestId('audio-phone-master')).toHaveTextContent('— LUFS');
    renderWithProviders(
      <AudioSurface sub="sub" loudness={momentaryOnly}>
        <p>body</p>
      </AudioSurface>,
    );
    const r = screen.getByTestId('audio-loudness-readout');
    expect(r).toHaveTextContent(/short-term\)\s*— LUFS/);
    expect(screen.getByTestId('audio-momentary-readout')).toHaveTextContent('-20.2 LUFS');
  });
});

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
    // The master strip is shown but every control is disabled (no invented function).
    const buttons = screen.getAllByRole('button');
    expect(buttons.length).toBeGreaterThan(0);
    for (const b of buttons) expect(b).toBeDisabled();
    expect(screen.getByTestId('audio-master-unavailable')).toHaveTextContent(
      'Master gain/mute unavailable: control plane exposes only per-input gain and mute in the saved audio document',
    );
    expect(screen.getByTestId('audio-master-fader')).toHaveAttribute('aria-disabled', 'true');
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
