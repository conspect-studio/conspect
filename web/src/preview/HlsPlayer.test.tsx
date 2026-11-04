import { describe, expect, it } from 'vitest';
import { waitFor } from '@testing-library/react';
import { renderWithProviders } from '../test/render';
import { HlsPlayer } from './HlsPlayer';

describe('HLS output player', () => {
  it('provides muted inline playback controls and labels delivery latency', async () => {
    const view = renderWithProviders(<HlsPlayer url="/hls/program/program.m3u8" label="Program output" />);
    const video = view.getByLabelText('Program output');
    expect(video).toHaveAttribute('controls');
    expect(video).toHaveAttribute('playsinline');
    expect(view.getByText(/HLS output.*delayed/)).toBeInTheDocument();
    await waitFor(() => { expect(view.getByRole('status')).toHaveTextContent('Playback unavailable'); });
  });
});
