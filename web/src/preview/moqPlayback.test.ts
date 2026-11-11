import type * as ConnectionModule from './moqConnection';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { Signal } from '@moq/signals';
interface MockSignals {
  catalog: Signal<unknown>; track: Signal<unknown>; config: Signal<unknown>; available: Signal<unknown>;
  error: Signal<unknown>; stats: Signal<unknown>; stalled: Signal<unknown>; buffered: Signal<unknown>;
  timestamp: Signal<unknown>; frame: Signal<unknown>;
}
const mocks = vi.hoisted(() => ({ supported: vi.fn(), connect: vi.fn(), close: vi.fn(), playerClose: vi.fn(), playerProps: vi.fn(), signals: undefined as MockSignals | undefined }));
vi.mock('@moq/net', () => ({ Connection: { isWebTransportSupported: mocks.supported }, Origin: { Producer: class { close = vi.fn(); } }, Path: { from: (s: string) => s } }));
// The mocked player still carries the real pinned signal surfaces, so the ABR sampler is
// exercised against the same reads production performs.
vi.mock('@moq/watch', async () => {
  const { Signal } = await import('@moq/signals');
  const signals = {
    catalog: new Signal<unknown>(undefined), track: new Signal<unknown>(undefined),
    config: new Signal<unknown>(undefined), available: new Signal<unknown>({}),
    error: new Signal<unknown>(undefined), stats: new Signal<unknown>(undefined),
    stalled: new Signal<unknown>(false), buffered: new Signal<unknown>([]),
    timestamp: new Signal<unknown>(undefined), frame: new Signal<unknown>(undefined),
  };
  mocks.signals = signals;
  return { Player: class {
    close = mocks.playerClose;
    broadcast = { out: { catalog: signals.catalog } };
    video = { out: { stats: signals.stats, stalled: signals.stalled, buffered: signals.buffered, frame: signals.frame },
      source: { out: { available: signals.available, track: signals.track, config: signals.config, error: signals.error } } };
    renderer = { out: { timestamp: signals.timestamp } };
    constructor(props: unknown) { mocks.playerProps(props); }
  } };
});
vi.mock('./moqConnection', async (original) => ({ ...await original<typeof ConnectionModule>(), connectMoq: mocks.connect }));
import { MoqUnavailable, startMoqPlayback, startMoqPlaybackWithRefresh, type MoqLease } from './moqPlayback';
import { AbrController } from './moqAbr';
const lease: MoqLease = { lease_id: 'a', generation: 1, expires_at: new Date(Date.now()+60000).toISOString(), expires_in_ms: 60000, url: 'https://relay.example', broadcast:'program', protocol:'moq-transport-18', video:{codec:'vp8', coded_width:320, coded_height:180}, audio:[],video_alternatives:[] };
afterEach(() => {
  vi.unstubAllGlobals(); vi.resetAllMocks();
  mocks.signals?.catalog.set(undefined);
});
function setup(supported = true) {
 mocks.supported.mockReturnValue(true);
 vi.stubGlobal('VideoDecoder', { isConfigSupported: vi.fn().mockResolvedValue({supported}) });
 const api = { acquire: vi.fn().mockResolvedValue(lease), release: vi.fn().mockResolvedValue(undefined) };
 const controller = new AbortController();
 return { api, controller, options: { api, signal:controller.signal, canvas:document.createElement('canvas'), audio:false, onActive:vi.fn() } };
}
describe('MoQ playback lease ownership', () => {
 it('does not allocate when WebTransport is unavailable', async () => {
  const {options,api} = setup(); mocks.supported.mockReturnValue(false);
  await expect(startMoqPlayback(options)).rejects.toBeInstanceOf(MoqUnavailable);
  expect(api.acquire).not.toHaveBeenCalled();
 });
 it('releases an unsupported codec before permitting fallback', async () => {
  const {options,api} = setup(false);
  await expect(startMoqPlayback(options)).rejects.toBeInstanceOf(MoqUnavailable);
  expect(api.release).toHaveBeenCalledWith(lease); expect(mocks.connect).not.toHaveBeenCalled();
 });
 it('does not permit fallback when release fails', async () => {
  const {options,api} = setup(false); api.release.mockRejectedValue(new Error('not revoked'));
  await expect(startMoqPlayback(options)).rejects.toThrow('not revoked');
 });
 it('does not classify authorization or capacity as unavailable', async () => {
  const {options,api} = setup(); api.acquire.mockRejectedValue(new Error('forbidden'));
  await expect(startMoqPlayback(options)).rejects.toThrow('forbidden'); expect(api.release).not.toHaveBeenCalled();
 });
 it('wires the live session probe and an ABR target, samples on the interval, and stops on release', async () => {
  vi.useFakeTimers();
  const observed = vi.spyOn(AbrController.prototype, 'observe');
  try {
   const {options,api,controller} = setup();
   api.acquire.mockResolvedValue({ ...lease, video_alternatives: [
     { track: 'video-low', codec: 'vp8', coded_width: 320, coded_height: 180,
       bitrate_bps: 800000, cadence_numerator: 30, cadence_denominator: 1 },
     { track: 'video-high', codec: 'vp8', coded_width: 640, coded_height: 360,
       bitrate_bps: 4000000, cadence_numerator: 30, cadence_denominator: 1 },
   ] });
   let ended!: () => void;
   const closed = new Promise<null>((resolve) => { ended = () => { resolve(null); }; });
   const probe = new Signal<{ rtt?: number }>({ rtt: 12 });
   const stats = vi.fn().mockResolvedValue({ bytesReceived: 0 });
   mocks.close.mockImplementation(() => { ended(); });
   mocks.connect.mockResolvedValue({closed, close:mocks.close, transport:'webtransport', probe, stats});
   const playback = startMoqPlayback(options);
   await vi.advanceTimersByTimeAsync(0);
   expect(mocks.playerProps).toHaveBeenCalledOnce();
   // The ABR takes one sample as soon as it starts, before the first interval elapses.
   expect(stats.mock.calls.length).toBeGreaterThanOrEqual(1);
   const props = mocks.playerProps.mock.calls[0]?.[0] as { probe?: unknown; target?: { peek(): unknown } } | undefined;
   // The pinned `Established.probe` getter reaches the player (Sync's RTT sizing), and the
   // ABR owns an explicit target signal, unset until a catalog is known.
   expect(props?.probe).toBe(probe);
   expect(props?.target?.peek()).toBeUndefined();
   // The lease above authorizes 320x180, so only a rendition inside that ceiling is offered.
   mocks.signals?.catalog.set({ video: { renditions: { 'video-low': { codec: 'vp8', container: { kind: 'legacy' },
     codedWidth: 320, codedHeight: 180, framerate: 30, bitrate: 800000 },
     'video-high': { codec: 'vp8', container: { kind: 'legacy' },
       codedWidth: 640, codedHeight: 360, framerate: 30, bitrate: 4000000 } } } });
   await vi.advanceTimersByTimeAsync(1000);
   expect(stats.mock.calls.length).toBeGreaterThanOrEqual(2);
   expect(props?.target?.peek()).toEqual({ name: 'video-low', bitrate: 800000 });
   // Encoded arrivals are not decoder output. Observe only this player's frame signal.
   mocks.signals?.stats.set({ bytesReceived: 100000, frameCount: 500 });
   await vi.advanceTimersByTimeAsync(1000);
   expect(observed.mock.calls.at(-1)?.[0].presentedFrames).toBe(0);
   mocks.signals?.frame.set({ timestamp: 1000, displayWidth: 320, displayHeight: 180 });
   await vi.advanceTimersByTimeAsync(0);
   mocks.signals?.frame.set(undefined);
   await vi.advanceTimersByTimeAsync(0);
   mocks.signals?.frame.set({ timestamp: 1000, displayWidth: 320, displayHeight: 180 });
   await vi.advanceTimersByTimeAsync(1000);
   expect(observed.mock.calls.at(-1)?.[0].presentedFrames).toBe(1);
   mocks.signals?.frame.set({ timestamp: 2000, displayWidth: 320, displayHeight: 180 });
   await vi.advanceTimersByTimeAsync(1000);
   expect(observed.mock.calls.at(-1)?.[0].presentedFrames).toBe(2);
   expect(observed.mock.calls.at(-1)?.[0].ladder.renditions.map((r) => r.name)).toEqual(['video-low', 'video-high']);
   // Abort and let the release path finish, then confirm the loop really stopped.
   controller.abort();
   await vi.advanceTimersByTimeAsync(0);
   await playback;
   const sampled = stats.mock.calls.length;
   await vi.advanceTimersByTimeAsync(5000);
   expect(stats.mock.calls.length).toBe(sampled);
   expect(mocks.playerClose).toHaveBeenCalled();
   expect(api.release).toHaveBeenCalledOnce();
  } finally { observed.mockRestore(); vi.useRealTimers(); }
 });

 it('refreshes only after the prior release ACK and stops after four fresh leases', async () => {
  vi.useFakeTimers();
  const clock = vi.spyOn(performance, 'now').mockImplementation(() => Date.now());
  try {
   const { options, api } = setup();
   api.acquire.mockImplementation(() => Promise.resolve({ ...lease, expires_at: new Date(Date.now() + 60000).toISOString() }));
   let acknowledge!: () => void;
   const ack = new Promise<void>((resolve) => { acknowledge = resolve; });
   api.release.mockImplementationOnce(() => ack);
   mocks.connect.mockImplementation(() => {
    let end!: () => void;
    const closed = new Promise<null>((resolve) => { end = () => { resolve(null); }; });
    return Promise.resolve({ closed, close: () => { end(); }, transport: 'webtransport',
      probe: new Signal({}), stats: () => Promise.resolve({ bytesReceived: 0 }) });
   });
   const playback = startMoqPlaybackWithRefresh(options);
   const failed = expect(playback).rejects.toThrow('presentation watchdog');
   await vi.advanceTimersByTimeAsync(12000);
   expect(api.release).toHaveBeenCalledTimes(1);
   await vi.advanceTimersByTimeAsync(10000);
   expect(api.acquire).toHaveBeenCalledTimes(1);
   acknowledge();
   await vi.advanceTimersByTimeAsync(1000);
   expect(api.acquire).toHaveBeenCalledTimes(2);
   await vi.advanceTimersByTimeAsync(100000);
   await failed;
   expect(api.acquire).toHaveBeenCalledTimes(5);
   expect(api.release).toHaveBeenCalledTimes(5);
  } finally { clock.mockRestore(); vi.useRealTimers(); }
 });

 it('renews expired leases independently of compatibility refresh attempts', async () => {
  vi.useFakeTimers();
  const clock = vi.spyOn(performance, 'now').mockImplementation(() => Date.now());
  try {
   const { options, api, controller } = setup();
   api.acquire.mockImplementation(() => Promise.resolve({ ...lease, expires_in_ms: 2000, expires_at: new Date(Date.now() + 2000).toISOString() }));
   mocks.connect.mockImplementation(() => {
    let end!: () => void;
    const closed = new Promise<null>((resolve) => { end = () => { resolve(null); }; });
    return Promise.resolve({ closed, close: () => { end(); }, transport: 'webtransport',
      probe: new Signal({}), stats: () => Promise.resolve({ bytesReceived: 0 }) });
   });
   const playback = startMoqPlaybackWithRefresh(options);
   await vi.advanceTimersByTimeAsync(11000);
   expect(api.acquire).toHaveBeenCalledTimes(7);
   expect(api.release).toHaveBeenCalledTimes(6);
   controller.abort();
   await vi.advanceTimersByTimeAsync(0);
   await playback;
  } finally { clock.mockRestore(); vi.useRealTimers(); }
 });

 it('does not let repeated short TTLs hide presentation failure indefinitely', async () => {
  vi.useFakeTimers();
  const clock = vi.spyOn(performance, 'now').mockImplementation(() => Date.now());
  try {
   const { options, api } = setup();
   api.acquire.mockImplementation(() => Promise.resolve({ ...lease, expires_in_ms: 2000 }));
   mocks.connect.mockImplementation(() => {
    let end!: () => void;
    const closed = new Promise<null>((resolve) => { end = () => { resolve(null); }; });
    return Promise.resolve({ closed, close: () => { end(); }, transport: 'webtransport',
      probe: new Signal({}), stats: () => Promise.resolve({ bytesReceived: 0 }) });
   });
   const failed = expect(startMoqPlaybackWithRefresh(options)).rejects.toThrow('presentation watchdog');
   await vi.advanceTimersByTimeAsync(50000);
   await failed;
   // Five preexpiry renewals, then a bounded initial failure + four recovery attempts.
   expect(api.acquire).toHaveBeenCalledTimes(10);
   expect(api.release).toHaveBeenCalledTimes(10);
  } finally { clock.mockRestore(); vi.useRealTimers(); }
 });

 it('does not extend the authorization deadline by the connection duration', async () => {
  vi.useFakeTimers();
  const clock = vi.spyOn(performance, 'now').mockImplementation(() => Date.now());
  try {
   const { options, api } = setup();
   api.acquire.mockResolvedValue({ ...lease, expires_in_ms: 1000 });
   mocks.connect.mockImplementation(() => new Promise((resolve) => { setTimeout(() => {
    resolve({ closed: Promise.resolve(null), close: mocks.close, transport: 'webtransport' });
   }, 1500); }));
   const failed = expect(startMoqPlayback(options)).rejects.toThrow('MoQ lease expired during connection');
   await vi.advanceTimersByTimeAsync(1500);
   await failed;
   expect(mocks.playerProps).not.toHaveBeenCalled();
   expect(api.release).toHaveBeenCalledTimes(1);
  } finally { clock.mockRestore(); vi.useRealTimers(); }
 });

 it('rejects a zero remaining server TTL and releases before connecting', async () => {
  const { options, api } = setup();
  api.acquire.mockResolvedValue({ ...lease, expires_in_ms: 0 });
  await expect(startMoqPlaybackWithRefresh(options)).rejects.toThrow('Invalid MoQ lease expiry');
  expect(mocks.connect).not.toHaveBeenCalled();
  expect(api.acquire).toHaveBeenCalledTimes(1);
  expect(api.release).toHaveBeenCalledTimes(1);
 });

 it('does not refresh when watchdog release is not acknowledged', async () => {
  vi.useFakeTimers();
  const clock = vi.spyOn(performance, 'now').mockImplementation(() => Date.now());
  try {
   const { options, api } = setup();
   api.release.mockRejectedValue(new Error('release not acknowledged'));
   let end!: () => void;
   const closed = new Promise<null>((resolve) => { end = () => { resolve(null); }; });
   mocks.connect.mockResolvedValue({ closed, close: () => { end(); }, transport: 'webtransport',
     probe: new Signal({}), stats: () => Promise.resolve({ bytesReceived: 0 }) });
   const failed = expect(startMoqPlaybackWithRefresh(options)).rejects.toThrow('release not acknowledged');
   await vi.advanceTimersByTimeAsync(12000);
   await failed;
   expect(api.acquire).toHaveBeenCalledTimes(1);
   expect(api.release).toHaveBeenCalledTimes(1);
  } finally { clock.mockRestore(); vi.useRealTimers(); }
 });

 it('closes the session before acknowledging release on abort', async () => {
  const {options,api,controller} = setup();
  let ended!: (error: null) => void;
  const closed = new Promise<null>((resolve) => {ended=resolve;});
  mocks.close.mockImplementation(() => { ended(null); });
  mocks.connect.mockResolvedValue({closed, close:mocks.close, transport:'webtransport'});
  options.onActive.mockImplementation(() => { controller.abort(); });
  await startMoqPlayback(options);
  expect(mocks.close).toHaveBeenCalledOnce(); expect(api.release).toHaveBeenCalledOnce(); expect(mocks.playerClose).toHaveBeenCalledOnce();
 });
});
