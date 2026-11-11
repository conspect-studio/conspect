// Graphics PlayoutView — TAKE / CLEAR / NEXT buttons and the F1 / F2 / Space
// shortcuts call the playout API; clicking a rundown row cues it; Alt+↓
// reorders the rundown.
import { afterAll, afterEach, beforeAll, describe, expect, it, vi } from 'vitest';
import { fireEvent, screen, waitFor } from '@testing-library/react';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';
import { MemoryRouter } from 'react-router-dom';

import { PlayoutView } from './PlayoutView';
import { renderWithProviders } from '../../test/render';

vi.mock('../../preview/PreviewSurface', () => ({
  PreviewSurface: ({ label }: { label: string }) => <div data-testid={`monitor-${label}`} />,
}));

const CHANNEL = {
  id: 'gfx-channel-1',
  name: 'Channel 1',
  rundown: ['syd7', 'sydnow'],
  transition: { kind: 'mix', frames: 12 },
  preview: 'syd7',
  program: null,
  seq: 0,
  program_source: 'gfx-channel-1',
  preview_source: 'gfx-channel-1-preview',
};
const ITEMS = [
  { id: 'syd7', name: 'Sydney 7-day', template: 'weather.seven_day', data_source: 'sydney', style: { background: 'gradient' } },
  { id: 'sydnow', name: 'Sydney now', template: 'weather.today', data_source: 'sydney', style: { background: 'transparent' } },
];

const calls: { verb: string; body: unknown }[] = [];

const server = setupServer(
  http.get('*/api/v1/preview/capabilities', () => HttpResponse.json({}, { status: 404 })),
  http.get('*/api/v1/graphics/channels', () => HttpResponse.json([CHANNEL])),
  http.get('*/api/v1/graphics/items', () => HttpResponse.json(ITEMS)),
  http.get('*/api/v1/sources', () =>
    HttpResponse.json([
      { id: 'gfx-channel-1', name: 'PGM', body: {} },
      { id: 'gfx-channel-1-preview', name: 'PVW', body: {} },
    ]),
  ),
  http.post('*/api/v1/graphics/channels/:id/:verb', async ({ params, request }) => {
    const body: unknown = await request.json();
    calls.push({ verb: String(params.verb), body });
    const program = params.verb === 'take' ? 'syd7' : null;
    return HttpResponse.json({ ...CHANNEL, program, preview: 'sydnow' });
  }),
  http.put('*/api/v1/graphics/channels/:id/rundown', async ({ request }) => {
    const body: unknown = await request.json();
    calls.push({ verb: 'rundown', body });
    return HttpResponse.json(CHANNEL);
  }),
);

beforeAll(() => {
  server.listen();
});
afterEach(() => {
  server.resetHandlers();
  calls.length = 0;
});
afterAll(() => {
  server.close();
});

async function renderView(): Promise<void> {
  renderWithProviders(
    <MemoryRouter>
      <PlayoutView />
    </MemoryRouter>,
  );
  await screen.findByTestId('gfx-pvw-item');
  await screen.findAllByText('Sydney 7-day');
}

describe('Graphics PlayoutView', () => {
  it('shows PVW/PGM monitors and documents the shortcuts', async () => {
    await renderView();
    expect(screen.getByTestId('monitor-PVW')).toBeInTheDocument();
    expect(screen.getByTestId('monitor-PGM')).toBeInTheDocument();
    expect(screen.getByTestId('gfx-pvw-item')).toHaveTextContent('Sydney 7-day');
    expect(screen.getByText(/F1 = TAKE/)).toBeInTheDocument();
  });

  it('TAKE and CLEAR buttons call the playout API', async () => {
    await renderView();
    fireEvent.click(screen.getByRole('button', { name: /TAKE/ }));
    await waitFor(() => {
      expect(calls.map((c) => c.verb)).toEqual(['take']);
    });
    await waitFor(() => {
      expect(screen.getByTestId('gfx-pgm-item')).toHaveTextContent('Sydney 7-day');
    });
    fireEvent.click(screen.getByRole('button', { name: /CLEAR/ }));
    await waitFor(() => {
      expect(calls.map((c) => c.verb)).toEqual(['take', 'clear']);
    });
  });

  it('F1 takes, F2 clears, Space cues next', async () => {
    await renderView();
    fireEvent.keyDown(window, { key: 'F1' });
    await waitFor(() => {
      expect(calls.map((c) => c.verb)).toContain('take');
    });
    fireEvent.keyDown(window, { key: 'F2' });
    await waitFor(() => {
      expect(calls.map((c) => c.verb)).toContain('clear');
    });
    fireEvent.keyDown(window, { key: ' ', code: 'Space' });
    await waitFor(() => {
      expect(calls.map((c) => c.verb)).toContain('next');
    });
  });

  it('shortcuts are ignored while typing in a field', async () => {
    await renderView();
    const input = screen.getByLabelText('Duration (frames)');
    fireEvent.keyDown(input, { key: 'F1' });
    await new Promise((r) => setTimeout(r, 20));
    expect(calls).toHaveLength(0);
  });

  it('clicking a rundown row cues it; Alt+ArrowDown reorders', async () => {
    await renderView();
    const row = screen.getByText('Sydney now').closest('li') as HTMLElement;
    fireEvent.click(row);
    await waitFor(() => {
      expect(calls[0]).toEqual({ verb: 'cue', body: { item: 'sydnow' } });
    });
    const first = screen.getAllByText('Sydney 7-day').find((el) => el.closest('li') !== null)?.closest('li') as HTMLElement;
    fireEvent.keyDown(first, { key: 'ArrowDown', altKey: true });
    await waitFor(() => {
      expect(calls.find((c) => c.verb === 'rundown')?.body).toEqual({ rundown: ['sydnow', 'syd7'] });
    });
  });
});
