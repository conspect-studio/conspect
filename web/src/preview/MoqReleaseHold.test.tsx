// An unacknowledged MoQ release: the server may hold the lease until its deadline.
// These tests use the real <MoqPlayer> and the real <PreviewSurface>; only the
// playback function, the WHEP player and the still poll are replaced.
import { afterEach, describe, expect, it, vi } from 'vitest';
import { useState, type JSX } from 'react';
import { act, fireEvent } from '@testing-library/react';
import { renderWithProviders } from '../test/render';

const mocks = vi.hoisted(() => ({ start: vi.fn(), Unacknowledged: class extends Error {} }));
vi.mock('./moqPlayback', () => ({
  MoqUnavailable: class extends Error {},
  MoqLeaseRefused: class extends Error { reason = 'moq-unavailable'; status = 503; },
  MoqReleaseUnacknowledged: mocks.Unacknowledged,
  startMoqPlaybackWithRefresh: mocks.start,
}));
vi.mock('./WhepPlayer', () => ({ WhepPlayer: () => <div data-testid="whep" /> }));
vi.mock('./useJpegPreview', () => ({ useJpegStill: () => ({ src: undefined, state: 'loading' }), useJpegPreview: () => undefined }));
import { MOQ_UNACKNOWLEDGED_HOLD_MS } from './MoqPlayer';
import { PreviewSurface } from './PreviewSurface';

const both = { moq: true, webrtc: true, fallback: 'jpeg', scopes: { program: { whep: true }, inputs: { whep: true }, outputs: { whep: true } } };
const stillsOnly = { moq: false, webrtc: false, fallback: 'jpeg', scopes: { program: { whep: false }, inputs: { whep: false }, outputs: { whep: false } } };
const advance = (ms: number): Promise<void> => act(async () => { await vi.advanceTimersByTimeAsync(ms); });
const releasedCalls = (spy: ReturnType<typeof vi.fn>): number => spy.mock.calls.filter(([state]) => state === 'released').length;
const seen: { transport: string; reason: string | undefined }[] = [];
const surface = (onLeaseState: (state: string) => void) => { seen.length = 0; return renderWithProviders(
  <PreviewSurface scope="program" whepEndpoint="/api/v1/preview/program/whep" jpegPath="/jpeg" label="preview"
    capabilities={both} onLeaseState={onLeaseState}
    onTransport={(transport, reason) => { seen.push({ transport, reason }); }} />); };

const programSurface = (onLease: (state: string) => void): JSX.Element => (
  <PreviewSurface scope="program" whepEndpoint="/api/v1/preview/program/whep" jpegPath="/jpeg" label="preview"
    capabilities={both} onLeaseState={onLease} />);

/** Two surfaces for one source that report to one lease state; each can be hidden. */
function Pair({ onLease }: { readonly onLease: (state: string) => void }): JSX.Element {
  const [first, setFirst] = useState(true);
  const [second, setSecond] = useState(false);
  return <>
    <button type="button" onClick={() => { setFirst((value) => !value); }}>first</button>
    <button type="button" onClick={() => { setSecond((value) => !value); }}>second</button>
    {first ? programSurface(onLease) : null}
    {second ? programSurface(onLease) : null}
  </>;
}

/** A consumer like the monitoring page: it shows or hides one Program surface and keeps its lease state. */
function Consumer({ onLease }: { readonly onLease: (state: string) => void }): JSX.Element {
  const [shown, setShown] = useState(true);
  const [capabilities, setCapabilities] = useState(both);
  return <>
    <button type="button" onClick={() => { setShown((value) => !value); }}>toggle</button>
    <button type="button" onClick={() => { setCapabilities(stillsOnly); }}>stills only</button>
    {shown ? <PreviewSurface scope="program" whepEndpoint="/api/v1/preview/program/whep" jpegPath="/jpeg" label="preview"
      capabilities={capabilities} onLeaseState={onLease} onSessionReleased={() => { onLease('session-released'); }} /> : null}
  </>;
}

afterEach(() => { vi.useRealTimers(); });

describe('unacknowledged MoQ release', () => {
  it('shows stills while the lease can still be held, then opens WebRTC and reports released once', async () => {
    vi.useFakeTimers();
    mocks.start.mockReset();
    mocks.start.mockRejectedValue(new mocks.Unacknowledged('not acknowledged'));
    const onLeaseState = vi.fn();
    const view = surface(onLeaseState);
    await advance(0);
    expect(view.getByTestId('jpeg-rung')).toBeInTheDocument();
    expect(view.queryByTestId('whep')).toBeNull();
    await advance(MOQ_UNACKNOWLEDGED_HOLD_MS - 1);
    expect(view.queryByTestId('whep')).toBeNull();
    expect(releasedCalls(onLeaseState)).toBe(0);
    expect(seen[seen.length - 1]).toEqual({ transport: 'jpeg', reason: 'release-unacknowledged' });
    await advance(1);
    expect(view.getByTestId('whep')).toBeInTheDocument();
    expect(releasedCalls(onLeaseState)).toBe(1);
    expect(seen[seen.length - 1]).toEqual({ transport: 'webrtc', reason: 'release-unacknowledged' });
  });

  it('reports released after the hold when the surface was unmounted before the release failed', async () => {
    vi.useFakeTimers();
    mocks.start.mockReset();
    let fail!: (error: Error) => void;
    mocks.start.mockImplementation(() => new Promise<void>((_, reject) => { fail = reject; }));
    const onLeaseState = vi.fn();
    const view = surface(onLeaseState);
    await advance(0);
    expect(onLeaseState).toHaveBeenLastCalledWith('negotiating');
    view.unmount();
    fail(new mocks.Unacknowledged('not acknowledged'));
    await advance(MOQ_UNACKNOWLEDGED_HOLD_MS - 1);
    expect(releasedCalls(onLeaseState)).toBe(0);
    await advance(1);
    expect(releasedCalls(onLeaseState)).toBe(1);
  });

  it('does not let the old hold mark a new session released when the surface is mounted again within the hold', async () => {
    vi.useFakeTimers();
    mocks.start.mockReset();
    let failFirst!: (error: Error) => void;
    // First session: its release is not acknowledged. Second session: active and still running.
    mocks.start
      .mockImplementationOnce((options: { onActive: () => void }) => { options.onActive(); return new Promise<void>((_, reject) => { failFirst = reject; }); })
      .mockImplementationOnce((options: { onActive: () => void }) => { options.onActive(); return new Promise<void>(() => { /* stays active */ }); });
    const onLease = vi.fn();
    const view = renderWithProviders(<Consumer onLease={onLease} />);
    await advance(0);
    expect(onLease).toHaveBeenLastCalledWith('active');
    fireEvent.click(view.getByText('toggle')); // the consumer hides the surface (for example to open an input)
    failFirst(new mocks.Unacknowledged('not acknowledged'));
    await advance(1000);
    fireEvent.click(view.getByText('toggle')); // and shows it again before the hold ends
    await advance(0);
    expect(mocks.start).toHaveBeenCalledTimes(2);
    expect(onLease).toHaveBeenLastCalledWith('active');
    const before = onLease.mock.calls.length;
    await advance(MOQ_UNACKNOWLEDGED_HOLD_MS);
    // The old session's hold ended. The new session is active, and stays so for the consumer.
    expect(onLease.mock.calls.slice(before)).toEqual([]);
    expect(onLease).toHaveBeenLastCalledWith('active');
  });

  it('still reports the old release when the surface comes back on stills and holds no session', async () => {
    vi.useFakeTimers();
    mocks.start.mockReset();
    let acknowledge!: () => void;
    // Active until unmounted; the release is acknowledged late.
    mocks.start.mockImplementation((options: { onActive: () => void; signal: AbortSignal }) => {
      options.onActive();
      return new Promise<void>((resolve) => { options.signal.addEventListener('abort', () => { acknowledge = resolve; }); });
    });
    const onLease = vi.fn();
    const view = renderWithProviders(<Consumer onLease={onLease} />);
    await advance(0);
    expect(onLease).toHaveBeenLastCalledWith('active');
    fireEvent.click(view.getByText('toggle')); // hidden: the release is in flight
    fireEvent.click(view.getByText('stills only')); // the server now offers stills only
    fireEvent.click(view.getByText('toggle')); // shown again, on stills: no session, no lease reports
    await advance(0);
    expect(view.getByTestId('jpeg-rung')).toBeInTheDocument();
    expect(mocks.start).toHaveBeenCalledTimes(1);
    expect(releasedCalls(onLease)).toBe(0);
    acknowledge();
    await advance(0);
    // Without this report the consumer would believe the old lease is active for ever.
    expect(releasedCalls(onLease)).toBe(1);
  });

  it('with two surfaces for one source, a release reaches the consumer only when the last one is gone', async () => {
    vi.useFakeTimers();
    mocks.start.mockReset();
    // Each session is active until its surface is unmounted; its release is then acknowledged.
    mocks.start.mockImplementation((options: { onActive: () => void; signal: AbortSignal }) => {
      options.onActive();
      return new Promise<void>((resolve) => { options.signal.addEventListener('abort', () => { resolve(); }); });
    });
    const onLease = vi.fn();
    const view = renderWithProviders(<Pair onLease={onLease} />);
    await advance(0);
    fireEvent.click(view.getByText('second')); // a second surface for the same source mounts
    await advance(0);
    expect(mocks.start).toHaveBeenCalledTimes(2);
    fireEvent.click(view.getByText('second')); // and is hidden again; the first is still active
    await advance(0);
    expect(releasedCalls(onLease)).toBe(0);
    expect(onLease).toHaveBeenLastCalledWith('active');
    fireEvent.click(view.getByText('first')); // now the last surface goes
    await advance(0);
    expect(releasedCalls(onLease)).toBe(1);
  });
});
