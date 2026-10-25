// ProgramMetersView (AUD-8 / SW-14): renders the program-bus peak/RMS meters
// from a (mocked) `audio.meter` frame; the loudness LUFS/dBTP readout is
// LoudnessMeter's job. Presentational — the realtime hook is tested separately.
import { describe, expect, it } from 'vitest';
import { screen } from '@testing-library/react';

import { renderWithProviders } from '../test/render';
import { ProgramMetersView } from './ProgramMeters';
import { meterFillPercent, parseAudioMeter } from '../realtime/useAudioMeter';
import type { AudioMeterSample } from '../realtime/useAudioMeter';
import type { AudioLoudnessSample } from '../realtime/useAudioLoudness';

function frame(over: Partial<AudioMeterSample> = {}): AudioMeterSample {
  return {
    track: 0,
    peak_db: [-6, -20.5],
    rms_db: [-12, -30],
    clip: false,
    overflow: false,
    sampled_hz: 30,
    ...over,
  };
}

describe('parseAudioMeter', () => {
  it('parses a wire frame and rejects malformed ones', () => {
    const wire = { track: 0, peak_db: [-6], rms_db: [-12], clip: true, overflow: false, sampled_hz: 30 };
    expect(parseAudioMeter(wire)?.clip).toBe(true);
    expect(parseAudioMeter({ ...wire, track: 2, input_id: 'cam' })?.input_id).toBe('cam');
    expect(parseAudioMeter({ ...wire, peak_db: 'x' })).toBeUndefined();
    expect(parseAudioMeter({ ...wire, rms_db: [null] })).toBeUndefined();
    expect(parseAudioMeter(null)).toBeUndefined();
  });
});

describe('meterFillPercent', () => {
  it('maps -60..0 dBFS onto 0..100 and clamps', () => {
    expect(meterFillPercent(0)).toBe(100);
    expect(meterFillPercent(-30)).toBe(50);
    expect(meterFillPercent(-120)).toBe(0);
    expect(meterFillPercent(6)).toBe(100);
  });
});

describe('ProgramMetersView', () => {
  it('shows a waiting state before any frame', () => {
    renderWithProviders(<ProgramMetersView status="open" current={undefined} inputs={[]} />);
    expect(screen.getByText(/waiting for meter/i)).toBeInTheDocument();
  });

  it('renders per-channel peak and RMS dBFS values from a frame', () => {
    renderWithProviders(<ProgramMetersView status="open" current={frame()} inputs={[]} />);
    expect(screen.getByTestId('meter-peak-0')).toHaveTextContent('-6.0');
    expect(screen.getByTestId('meter-peak-1')).toHaveTextContent('-20.5');
    expect(screen.getByTestId('meter-rms-0')).toHaveTextContent('-12.0');
    expect(screen.getByTestId('meter-rms-1')).toHaveTextContent('-30.0');
    expect(screen.queryByText(/clip/i)).not.toBeInTheDocument();
  });

  it('flags clipping with text, not colour alone', () => {
    renderWithProviders(<ProgramMetersView status="open" current={frame({ clip: true })} inputs={[]} />);
    expect(screen.getByText(/clip/i)).toBeInTheDocument();
  });
});

describe('ProgramMetersView loudness + per-input', () => {
  const loudness = {
    program: 0,
    momentary: -22.5,
    short_term: -23.2,
    integrated: -23.0,
    lra: 5.4,
    true_peak_dbtp: -3.1,
    target_lufs: -23,
    ceiling_dbtp: -1.5,
    tolerance_lu: 1,
    sampled_hz: 10,
  };

  it('renders LUFS (M/S/I), LRA and dBTP from a mocked loudness frame', () => {
    renderWithProviders(
      <ProgramMetersView status="open" current={frame()} inputs={[]} loudness={loudness} />,
    );
    expect(screen.getByTestId('meter-lufs-m')).toHaveTextContent('-22.5');
    expect(screen.getByTestId('meter-lufs-s')).toHaveTextContent('-23.2');
    expect(screen.getByTestId('meter-lufs-i')).toHaveTextContent('-23.0');
    expect(screen.getByTestId('meter-lra')).toHaveTextContent('5.4');
    expect(screen.getByTestId('meter-dbtp')).toHaveTextContent('-3.1');
  });

  it('shows dashes for absent loudness fields, never a fabricated number', () => {
    const gated: AudioLoudnessSample = {
      program: 0,
      short_term: -23.2,
      lra: 5.4,
      true_peak_dbtp: -3.1,
      target_lufs: -23,
      ceiling_dbtp: -1.5,
      tolerance_lu: 1,
      sampled_hz: 10,
    };
    renderWithProviders(
      <ProgramMetersView
        status="open"
        current={frame()}
        inputs={[]}
        loudness={gated}
      />,
    );
    expect(screen.getByTestId('meter-lufs-m')).toHaveTextContent('—');
    expect(screen.getByTestId('meter-lufs-i')).toHaveTextContent('—');
  });

  it('renders one meter row per input track with its source id', () => {
    renderWithProviders(
      <ProgramMetersView
        status="open"
        current={frame()}
        inputs={[
          frame({ track: 1, input_id: 'cam_a', peak_db: [-10, -11] }),
          frame({ track: 2, input_id: 'cam_b', peak_db: [-30, -31] }),
        ]}
      />,
    );
    expect(screen.getByTestId('input-meter-cam_a')).toHaveTextContent('-10.0');
    expect(screen.getByTestId('input-meter-cam_b')).toHaveTextContent('-30.0');
  });
});
