// ProgramMetersView (AUD-8 / SW-14): renders the program-bus peak/RMS meters
// from a (mocked) `audio.meter` frame; the loudness LUFS/dBTP readout is
// LoudnessMeter's job. Presentational — the realtime hook is tested separately.
import { describe, expect, it } from 'vitest';
import { screen } from '@testing-library/react';

import { renderWithProviders } from '../test/render';
import { ProgramMetersView } from './ProgramMeters';
import { meterFillPercent, parseAudioMeter } from '../realtime/useAudioMeter';
import type { AudioMeterSample } from '../realtime/useAudioMeter';

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
    renderWithProviders(<ProgramMetersView status="open" current={undefined} />);
    expect(screen.getByText(/waiting for meter/i)).toBeInTheDocument();
  });

  it('renders per-channel peak and RMS dBFS values from a frame', () => {
    renderWithProviders(<ProgramMetersView status="open" current={frame()} />);
    expect(screen.getByTestId('meter-peak-0')).toHaveTextContent('-6.0');
    expect(screen.getByTestId('meter-peak-1')).toHaveTextContent('-20.5');
    expect(screen.getByTestId('meter-rms-0')).toHaveTextContent('-12.0');
    expect(screen.getByTestId('meter-rms-1')).toHaveTextContent('-30.0');
    expect(screen.queryByText(/clip/i)).not.toBeInTheDocument();
  });

  it('flags clipping with text, not colour alone', () => {
    renderWithProviders(<ProgramMetersView status="open" current={frame({ clip: true })} />);
    expect(screen.getByText(/clip/i)).toBeInTheDocument();
  });
});
