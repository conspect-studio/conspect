import { afterEach, describe, expect, it, vi } from 'vitest';
import { waitFor } from '@testing-library/react';
import { MemoryRouter } from 'react-router-dom';

import { renderWithProviders } from '../../test/render';
import { ProductionDesk } from './ProductionDesk';

afterEach(() => { vi.unstubAllGlobals(); vi.restoreAllMocks(); });

describe('ProductionDesk honesty', () => {
  it('shows the typed unavailable state, never an <img>, when the still route answers the SPA index', async () => {
    vi.stubGlobal('fetch', vi.fn((url: string) => Promise.resolve(
      url.includes('/preview/switcher/')
        ? new Response('<!doctype html>', { status: 200, headers: { 'content-type': 'text/html' } })
        : new Response('{}', { status: 404 }),
    )));
    const { findByTestId, queryByAltText, getByTestId } = renderWithProviders(<MemoryRouter><ProductionDesk /></MemoryRouter>);
    await waitFor(async () => { expect(await findByTestId('wall-still-note')).toBeTruthy(); });
    expect(getByTestId('wall-still-note')).toHaveTextContent('~2 s');
    expect(await findByTestId('preview-unavailable')).toHaveTextContent('Preview unavailable — switcher backend not deployed');
    expect(queryByAltText('Preview monitor')).toBeNull();
  });
});
