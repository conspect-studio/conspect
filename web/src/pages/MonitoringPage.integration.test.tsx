// Real MonitoringPage → PreviewSurface → WhepPlayer lifecycle integration.
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { act, cleanup, fireEvent, screen, waitFor } from '@testing-library/react';
import { MemoryRouter } from 'react-router-dom';
import { renderWithProviders } from '../test/render';
import { MonitoringPage } from './MonitoringPage';

class FakePeer extends EventTarget {
  iceGatheringState = 'complete';
  localDescription = { type: 'offer', sdp: 'v=0' };
  ontrack = null;
  onconnectionstatechange: (() => void) | null = null;
  connectionState = 'new';
  addTransceiver(): void { /* The offer carries receive-only tracks. */ }
  createOffer(): Promise<RTCSessionDescriptionInit> { return Promise.resolve({ type: 'offer', sdp: 'v=0' }); }
  setLocalDescription(): Promise<void> { return Promise.resolve(); }
  setRemoteDescription(): Promise<void> { return Promise.resolve(); }
  getStats(): Promise<RTCStatsReport> { return Promise.resolve(new Map() as RTCStatsReport); }
  close(): void { /* closed */ }
  fail(): void { this.connectionState = 'failed'; this.onconnectionstatechange?.(); }
}
const peers: FakePeer[] = [];
vi.mock('../preview/pcFactory', () => ({ defaultPcFactory: () => { const peer = new FakePeer(); peers.push(peer); return peer; } }));
vi.mock('../realtime/useEngineEvents', () => ({ useEngineEvents: () => ({ status: 'open' }) }));
vi.mock('../resources/useLiveTiles', () => ({ useLiveTiles: () => new Map() }));
const posts: string[] = [];
const deletes: (() => void)[] = [];
let programReject = false;
let inputReject = false;
let inputIds = ['a', 'b'];
let hls = false;
let programAvailable = true;
let pendingPost: ((response: Response) => void) | undefined;
const response = (status: number, location?: string): Response => {
  const result = new Response(status === 204 ? null : status === 503 ? JSON.stringify({ fallback: 'jpeg' }) : 'v=0', {
    status, headers: location ? { Location: location } : {},
  });
  Object.defineProperty(result, 'url', { value: 'https://example.test/api/v1/preview/program/whep' });
  return result;
};
const fetchMock = vi.fn<typeof fetch>(async (path, options) => {
  const url = path instanceof Request ? path.url : String(path);
  if (options?.method === 'POST') {
    posts.push(url);
    const busy = url.includes('/inputs/') ? inputReject : programReject;
    if (url.includes('/inputs/a/') && pendingPost !== undefined) return new Promise<Response>((resolve) => { pendingPost = resolve; });
    return busy ? response(503) : response(201, `/lease/${String(posts.length)}`);
  }
  if (options?.method === 'DELETE') return new Promise<Response>((resolve) => { deletes.push(() => { resolve(response(204)); }); });
  if (url.includes('/preview/inputs') && !url.endsWith('.jpg')) return Response.json(inputIds);
  if (url.includes('/preview/capabilities')) return Response.json({ webrtc: true, scopes: { program: { whep: programAvailable }, inputs: { whep: true } } });
  if (url.includes('/api/v1/outputs')) return Response.json(hls ? [{ id: 'hls', name: 'HLS', body: { kind: 'hls', path: '/out.m3u8' } }] : []);
  return response(404);
});
const program = '/api/v1/preview/program/whep';
const input = (id: string): string => `/api/v1/preview/inputs/${id}/whep`;
async function start(): Promise<void> {
  renderWithProviders(<MemoryRouter><MonitoringPage /></MemoryRouter>);
  await screen.findByRole('button', { name: 'Open preview of input a' });
  await waitFor(() => { expect(posts).toContain(program); });
}
async function open(id: string): Promise<void> {
  await act(async () => { fireEvent.click(screen.getByRole('button', { name: `Open preview of input ${id}`, hidden: true })); await Promise.resolve(); });
}
async function finishDelete(): Promise<void> {
  const done = deletes.shift();
  await act(async () => { done?.(); await Promise.resolve(); });
}
beforeEach(() => {
  posts.length = 0; deletes.length = 0; peers.length = 0;
  programReject = false; inputReject = false; inputIds = ['a', 'b']; hls = false; programAvailable = true; pendingPost = undefined;
  vi.stubGlobal('fetch', fetchMock);
  vi.stubGlobal('MediaStream', class { addTrack(): void { /* no track */ } });
  fetchMock.mockClear();
});
afterEach(async () => {
  // Unmount while the fetch stub is still installed. A player releases its
  // session on unmount; that DELETE must be answered here. If the stub is
  // removed first, the DELETE goes to the real network, stays pending into the
  // next test (whose player waits for earlier releases), and its late failure
  // calls into React after this file's environment is gone.
  cleanup();
  for (let quiet = 0; quiet < 3;) {
    const pending = deletes.splice(0);
    for (const done of pending) done();
    quiet = pending.length === 0 ? quiet + 1 : 0;
    await new Promise<void>((resolve) => { setTimeout(resolve, 0); });
  }
  vi.unstubAllGlobals();
});
describe('real single-focus lease handoff', () => {
  it('program 503 fallback lets source open without waiting for a nonexistent lease', async () => {
    programReject = true;
    await start();
    await screen.findByTestId('fallback-badge');
    await open('a');
    await waitFor(() => { expect(posts).toContain(input('a')); });
    expect(posts.filter((p) => p === program)).toHaveLength(1);
  });
  it('source fatal closes and restores program without a duplicate DELETE', async () => {
    await start(); await open('a');
    await waitFor(() => { expect(deletes).toHaveLength(1); });
    await finishDelete();
    await waitFor(() => { expect(posts).toContain(input('a')); });
    await waitFor(() => { expect(peers.at(-1)?.onconnectionstatechange).not.toBeNull(); });
    act(() => { peers.at(-1)?.fail(); });
    await screen.findByTestId('input-preview-unavailable');
    fireEvent.click(screen.getByRole('button', { name: 'Close' }));
    await waitFor(() => { expect(deletes).toHaveLength(1); });
    expect(posts.filter((p) => p === program)).toHaveLength(1);
    await finishDelete();
    await waitFor(() => { expect(posts.filter((p) => p === program)).toHaveLength(2); });
    expect(deletes).toHaveLength(0);
  });
  it('waits for both DELETE responses when switching input and program', async () => {
    await start(); await open('a');
    await waitFor(() => { expect(deletes).toHaveLength(1); });
    expect(posts).not.toContain(input('a'));
    await finishDelete();
    await waitFor(() => { expect(posts).toContain(input('a')); });
    await open('b');
    await waitFor(() => { expect(deletes).toHaveLength(1); });
    expect(posts).not.toContain(input('b'));
    await finishDelete();
    await waitFor(() => { expect(posts).toContain(input('b')); });
  });
  it('cancels rapid open and close while DELETE is delayed', async () => {
    await start(); await open('a');
    await waitFor(() => { expect(deletes).toHaveLength(1); });
    fireEvent.click(screen.getByRole('button', { name: 'Close' }));
    await finishDelete();
    await waitFor(() => { expect(posts.filter((p) => p === program)).toHaveLength(2); });
    expect(posts).not.toContain(input('a'));
  });
  it('waits for late input POST and its DELETE before program resumes', async () => {
    await start();
    pendingPost = () => { /* Arm the delayed POST. */ };
    await open('a');
    await waitFor(() => { expect(deletes).toHaveLength(1); });
    await finishDelete();
    await waitFor(() => { expect(posts).toContain(input('a')); });
    fireEvent.click(screen.getByRole('button', { name: 'Close' }));
    expect(posts.filter((p) => p === program)).toHaveLength(1);
    act(() => { pendingPost?.(response(201, '/lease/late')); });
    await waitFor(() => { expect(deletes).toHaveLength(1); });
    expect(posts.filter((p) => p === program)).toHaveLength(1);
    await finishDelete();
    await waitFor(() => { expect(posts.filter((p) => p === program)).toHaveLength(2); });
  });
  it('opens input without an existing program lease when HLS is selected', async () => {
    hls = true;
    await start();
    await screen.findByRole('combobox');
    fireEvent.change(screen.getByRole('combobox'), { target: { value: 'hls' } });
    await waitFor(() => { expect(deletes).toHaveLength(1); });
    await finishDelete();
    await open('a');
    await waitFor(() => { expect(posts).toContain(input('a')); });
  });
  it('opens an input when program WHEP was never mounted', async () => {
    programAvailable = false;
    renderWithProviders(<MemoryRouter><MonitoringPage /></MemoryRouter>);
    await screen.findByRole('button', { name: 'Open preview of input a' });
    expect(posts).not.toContain(program);
    await open('a');
    await waitFor(() => { expect(posts).toContain(input('a')); });
  });
  it('keeps last good input ids when a refresh fails', async () => {
    await start();
    inputIds = [];
    fetchMock.mockImplementationOnce(() => Promise.resolve(response(503)));
    await open('a');
    expect(screen.getByRole('button', { name: 'Open preview of input b', hidden: true })).toBeInTheDocument();
  });
});
