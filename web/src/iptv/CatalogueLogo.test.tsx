import { render, waitFor } from '@testing-library/react';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest';
import { fireEvent } from '@testing-library/react';
import { CatalogueLogo } from './CatalogueLogo';

const seen: (string | null)[] = [];
const server = setupServer(
  http.get('*/api/v1/catalogues/iptv-org/logos/:k', ({ request, params }) => {
    seen.push(request.headers.get('authorization'));
    if (params.k === 'l:missing') {
      return new HttpResponse(null, { status: 404 });
    }
    if (params.k === 'l:none') {
      return new HttpResponse(null, { status: 204 });
    }
    if (params.k === 'l:err') {
      return HttpResponse.error();
    }
    return new HttpResponse(new Uint8Array([137, 80, 78, 71]), { headers: { 'content-type': 'image/png' } });
  }),
);
beforeAll(() => { server.listen(); });
afterEach(() => { server.resetHandlers(); seen.length = 0; window.localStorage.clear(); });
afterAll(() => { server.close(); });

describe('CatalogueLogo', () => {
  it('fetches the logo with the bearer token and shows it', async () => {
    window.localStorage.setItem('multiview.apiToken', 'admin.secret');
    const { container } = render(<CatalogueLogo logoKey="l:abc" name="BBC News" />);
    await waitFor(() => { expect(container.querySelector('img')).not.toBeNull(); });
    expect(seen).toEqual(['Bearer admin.secret']);
    expect(container.querySelector('img')?.getAttribute('src')).toMatch(/^blob:/);
  });

  it('renders an initials placeholder when the logo is missing (404)', async () => {
    const { container, findByTestId } = render(<CatalogueLogo logoKey="l:missing" name="BBC News" />);
    const box = await findByTestId('logo-placeholder');
    expect(box.textContent).toBe('BN');
    expect(box.getAttribute('aria-hidden')).toBe('true');
    expect(container.querySelector('img')).toBeNull();
  });

  it('treats 204 No Content as no logo', async () => {
    const { container, findByTestId } = render(<CatalogueLogo logoKey="l:none" name="Euronews" />);
    expect((await findByTestId('logo-placeholder')).textContent).toBe('E');
    expect(container.querySelector('img')).toBeNull();
  });

  it('treats a network error as no logo', async () => {
    const { findByTestId } = render(<CatalogueLogo logoKey="l:err" name="ZDF" />);
    expect((await findByTestId('logo-placeholder')).textContent).toBe('Z');
  });

  it('falls back to the placeholder when the <img> fails to decode', async () => {
    const { container, findByTestId } = render(<CatalogueLogo logoKey="l:abc" name="Arte TV" />);
    await waitFor(() => { expect(container.querySelector('img')).not.toBeNull(); });
    fireEvent.error(container.querySelector('img')!);
    expect((await findByTestId('logo-placeholder')).textContent).toBe('AT');
    expect(container.querySelector('img')).toBeNull();
  });
});
