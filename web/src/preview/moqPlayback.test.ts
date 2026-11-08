import type * as ConnectionModule from './moqConnection';
import { afterEach, describe, expect, it, vi } from 'vitest';
const mocks = vi.hoisted(() => ({ supported: vi.fn(), connect: vi.fn(), close: vi.fn(), playerClose: vi.fn() }));
vi.mock('@moq/net', () => ({ Connection: { isWebTransportSupported: mocks.supported }, Origin: { Producer: class { close = vi.fn(); } }, Path: { from: (s: string) => s } }));
vi.mock('@moq/watch', () => ({ Player: class { close = mocks.playerClose; } }));
vi.mock('./moqConnection', async (original) => ({ ...await original<typeof ConnectionModule>(), connectMoq: mocks.connect }));
import { MoqUnavailable, startMoqPlayback, type MoqLease } from './moqPlayback';
const lease: MoqLease = { lease_id: 'a', generation: 1, expires_at: new Date(Date.now()+60000).toISOString(), url: 'https://relay.example', broadcast:'program', protocol:'moq-transport-18', video:{codec:'vp8', coded_width:320, coded_height:180}, audio:[] };
afterEach(() => { vi.unstubAllGlobals(); vi.resetAllMocks(); });
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
