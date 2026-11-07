// Network-path panel (design §3a): reachable from any source row and from a
// probe result; IPv4 vs IPv6 side by side; copy as text / JSON.
import { afterAll, afterEach, beforeAll, describe, expect, it, vi } from 'vitest';
import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';
import { MemoryRouter } from 'react-router-dom';

import type { NetworkReport } from '../api/catalogue';
import { SourcesPage } from '../pages/SourcesPage';
import { renderWithProviders } from '../test/render';
import { ProbeSummary } from './ProbeSummary';

// Real hosts from the iptv-org rows (KSHB uplynk; PBS KMOS CDN).
const REPORT: NetworkReport = {
  egress: { ipv4: '203.0.113.7', ipv6: '2001:db8::7', country: 'US', colo: 'DFW' },
  hosts: [
    {
      host: 'content.uplynk.com',
      role: 'master',
      dns: { cname: ['content.uplynk.com.edgekey.net'], a: ['192.0.2.10'], aaaa: ['2001:db8:1::10'], ttl: 60 },
      addresses: [
        { ip: '2001:db8:1::10', family: 'ipv6', class: 'public', connect_ms: 12, status: 403, error: null },
        { ip: '192.0.2.10', family: 'ipv4', class: 'public', connect_ms: 18, status: 200, error: null },
      ],
      tls: { version: 'TLSv1.3', alpn: 'h2', subject: 'content.uplynk.com', issuer: 'DigiCert', not_after: '2027-01-01T00:00:00Z' },
    },
    {
      host: 'hls-kmosdt.lls.cdn.pbs.org',
      role: 'segment',
      dns: { cname: [], a: ['192.0.2.20'], aaaa: [], ttl: 30 },
      addresses: [{ ip: '192.0.2.20', family: 'ipv4', class: 'public', connect_ms: null, status: null, error: 'timeout' }],
      tls: null,
    },
    {
      host: 'keys.example.test',
      role: 'key',
      dns: { cname: [], a: [], aaaa: [], ttl: null, resolver: 'system', error: 'no record found for keys.example.test' },
      addresses: [],
      tls: null,
    },
  ],
  hops: [
    {
      url_redacted: 'https://content.uplynk.com/channel/50d0fa1b042945a3a4f550f9b8412c83.m3u8',
      status: 403, ms: 41, server: 'AkamaiGHost', via: null, age: null,
      cdn_headers: { 'x-akamai-request-id': 'abc' },
      body_excerpt: 'Not available in your region',
    },
  ],
  request_headers: { user_agent: 'Mozilla/5.0', referrer: null },
  assessment: ['IPv6 path returns 403, IPv4 path returns 200 → prefer IPv4 for this host.'],
  curl: ['curl -4 -sS -o /dev/null -w "%{http_code}" https://content.uplynk.com/channel/50d0fa1b042945a3a4f550f9b8412c83.m3u8'],
};

let networkPosts: string[] = [];
const server = setupServer(
  http.get('*/api/v1/sources', () =>
    HttpResponse.json([
      { id: 'kshb', name: 'KSHB 41', body: { id: 'kshb', kind: 'hls', url: 'https://content.uplynk.com/channel/x.m3u8' } },
    ]),
  ),
  http.get('*/api/v1/catalogues/iptv-org/links', () => HttpResponse.json({ links: [] })),
  http.post('*/api/v1/sources/:id/network', ({ params }) => {
    networkPosts.push(String(params.id));
    return HttpResponse.json(REPORT);
  }),
);
beforeAll(() => { server.listen(); });
afterEach(() => { server.resetHandlers(); networkPosts = []; });
afterAll(() => { server.close(); });

describe('Network panel', () => {
  it('opens from an unlinked source row, POSTs, and shows IPv4 vs IPv6 side by side', async () => {
    renderWithProviders(<MemoryRouter><SourcesPage /></MemoryRouter>);
    await userEvent.click(await screen.findByRole('button', { name: /^network: kshb 41$/i }));
    const dialog = await screen.findByRole('dialog');
    const host = await within(dialog).findByTestId('net-host-content.uplynk.com');
    expect(networkPosts).toEqual(['kshb']);
    const table = within(host).getByRole('table');
    expect(within(table).getByRole('columnheader', { name: 'IPv4' })).toBeInTheDocument();
    expect(within(table).getByRole('columnheader', { name: 'IPv6' })).toBeInTheDocument();
    const cells = within(table).getAllByRole('cell');
    expect(cells[0]).toHaveTextContent('192.0.2.10');
    expect(cells[0]).toHaveTextContent('HTTP 200');
    expect(cells[1]).toHaveTextContent('2001:db8:1::10');
    expect(cells[1]).toHaveTextContent('HTTP 403');
    expect(within(dialog).getByText(/203\.0\.113\.7/)).toBeInTheDocument();
    expect(within(dialog).getByText('DFW')).toBeInTheDocument();
    expect(within(dialog).getByText(/prefer IPv4 for this host/)).toBeInTheDocument();
    expect(within(dialog).getByText('Not available in your region')).toBeInTheDocument();
    // A host without IPv6 says so, and a failed address shows its error.
    const seg = within(dialog).getByTestId('net-host-hls-kmosdt.lls.cdn.pbs.org');
    expect(seg).toHaveTextContent('No IPv6 address');
    expect(seg).toHaveTextContent('Failed: timeout');
  });

  it('shows a DNS lookup failure for a host instead of empty records', async () => {
    renderWithProviders(<MemoryRouter><SourcesPage /></MemoryRouter>);
    await userEvent.click(await screen.findByRole('button', { name: /^network: kshb 41$/i }));
    const dialog = await screen.findByRole('dialog');
    const host = await within(dialog).findByTestId('net-host-keys.example.test');
    expect(within(host).getByText(/DNS lookup failed:/)).toBeInTheDocument();
    expect(within(host).getByText(/no record found/)).toBeInTheDocument();
  });

  it('shows an error when the report cannot be fetched', async () => {
    server.use(http.post('*/api/v1/sources/:id/network', () => HttpResponse.json({ title: 'Forbidden', status: 403 }, { status: 403 })));
    renderWithProviders(<MemoryRouter><SourcesPage /></MemoryRouter>);
    await userEvent.click(await screen.findByRole('button', { name: /^network: kshb 41$/i }));
    expect(await screen.findByRole('alert')).toHaveTextContent('Forbidden');
  });

  it('copies as text and as JSON', async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.defineProperty(navigator, 'clipboard', { value: { writeText }, configurable: true });
    renderWithProviders(<MemoryRouter><SourcesPage /></MemoryRouter>);
    await userEvent.click(await screen.findByRole('button', { name: /^network: kshb 41$/i }));
    const dialog = await screen.findByRole('dialog');
    await userEvent.click(await within(dialog).findByRole('button', { name: 'Copy as text' }));
    await waitFor(() => { expect(writeText).toHaveBeenCalledTimes(1); });
    const text = String(writeText.mock.calls[0]?.[0]);
    expect(text).toContain('Egress: IPv4 203.0.113.7  IPv6 2001:db8::7  country US  colo DFW');
    expect(text).toContain('IPV6 2001:db8:1::10 [public] connect 12 ms  status 403');
    expect(text).toContain('curl -4');
    expect(text).toContain('DNS error: no record found for keys.example.test');
    await userEvent.click(within(dialog).getByRole('button', { name: 'Copy as JSON' }));
    await waitFor(() => { expect(writeText).toHaveBeenCalledTimes(2); });
    expect(JSON.parse(String(writeText.mock.calls[1]?.[0]))).toEqual(REPORT);
  });

  it('opens from a probe result carrying a network block without another request', async () => {
    renderWithProviders(
      <ProbeSummary
        result={{
          k: 's:d4e9ec8986d2', source_id: null, verdict: 'geo_blocked_here', probed_from: 'dallas-1',
          probed_at: '2026-11-07T05:00:00Z', detail: null, facts: null, events: [], network: REPORT,
        }}
      />,
    );
    await userEvent.click(screen.getByRole('button', { name: /network path of this probe/i }));
    const dialog = await screen.findByRole('dialog');
    expect(within(dialog).getByTestId('net-host-content.uplynk.com')).toBeInTheDocument();
    expect(networkPosts).toEqual([]);
  });
});
