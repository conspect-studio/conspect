// SyncGroupsPage — CRUD over /api/v1/sync-groups with per-member offset_ms,
// target_skew_ms bounds, the HONEST weakest-member tier claim (managed-
// devices.md §8 — never over-claimed), cast devices never offered as members
// (Tier D), and the 202 measure verb.
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest';
import { screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';
import { MemoryRouter } from 'react-router-dom';

import { SyncGroupsPage } from './SyncGroupsPage';
import { renderWithProviders } from '../test/render';

const DEVICES = [
  { id: 'dev-a', name: 'Foyer box', body: { id: 'dev-a', driver: 'zowietek', address: 'http://[fd00::1]' } },
  { id: 'dev-b', name: 'Lobby node', body: { id: 'dev-b', driver: 'displaynode' } },
  { id: 'dev-tv', name: 'Break-room TV', body: { id: 'dev-tv', driver: 'cast', address: 'http://[fd00::7]' } },
];

const GROUPS = [
  {
    id: 'wall',
    name: 'Lobby wall',
    body: {
      id: 'wall',
      target_skew_ms: 80,
      members: [
        { device: 'dev-a', offset_ms: 120 },
        { device: 'dev-b', offset_ms: 0 },
      ],
    },
  },
];

const server = setupServer(
  http.get('*/api/v1/sync-groups', () => HttpResponse.json(GROUPS)),
  http.get('*/api/v1/devices', () => HttpResponse.json(DEVICES)),
  http.get('*/api/v1/devices/:id/status', () =>
    HttpResponse.json({ title: 'not found', status: 404 }, { status: 404 }),
  ),
);

beforeAll(() => {
  server.listen();
});
afterEach(() => {
  server.resetHandlers();
});
afterAll(() => {
  server.close();
});

function renderGroups(): void {
  renderWithProviders(
    <MemoryRouter>
      <SyncGroupsPage />
    </MemoryRouter>,
  );
}

describe('SyncGroupsPage', () => {
  it('lists groups with target skew and the honest weakest-member tier', async () => {
    renderGroups();
    expect(await screen.findByText('Lobby wall')).toBeInTheDocument();
    expect(screen.getByText(/80 ms/)).toBeInTheDocument();
    // dev-a is a zowietek (Tier C, bounded drift): the group can only claim
    // bounded skew even though dev-b is frame-accurate.
    expect(screen.getByText(/bounded skew/i)).toBeInTheDocument();
  });

  it('names the unique weakest member limiting the tier (§8 honesty rule)', async () => {
    renderGroups();
    expect(await screen.findByText('Lobby wall')).toBeInTheDocument();
    // dev-a (zowietek, bounded skew) is the sole member dragging the group
    // below dev-b's frame-accurate tier: say so, per the brief's sketch.
    expect(screen.getByText('limited by dev-a')).toBeInTheDocument();
  });

  it('omits the limiter when the weakest tier is shared by several members', async () => {
    server.use(
      http.get('*/api/v1/devices', () =>
        HttpResponse.json([
          ...DEVICES,
          {
            id: 'dev-c',
            name: 'Stage box',
            body: { id: 'dev-c', driver: 'zowietek', address: 'http://[fd00::2]' },
          },
        ]),
      ),
      http.get('*/api/v1/sync-groups', () =>
        HttpResponse.json([
          {
            id: 'pair',
            name: 'Pair wall',
            body: {
              id: 'pair',
              target_skew_ms: 80,
              members: [
                { device: 'dev-a', offset_ms: 0 },
                { device: 'dev-c', offset_ms: 0 },
              ],
            },
          },
        ]),
      ),
    );
    renderGroups();
    expect(await screen.findByText('Pair wall')).toBeInTheDocument();
    expect(screen.getByText(/bounded skew/i)).toBeInTheDocument();
    // No single member limits the claim: naming one would be dishonest.
    expect(screen.queryByText(/limited by/i)).not.toBeInTheDocument();
  });

  it('measure rides the 202 operation path', async () => {
    let measured = 0;
    server.use(
      http.post('*/api/v1/sync-groups/wall/measure', () => {
        measured += 1;
        return HttpResponse.json(
          { operation_id: 'op-m', kind: 'sync-measure' },
          { status: 202 },
        );
      }),
    );
    renderGroups();
    expect(await screen.findByText('Lobby wall')).toBeInTheDocument();
    await userEvent.click(
      screen.getByRole('button', { name: /measure skew: lobby wall/i }),
    );
    expect(measured).toBe(1);
  });

  it('never offers cast devices as members (Tier D)', async () => {
    renderGroups();
    expect(await screen.findByText('Lobby wall')).toBeInTheDocument();
    await userEvent.click(screen.getByRole('button', { name: 'New sync group' }));
    const dialog = await screen.findByRole('dialog');
    await userEvent.click(within(dialog).getByRole('button', { name: /add member/i }));
    // The member picker offers the adoptable sync-capable devices only.
    expect(within(dialog).queryByText('dev-tv')).not.toBeInTheDocument();
    expect(within(dialog).getByText('dev-a')).toBeInTheDocument();
    // The exclusion is explained, not silent.
    expect(within(dialog).getByText(/cast devices/i)).toBeInTheDocument();
  });

  it('creating posts the exact config SyncGroup body', async () => {
    let posted: unknown;
    server.use(
      http.post('*/api/v1/sync-groups/:id', async ({ request, params }) => {
        posted = { id: params.id, payload: await request.json() };
        return HttpResponse.json(
          { id: String(params.id), name: 'New wall', body: { id: String(params.id), target_skew_ms: 50, members: [] } },
          { status: 201, headers: { ETag: '"1"' } },
        );
      }),
    );
    renderGroups();
    expect(await screen.findByText('Lobby wall')).toBeInTheDocument();
    await userEvent.click(screen.getByRole('button', { name: 'New sync group' }));
    const dialog = await screen.findByRole('dialog');
    await userEvent.type(within(dialog).getByLabelText('Identifier'), 'new-wall');
    await userEvent.type(within(dialog).getByLabelText('Name'), 'New wall');
    const skew = within(dialog).getByLabelText(/target skew/i);
    await userEvent.clear(skew);
    await userEvent.type(skew, '50');
    await userEvent.click(within(dialog).getByRole('button', { name: /add member/i }));
    const offset = within(dialog).getByLabelText(/offset/i);
    await userEvent.clear(offset);
    await userEvent.type(offset, '25');
    await userEvent.click(within(dialog).getByRole('button', { name: 'Create' }));
    expect(posted).toEqual({
      id: 'new-wall',
      payload: {
        name: 'New wall',
        body: {
          id: 'new-wall',
          target_skew_ms: 50,
          members: [{ device: 'dev-a', offset_ms: 25 }],
        },
      },
    });
  });

  it('requires at least one member before saving', async () => {
    renderGroups();
    expect(await screen.findByText('Lobby wall')).toBeInTheDocument();
    await userEvent.click(screen.getByRole('button', { name: 'New sync group' }));
    const dialog = await screen.findByRole('dialog');
    await userEvent.type(within(dialog).getByLabelText('Identifier'), 'empty');
    await userEvent.type(within(dialog).getByLabelText('Name'), 'Empty');
    await userEvent.click(within(dialog).getByRole('button', { name: 'Create' }));
    expect(
      await within(dialog).findByText(/at least one member/i),
    ).toBeInTheDocument();
  });
});
