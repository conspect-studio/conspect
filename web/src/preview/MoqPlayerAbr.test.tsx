// The ABR state must be readable from the page, which is what the acceptance probe and any
// operator diagnostic depend on. This exercises the exposure path only: <MoqPlayer> renders
// the state the ABR produced onto its own element as data-abr-* attributes and forwards it.
import { describe, expect, it, vi } from 'vitest';
import { waitFor } from '@testing-library/react';
import { renderWithProviders } from '../test/render';
import type { AbrState } from './moqAbr';

const mocks = vi.hoisted(() => ({ start: vi.fn(), Unacknowledged: class extends Error {} }));
vi.mock('./moqPlayback', () => ({
  MoqUnavailable: class extends Error {},
  MoqReleaseUnacknowledged: mocks.Unacknowledged,
  MoqLeaseRefused: class extends Error { reason = 'moq-unavailable'; status = 503; },
  startMoqPlaybackWithRefresh: mocks.start,
}));
import { MoqPlayer } from './MoqPlayer';

const state: AbrState = {
  reason: 'probe-failed',
  measurement: 'video-track-bytes',
  arrivalBps: 1_200_000,
  instantBps: 1_150_000,
  requested: { name: 'video-540p', bitrate: 2_000_000 },
  effective: { name: 'video-360p', bitrate: 800_000 },
  ladder: [
    { name: 'video-360p', bitrate: 800_000, width: 640, height: 360, framerate: 30 },
    { name: 'video-540p', bitrate: 2_000_000, width: 960, height: 540, framerate: 30 },
  ],
  excluded: [{ name: 'video-1080p', cause: 'not-authorized-codec' }],
  probing: undefined,
  probeFailures: 1,
  probeBackoffUntilMs: 42_000,
  queuedAheadMs: 250,
  lostBytes: 4096,
  probeRttMs: 12,
  peerEstimatedRecvRateBps: 40_000_000,
  unacknowledgedSwitches: 2,
  compat: 'switch-unacknowledged',
  measuredAtMs: 11_000,
  switches: 3,
  samples: 20,
  lastSwitchAtMs: 12_000,
};

describe('MoQ player ABR exposure', () => {
  it('does not block an independent consumer when another consumer release fails', async () => {
    mocks.start.mockClear();
    let fail!: (error: Error) => void;
    mocks.start.mockImplementationOnce(() => new Promise<void>((_, reject) => { fail = reject; }))
      .mockResolvedValue(undefined);
    const onFatal = vi.fn();
    const first = renderWithProviders(<MoqPlayer api={{ acquire: vi.fn(), release: vi.fn() }}
      label="first" onUnavailable={vi.fn()} onFatal={onFatal} />);
    await waitFor(() => { expect(mocks.start).toHaveBeenCalledTimes(1); });
    renderWithProviders(<MoqPlayer api={{ acquire: vi.fn(), release: vi.fn() }}
      label="second" onUnavailable={vi.fn()} onFatal={vi.fn()} />);
    await waitFor(() => { expect(mocks.start).toHaveBeenCalledTimes(2); });
    fail(new mocks.Unacknowledged('release not acknowledged'));
    await waitFor(() => { expect(onFatal).toHaveBeenCalledTimes(1); });
    first.unmount();
    renderWithProviders(<MoqPlayer api={{ acquire: vi.fn(), release: vi.fn() }}
      label="third" onUnavailable={vi.fn()} onFatal={vi.fn()} />);
    await waitFor(() => { expect(mocks.start).toHaveBeenCalledTimes(3); });
  });

  it('reports released after a failed session whose release was acknowledged, and keeps the lease state when it was not', async () => {
    mocks.start.mockReset();
    mocks.start.mockRejectedValueOnce(new Error('MoQ session ended'));
    const clean = { onUnavailable: vi.fn(), onFatal: vi.fn(), onLeaseState: vi.fn() };
    renderWithProviders(<MoqPlayer api={{ acquire: vi.fn(), release: vi.fn() }} label="clean" {...clean} />);
    await waitFor(() => { expect(clean.onUnavailable).toHaveBeenCalledWith('session-failed'); });
    expect(clean.onLeaseState.mock.calls.map(([state]) => state as string)).toEqual(['negotiating', 'released']);
    expect(clean.onFatal).not.toHaveBeenCalled();

    mocks.start.mockRejectedValueOnce(new mocks.Unacknowledged('release not acknowledged'));
    const held = { onUnavailable: vi.fn(), onFatal: vi.fn(), onLeaseState: vi.fn() };
    renderWithProviders(<MoqPlayer api={{ acquire: vi.fn(), release: vi.fn() }} label="held" {...held} />);
    await waitFor(() => { expect(held.onUnavailable).toHaveBeenCalledWith('release-unacknowledged'); });
    expect(held.onLeaseState.mock.calls.map(([state]) => state as string)).toEqual(['negotiating']);
    expect(held.onFatal).toHaveBeenCalledTimes(1);
  });

  it('waits for release acknowledgement before a replacement acquires and carries adaptation', async () => {
    mocks.start.mockClear();
    let acknowledge!: () => void;
    const ack = new Promise<void>((resolve) => { acknowledge = resolve; });
    mocks.start.mockImplementationOnce((options: { signal: AbortSignal; onAbr: (value: AbrState) => void }) => {
      options.onAbr(state);
      return new Promise<void>((resolve) => {
        options.signal.addEventListener('abort', () => { void ack.then(resolve); });
      });
    }).mockResolvedValue(undefined);
    const first = { acquire: vi.fn(), release: vi.fn() };
    const next = { acquire: vi.fn(), release: vi.fn() };
    const onUnavailable = vi.fn();
    const onFatal = vi.fn();
    const view = renderWithProviders(<MoqPlayer api={first} label="preview" onUnavailable={onUnavailable} onFatal={onFatal} />);
    await waitFor(() => { expect(mocks.start).toHaveBeenCalledTimes(1); });
    view.rerender(<MoqPlayer api={next} label="preview" onUnavailable={onUnavailable} onFatal={onFatal} />);
    await Promise.resolve();
    expect(mocks.start).toHaveBeenCalledTimes(1);
    acknowledge();
    await waitFor(() => { expect(mocks.start).toHaveBeenCalledTimes(2); });
    expect(mocks.start.mock.calls[1]?.[0].resume).toEqual(expect.objectContaining({
      requested: state.requested, probeBackoffUntilMs: state.probeBackoffUntilMs, compat: state.compat,
    }));
  });

  it('writes requested, effective, reason and the ladder onto the player element', async () => {
    mocks.start.mockImplementation((options: { onAbr?: (value: AbrState) => void }) => {
      options.onAbr?.(state);
      return Promise.resolve();
    });
    const onAbr = vi.fn();
    const view = renderWithProviders(
      <MoqPlayer api={{ acquire: vi.fn(), release: vi.fn() }} label="preview"
        onUnavailable={(): void => { /* unused */ }} onFatal={(): void => { /* unused */ }} onAbr={onAbr} />,
    );
    const canvas = await view.findByTestId('moq-player');
    await waitFor(() => { expect(canvas.getAttribute('data-abr-reason')).toBe('probe-failed'); });
    expect(canvas.getAttribute('data-abr-requested')).toBe('video-540p');
    expect(canvas.getAttribute('data-abr-requested-bps')).toBe('2000000');
    expect(canvas.getAttribute('data-abr-effective')).toBe('video-360p');
    expect(canvas.getAttribute('data-abr-effective-bps')).toBe('800000');
    expect(canvas.getAttribute('data-abr-measurement')).toBe('video-track-bytes');
    expect(canvas.getAttribute('data-abr-arrival-bps')).toBe('1200000');
    expect(canvas.getAttribute('data-abr-instant-bps')).toBe('1150000');
    expect(canvas.getAttribute('data-abr-ladder')).toBe('video-360p:800000,video-540p:2000000');
    expect(canvas.getAttribute('data-abr-excluded')).toBe('video-1080p:not-authorized-codec');
    expect(canvas.getAttribute('data-abr-probing')).toBe('');
    expect(canvas.getAttribute('data-abr-probe-failures')).toBe('1');
    expect(canvas.getAttribute('data-abr-probe-backoff-until-ms')).toBe('42000');
    expect(canvas.getAttribute('data-abr-switches')).toBe('3');
    expect(canvas.getAttribute('data-abr-samples')).toBe('20');
    expect(canvas.getAttribute('data-abr-peer-estimate-bps')).toBe('40000000');
    expect(canvas.getAttribute('data-abr-probe-rtt-ms')).toBe('12');
    expect(canvas.getAttribute('data-abr-unacknowledged')).toBe('2');
    expect(canvas.getAttribute('data-abr-compat')).toBe('switch-unacknowledged');
    expect(onAbr).toHaveBeenCalledWith(state);
  });
});
