import { afterAll, afterEach, beforeAll, describe, expect, it, vi } from 'vitest';
import { act, fireEvent, screen } from '@testing-library/react';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';
import { MemoryRouter } from 'react-router-dom';
import { useEffect, useRef } from 'react';

import { renderWithProviders } from '../test/render';
import { MonitoringPage } from './MonitoringPage';

const release = vi.hoisted(() => new Map<string, () => void>());
const sessions = vi.hoisted(() => new Array<string>());
vi.mock('../preview/PreviewSurface', () => ({
  PreviewSurface: ({ whepEndpoint, onSessionReleased, onLeaseState }: { whepEndpoint: string; onSessionReleased?: () => void; onLeaseState?: (state: 'negotiating' | 'active' | 'released') => void }): null => {
    const callback = useRef(onSessionReleased);
    const state = useRef(onLeaseState);
    callback.current = onSessionReleased;
    state.current = onLeaseState;
    useEffect(() => {
      sessions.push(whepEndpoint);
      state.current?.('negotiating');
      release.set(whepEndpoint, () => { state.current?.('released'); callback.current?.(); });
    }, [whepEndpoint]);
    return null;
  },
}));
vi.mock('../realtime/useEngineEvents', () => ({ useEngineEvents: () => ({ status: 'open' }) }));
vi.mock('../resources/useLiveTiles', () => ({ useLiveTiles: () => new Map() }));

let ids = ['a', 'b'];
const server = setupServer(
  http.get('*/api/v1/preview/inputs', () => HttpResponse.json(ids)),
  http.get('*/api/v1/preview/capabilities', () => HttpResponse.json({
    webrtc: true,
    scopes: {
      program: { whep: true, fidelity: 'real-encoded-output' },
      inputs: { whep: true },
      outputs: { whep: true },
    },
  })),
  http.get('*/api/v1/outputs', () => HttpResponse.json([])),
);
beforeAll(() => { server.listen(); });
afterEach(() => { server.resetHandlers(); release.clear(); sessions.length = 0; ids = ['a', 'b']; });
afterAll(() => { server.close(); });

function posts(endpoint: string): number {
  return sessions.filter((session) => session === endpoint).length;
}

async function renderPage(): Promise<void> {
  renderWithProviders(<MemoryRouter><MonitoringPage /></MemoryRouter>);
  await screen.findByRole('button', { name: 'Open preview of input a' });
  expect(posts('/api/v1/preview/program/whep')).toBeGreaterThan(0);
}

describe('MonitoringPage single WHEP focus', () => {
  it('waits for program DELETE and then input DELETE before it restores program', async () => {
    await renderPage();
    fireEvent.click(screen.getByRole('button', { name: 'Open preview of input a' }));
    expect(posts('/api/v1/preview/inputs/a/whep')).toBe(0);
    act(() => { release.get('/api/v1/preview/program/whep')?.(); });
    expect(posts('/api/v1/preview/inputs/a/whep')).toBeGreaterThan(0);
    fireEvent.click(screen.getByRole('button', { name: 'Close' }));
    expect(posts('/api/v1/preview/program/whep')).toBe(1);
    act(() => { release.get('/api/v1/preview/inputs/a/whep')?.(); });
    expect(posts('/api/v1/preview/program/whep')).toBeGreaterThan(1);
  });

  it('waits for input DELETE before it opens another input', async () => {
    await renderPage();
    fireEvent.click(screen.getByRole('button', { name: 'Open preview of input a' }));
    act(() => { release.get('/api/v1/preview/program/whep')?.(); });
    expect(posts('/api/v1/preview/inputs/a/whep')).toBe(1);
    fireEvent.click(screen.getByRole('button', { name: 'Open preview of input b', hidden: true }));
    expect(posts('/api/v1/preview/inputs/b/whep')).toBe(0);
    expect(posts('/api/v1/preview/program/whep')).toBe(1);
    act(() => { release.get('/api/v1/preview/inputs/a/whep')?.(); });
    expect(posts('/api/v1/preview/inputs/b/whep')).toBe(1);
    expect(posts('/api/v1/preview/program/whep')).toBe(1);
  });

  it('cancels a quick open/close before program DELETE without a new input POST', async () => {
    await renderPage();
    fireEvent.click(screen.getByRole('button', { name: 'Open preview of input a' }));
    fireEvent.click(screen.getByRole('button', { name: 'Close' }));
    act(() => { release.get('/api/v1/preview/program/whep')?.(); });
    expect(posts('/api/v1/preview/inputs/a/whep')).toBe(0);
    expect(posts('/api/v1/preview/program/whep')).toBeGreaterThan(1);
  });
});
