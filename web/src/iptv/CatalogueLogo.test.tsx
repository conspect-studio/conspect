import { render, waitFor } from '@testing-library/react';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest';
import { CatalogueLogo } from './CatalogueLogo';

const seen: (string | null)[] = [];
const server = setupServer(
  http.get('*/api/v1/catalogues/iptv-org/logos/:k', ({ request, params }) => {
    seen.push(request.headers.get('authorization'));
    return params.k === 'l:missing'
      ? new HttpResponse(null, { status: 404 })
      : new HttpResponse(new Uint8Array([137, 80, 78, 71]), { headers: { 'content-type': 'image/png' } });
  }),
);
beforeAll(() => { server.listen(); });
afterEach(() => { server.resetHandlers(); seen.length = 0; window.localStorage.clear(); });
afterAll(() => { server.close(); });

describe('CatalogueLogo', () => {
  it('fetches the logo with the bearer token and shows it', async () => {
    window.localStorage.setItem('multiview.apiToken', 'admin.secret');
    const { container } = render(<CatalogueLogo logoKey="l:abc" />);
    await waitFor(() => { expect(container.querySelector('img')).not.toBeNull(); });
    expect(seen).toEqual(['Bearer admin.secret']);
    expect(container.querySelector('img')?.getAttribute('src')).toMatch(/^blob:/);
  });

  it('renders nothing when the logo is missing', async () => {
    const { container } = render(<CatalogueLogo logoKey="l:missing" />);
    await waitFor(() => { expect(seen).toHaveLength(1); });
    expect(container.querySelector('img')).toBeNull();
  });
});
