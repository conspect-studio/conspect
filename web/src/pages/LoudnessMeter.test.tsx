// LoudnessMeter widget (AUD-8): renders the program-bus EBU R128 compliance
// meter from a loudness sample + the client-side ballistics. These tests inject
// the loudness STATE directly (the realtime hook is exercised separately in
// useAudioLoudness.test.tsx), so they assert the rendering + compliance colour
// zoning is correct for: live in-spec audio, drifting audio, gated silence, and
// an over-ceiling true-peak.
import { describe, expect, it } from 'vitest';
import { screen, within } from '@testing-library/react';

import { renderWithProviders } from '../test/render';
import { LoudnessMeterView } from './LoudnessMeter';
import type { AudioLoudnessSample } from '../realtime/useAudioLoudness';

function sample(over: Partial<AudioLoudnessSample> = {}): AudioLoudnessSample {
  return {
    program: 0,
    target_lufs: -23,
    ceiling_dbtp: -1.5,
    tolerance_lu: 1,
    sampled_hz: 10,
    ...over,
  };
}

describe('LoudnessMeterView', () => {
  it('shows a waiting state before any sample arrives', () => {
    renderWithProviders(
      <LoudnessMeterView
        status="connecting"
        current={undefined}
        displayMomentary={undefined}
        heldPeakDbtp={undefined}
      />,
    );
    expect(screen.getByText(/waiting for loudness/i)).toBeInTheDocument();
  });

  it('renders the measured M/S/I/LRA/dBTP values and the compliance target', () => {
    renderWithProviders(
      <LoudnessMeterView
        status="open"
        current={sample({
          momentary: -22.5,
          short_term: -23.2,
          integrated: -23.0,
          lra: 4.5,
          true_peak_dbtp: -2.3,
        })}
        displayMomentary={-22.5}
        heldPeakDbtp={-2.3}
      />,
    );
    // The integrated readout is the headline compliance number.
    const integrated = screen.getByTestId('loudness-integrated');
    expect(integrated).toHaveTextContent('-23.0');
    expect(integrated).toHaveTextContent(/LUFS/i);
    // Short-term + momentary + LRA + dBTP are all shown.
    expect(screen.getByTestId('loudness-short-term')).toHaveTextContent('-23.2');
    expect(screen.getByTestId('loudness-momentary')).toHaveTextContent('-22.5');
    expect(screen.getByTestId('loudness-lra')).toHaveTextContent('4.5');
    expect(screen.getByTestId('loudness-true-peak')).toHaveTextContent('-2.3');
    // The compliance target rides the view (so the operator sees the spec).
    expect(screen.getByText(/-23(\.0)? LUFS/)).toBeInTheDocument();
  });

  it('marks integrated as in-spec within tolerance (text + status, not colour alone)', () => {
    renderWithProviders(
      <LoudnessMeterView
        status="open"
        current={sample({ integrated: -23.0 })}
        displayMomentary={-23}
        heldPeakDbtp={undefined}
      />,
    );
    const integrated = screen.getByTestId('loudness-integrated');
    // WCAG: a textual status accompanies the colour — assert the TEXT.
    expect(within(integrated).getByText(/in spec/i)).toBeInTheDocument();
  });

  it('marks integrated as out of spec when far from target', () => {
    renderWithProviders(
      <LoudnessMeterView
        status="open"
        current={sample({ integrated: -18.0 })}
        displayMomentary={-18}
        heldPeakDbtp={undefined}
      />,
    );
    const integrated = screen.getByTestId('loudness-integrated');
    expect(within(integrated).getByText(/out of spec/i)).toBeInTheDocument();
  });

  it('shows a clip/over-ceiling warning when the true-peak breaches the ceiling', () => {
    renderWithProviders(
      <LoudnessMeterView
        status="open"
        current={sample({ integrated: -23, true_peak_dbtp: -0.5 })}
        displayMomentary={-23}
        heldPeakDbtp={-0.5}
      />,
    );
    const peak = screen.getByTestId('loudness-true-peak');
    expect(within(peak).getByText(/over/i)).toBeInTheDocument();
  });

  it('shows gated silence (no measurement) without fabricating a value', () => {
    renderWithProviders(
      <LoudnessMeterView
        status="open"
        current={sample()}
        displayMomentary={undefined}
        heldPeakDbtp={undefined}
      />,
    );
    const integrated = screen.getByTestId('loudness-integrated');
    // No fabricated number: a dash placeholder, and the zone reads "silent".
    expect(integrated).toHaveTextContent('—');
    expect(within(integrated).getByText(/silent|no signal|gated/i)).toBeInTheDocument();
  });
});
