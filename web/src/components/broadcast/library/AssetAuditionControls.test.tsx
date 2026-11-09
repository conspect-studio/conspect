// AssetAuditionControls tests — the honest audition contract (audit §6:
// NO audition endpoint, waveform-peaks service or spectrogram exists):
//   * an unsupported audition gate renders the unavailable panel with the
//     reason — no transport, never a fake "working" state;
//   * playback is presentational and labeled LOCAL — play/pause and stop
//     fire intents only; the label reflects the caller's playing prop;
//     an absent position renders the honest not-reported fallback;
//   * channel + analysis-mode selection are controlled: firing an intent
//     changes nothing until the caller re-renders; a mode without a
//     service renders disabled with its visible reason;
//   * the analysis surface renders ONLY the caller's real supplied peaks
//     (clamped 0..1 — NaN degrades, never renders); `null` analysis or an
//     empty window renders the honest no-analysis / not-measured lines —
//     nothing synthetic is ever generated.
import { describe, expect, it, vi } from 'vitest';
import { fireEvent, screen, within } from '@testing-library/react';

import { renderWithProviders } from '../../../test/render';
import { AssetAuditionControls } from './AssetAuditionControls';
import type { AnalysisModeOption } from './AssetAuditionControls';
import type { ActionGate } from './types';

const SUPPORTED: ActionGate = { supported: true, unavailableReason: null };

const MODES: readonly AnalysisModeOption[] = [
  { id: 'waveform', label: 'Waveform', gate: SUPPORTED },
  {
    id: 'spectrogram',
    label: 'Spectrogram',
    gate: {
      supported: false,
      unavailableReason: 'No spectrogram service exists today.',
    },
  },
];

function renderAudition(overrides: {
  auditionGate?: ActionGate;
  transport?: { playing: boolean; positionLabel: string | null };
  channels?: readonly { id: string; label: string }[];
  activeChannelId?: string | null;
  analysisModes?: readonly AnalysisModeOption[];
  activeAnalysisModeId?: string | null;
  analysis?:
    | {
        kind: 'waveform' | 'spectrogram';
        peaks: readonly number[];
        measuredNote?: string | null;
      }
    | null;
} = {}) {
  const onPlayPauseIntent = vi.fn();
  const onStopIntent = vi.fn();
  const onChannelChange = vi.fn();
  const onAnalysisModeChange = vi.fn();
  const { container } = renderWithProviders(
    <AssetAuditionControls
      assetName="Interview 12"
      auditionGate={overrides.auditionGate ?? SUPPORTED}
      transport={
        overrides.transport ?? { playing: false, positionLabel: null }
      }
      onPlayPauseIntent={onPlayPauseIntent}
      onStopIntent={onStopIntent}
      channels={
        overrides.channels ?? [
          { id: 'stereo', label: 'Stereo' },
          { id: 'left', label: 'Left' },
        ]
      }
      activeChannelId={overrides.activeChannelId ?? 'stereo'}
      onChannelChange={onChannelChange}
      analysisModes={overrides.analysisModes ?? MODES}
      activeAnalysisModeId={overrides.activeAnalysisModeId ?? 'waveform'}
      onAnalysisModeChange={onAnalysisModeChange}
      analysis={overrides.analysis ?? null}
    />,
  );
  return {
    container,
    onPlayPauseIntent,
    onStopIntent,
    onChannelChange,
    onAnalysisModeChange,
  };
}

function panel(container: HTMLElement): HTMLElement {
  return container.querySelector<HTMLElement>(
    '[data-testid="asset-audition-controls"]',
  )!;
}

describe('AssetAuditionControls — capability honesty', () => {
  it('an unsupported audition gate renders the unavailable panel with the reason', () => {
    const helpers = renderAudition({
      auditionGate: {
        supported: false,
        unavailableReason: 'No per-asset audition endpoint exists today.',
      },
    });
    const section = panel(helpers.container);
    expect(section.dataset.state).toBe('unavailable');
    expect(section).toHaveTextContent(/Audition is not available/);
    expect(section).toHaveTextContent(/No per-asset audition endpoint exists today\./);
    expect(section.textContent).not.toMatch(/working/i);
    // No transport chrome renders for a missing contract.
    expect(screen.queryByRole('button', { name: /^Play$/ })).toBeNull();
    expect(screen.queryByRole('button', { name: /^Stop$/ })).toBeNull();
  });
});

describe('AssetAuditionControls — presentational local transport', () => {
  it('labels the surface LOCAL and reflects the caller’s playing state', () => {
    const helpers = renderAudition({
      transport: { playing: true, positionLabel: '00:00:12:04' },
    });
    const section = panel(helpers.container);
    expect(section.dataset.playing).toBe('true');
    expect(section).toHaveTextContent(/Local audition — not a program path/);
    expect(within(section).getByRole('button', { name: /^Pause$/ })).toBeTruthy();
    expect(section).toHaveTextContent(/00:00:12:04/);
  });

  it('fires play/pause and stop intents only — nothing applies inside', () => {
    const helpers = renderAudition();
    fireEvent.click(screen.getByRole('button', { name: /^Play$/ }));
    expect(helpers.onPlayPauseIntent).toHaveBeenCalledTimes(1);
    expect(helpers.onStopIntent).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole('button', { name: /^Stop$/ }));
    expect(helpers.onStopIntent).toHaveBeenCalledTimes(1);
    // Controlled: the label still shows Play (the caller owns the state).
    expect(screen.getByRole('button', { name: /^Play$/ })).toBeTruthy();
  });

  it('an absent position renders the honest not-reported fallback', () => {
    const { container } = renderAudition();
    expect(panel(container)).toHaveTextContent(/Position not reported/);
  });
});

describe('AssetAuditionControls — controlled channel and mode selection', () => {
  it('channel chips are controlled and emit intents, keeping 48px targets', () => {
    const helpers = renderAudition({ activeChannelId: 'left' });
    const channels = within(helpers.container).getByTestId('audition-channels');
    const left = within(channels).getByRole('button', { name: /^Left$/ });
    const stereo = within(channels).getByRole('button', { name: /^Stereo$/ });
    expect(left).toHaveAttribute('aria-pressed', 'true');
    expect(stereo).toHaveAttribute('aria-pressed', 'false');
    expect(stereo.className).toMatch(/\bmin-h-12\b/);
    fireEvent.click(stereo);
    expect(helpers.onChannelChange).toHaveBeenCalledWith('stereo');
    // No silent mutation: the pressed state is still the prop's.
    expect(stereo).toHaveAttribute('aria-pressed', 'false');
  });

  it('a mode without a service renders disabled with its visible reason', () => {
    const helpers = renderAudition();
    const spectrogram = screen.getByRole('button', { name: /^Spectrogram$/ });
    expect(spectrogram).toBeDisabled();
    expect(spectrogram.getAttribute('title')).toBe(
      'No spectrogram service exists today.',
    );
    fireEvent.click(spectrogram);
    expect(helpers.onAnalysisModeChange).not.toHaveBeenCalled();
  });

  it('the active mode renders as the selected variant and switching emits an intent', () => {
    const helpers = renderAudition();
    const waveform = screen.getByRole('button', { name: /^Waveform$/ });
    expect(waveform.className).not.toMatch(/border-input/);
    fireEvent.click(waveform);
    expect(helpers.onAnalysisModeChange).toHaveBeenCalledWith('waveform');
  });
});

describe('AssetAuditionControls — analysis surface honesty', () => {
  it('null analysis renders the honest no-analysis line — never synthetic data', () => {
    const { container } = renderAudition();
    const surface = within(container).getByTestId('audition-analysis-surface');
    expect(surface.dataset.analysisKind).toBe('none');
    expect(surface).toHaveTextContent(
      /No analysis for this mode — no analysis service supplied it\./,
    );
    expect(
      within(surface).queryAllByTestId('audition-analysis-bar'),
    ).toHaveLength(0);
  });

  it('renders ONLY the caller’s real supplied peaks, clamped to 0..1', () => {
    const { container } = renderAudition({
      analysis: {
        kind: 'waveform',
        peaks: [0.5, 1, -3, Number.NaN, 0.25],
        measuredNote: 'Peak envelope, −12 dBFS window',
      },
    });
    const surface = within(container).getByTestId('audition-analysis-surface');
    expect(surface.dataset.analysisKind).toBe('waveform');
    const bars = within(surface).getAllByTestId('audition-analysis-bar');
    expect(bars).toHaveLength(5);
    // Real values scale; invalid values degrade to the flat floor — never NaN.
    expect(bars[0]).toHaveStyle({ height: '40px' });
    expect(bars[1]).toHaveStyle({ height: '80px' });
    expect(bars[2]).toHaveStyle({ height: '2px' });
    expect(bars[3]).toHaveStyle({ height: '2px' });
    expect(bars[4]).toHaveStyle({ height: '20px' });
    expect(surface).toHaveTextContent(/Peak envelope, −12 dBFS window/);
  });

  it('an empty analysis window renders not-measured, not a fake surface', () => {
    const { container } = renderAudition({
      analysis: { kind: 'waveform', peaks: [] },
    });
    const surface = within(container).getByTestId('audition-analysis-surface');
    expect(surface).toHaveTextContent(/Analysis not measured yet\./);
    expect(
      within(surface).queryAllByTestId('audition-analysis-bar'),
    ).toHaveLength(0);
  });
});
