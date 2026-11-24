import { describe, expect, it, vi, afterEach } from 'vitest';
import { render, screen } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { I18nProvider } from '@lingui/react';
import { i18n } from '@lingui/core';
import { MemoryRouter } from 'react-router-dom';
import { messages } from '../locales/en/messages';
import { SwitcherPage } from './SwitcherPage';

const preview = vi.hoisted(() => ({
  calls: [] as { path: string; enabled: boolean }[],
  url: undefined as string | undefined,
  state: 'loading', requestFailed: false,
}));
vi.mock('../preview/useJpegPreview', () => ({
  useJpegStill: (path: string, _rate: number, enabled = true) => {
    preview.calls.push({ path, enabled });
    return preview;
  },
}));
vi.mock('../api/switcher', () => ({ getSwitcher: () => Promise.resolve({ program_source: 'a', preview_source: 'b', tick: 1 }), commandSwitcher: vi.fn(), planSwitcher: vi.fn() }));
vi.mock('../api/patch', () => ({ getPatch: () => Promise.resolve({ mixer: [] }), planPatch: vi.fn(), takePatch: vi.fn() }));
vi.mock('../resources/queries', () => ({ useSources: () => ({ data: [] }) }));
vi.mock('../realtime/shared', () => ({ subscribeRealtime: () => () => undefined }));
i18n.load('en', messages);
i18n.activate('en');
afterEach(() => { preview.calls = []; preview.url = undefined; preview.state = 'loading'; preview.requestFailed = false; });
function page() {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  render(<QueryClientProvider client={client}><I18nProvider i18n={i18n}><MemoryRouter><SwitcherPage /></MemoryRouter></I18nProvider></QueryClientProvider>);
}
describe('Switcher monitor preview evidence', () => {
  it('enables polling for both Program and Preview endpoints', () => {
    page();
    expect(preview.calls).toEqual(expect.arrayContaining([
      { path: '/api/v1/preview/program.jpg', enabled: true },
      { path: '/api/v1/preview/switcher/me1/preview.jpg', enabled: true },
    ]));
  });
  it('shows failed requests rather than waiting on both monitors', () => {
    preview.requestFailed = true;
    page();
    expect(screen.getAllByText('Preview request failed — retrying')).toHaveLength(2);
    expect(screen.queryByText('Waiting for a real frame')).toBeNull();
  });
  it.each(['stale', 'fresh'])('marks a retained %s image unverified after a failed poll', (state) => {
    preview.url = 'blob:previous-frame'; preview.state = state; preview.requestFailed = true;
    page();
    expect(screen.getAllByText('Last received image — live video cannot be verified')).toHaveLength(2);
  });
});
