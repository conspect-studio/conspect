// AssetDurationLabel tests — the display half of the per-asset duration
// contract. The card/inspector defect (correction-4 comfort §N8) was audio
// assets printing INVALID beside a correct 48 kHz note, and derivative clip
// cards printing undefined. Here: a valid audio asset NEVER shows INVALID; an
// unknown asset shows a meaningful unavailable state; an unmeasured length on
// a known cadence stays distinct from an unknown cadence; and a derivative
// marks its inherited grid.
import { describe, expect, it } from 'vitest';
import { screen } from '@testing-library/react';

import { renderWithProviders } from '../../test/render';
import { AssetDurationLabel } from './AssetDurationLabel';

const FORBIDDEN = /INVALID|NaN|undefined/;

describe('AssetDurationLabel', () => {
  it('renders an audio asset with its elapsed clock and sample precision', () => {
    renderWithProviders(
      <AssetDurationLabel
        asset={{ grid: { kind: 'audio', sampleRateHz: 48000 }, units: 96000 }}
      />,
    );
    expect(screen.getByText(/00:00:02\.000/)).toBeInTheDocument();
    expect(screen.getByText(/48 kHz/)).toBeInTheDocument();
    expect(screen.getByText(/SMP 96,000/)).toBeInTheDocument();
    expect(screen.queryByText(FORBIDDEN)).toBeNull();
  });

  it('renders a fractional video asset with NDF cadence precision', () => {
    renderWithProviders(
      <AssetDurationLabel
        asset={{
          grid: { kind: 'video', numerator: 30000, denominator: 1001 },
          units: 300,
        }}
      />,
    );
    expect(screen.getByText(/00:00:10:00/)).toBeInTheDocument();
    expect(
      screen.getByText(/29\.97 \(30000\/1001\) fps NDF/),
    ).toBeInTheDocument();
    expect(screen.queryByText(FORBIDDEN)).toBeNull();
  });

  it('shows a meaningful unavailable state for an unknown asset', () => {
    renderWithProviders(<AssetDurationLabel asset={{}} />);
    expect(screen.getByText('Duration unknown')).toBeInTheDocument();
    expect(screen.getByText('cadence unknown')).toBeInTheDocument();
    expect(screen.queryByText(FORBIDDEN)).toBeNull();
  });

  it('keeps a known cadence distinct from a measured length', () => {
    renderWithProviders(
      <AssetDurationLabel asset={{ grid: { kind: 'audio', sampleRateHz: 48000 } }} />,
    );
    expect(screen.getByText('Duration not measured')).toBeInTheDocument();
    expect(screen.getByText(/48 kHz/)).toBeInTheDocument();
    expect(screen.queryByText(FORBIDDEN)).toBeNull();
  });

  it('marks a derivative that inherited its parent grid', () => {
    renderWithProviders(
      <AssetDurationLabel
        asset={{ units: 625 }}
        parent={{ grid: { kind: 'video', numerator: 25, denominator: 1 }, units: 15000 }}
      />,
    );
    expect(screen.getByText(/00:00:25:00/)).toBeInTheDocument();
    expect(screen.getByText(/25 fps/)).toBeInTheDocument();
    expect(screen.getByText(/inherited from source/)).toBeInTheDocument();
    expect(screen.queryByText(FORBIDDEN)).toBeNull();
  });
});
