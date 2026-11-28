// Route document titles must follow the active locale (#594). The test renders
// the real AppLayout on the production "/" route and injects a test-only catalog
// through the same i18n.load/i18n.activate seam PatchMatrix.test.tsx uses. No
// test locale is added to SUPPORTED_LOCALES; the catalog is restored in finally.
import { afterEach, describe, expect, it } from 'vitest';
import { waitFor } from '@testing-library/react';
import type { RenderResult } from '@testing-library/react';
import { MemoryRouter, Route, Routes } from 'react-router-dom';
import { i18n } from '@lingui/core';
import type { Messages } from '@lingui/core';
import { OPERATOR_SURFACE_HEADINGS } from './operatorRoutes';

import { AppLayout } from './AppLayout';
import { I18nProvider } from '../i18n/I18nProvider';
import { ThemeProvider } from '../theme/ThemeProvider';
import { renderWithProviders } from '../test/render';
import { messages as enMessages } from '../locales/en/messages';

// Catalog ids are content hashes; the test catalog translates only the Production
// desk heading. Every other heading is absent on purpose so the fallback path is
// exercised.
const PRODUCTION_DESK_ID = OPERATOR_SURFACE_HEADINGS['/'].id;
const TEST_LOCALE = 'xt';
const TEST_TITLE = 'Bureau du direct';

const testMessages: Messages = { [PRODUCTION_DESK_ID]: TEST_TITLE };

function renderAt(path: string): RenderResult {
  return renderWithProviders(
    <ThemeProvider>
      <I18nProvider>
        <MemoryRouter initialEntries={[path]}>
          <Routes>
            <Route element={<AppLayout />}>
              <Route path="/" element={<h1>probe</h1>} />
              <Route path="/routing" element={<h1>probe</h1>} />
            </Route>
          </Routes>
        </MemoryRouter>
      </I18nProvider>
    </ThemeProvider>,
  );
}

afterEach(() => {
  i18n.activate('en');
});

describe('route document titles follow the active locale (#594)', () => {
  it('derives the "/" title from the injected locale', async () => {
    i18n.load(TEST_LOCALE, testMessages);
    i18n.load('en', enMessages);
    try {
      // Source locale first: the title is the English heading.
      i18n.activate('en');
      const first = renderAt('/');
      await waitFor(() => {
        expect(document.title).toBe('Production desk — Conspect');
      });
      first.unmount();

      // Switching the active locale must CHANGE the title: the defect a single-locale test misses.
      // I18nProvider re-activates the negotiated locale on mount, so the injected
      // locale is activated after mount and must then win.
      const second = renderAt('/');
      await waitFor(() => { expect(document.title).toBe('Production desk — Conspect'); });
      i18n.activate(TEST_LOCALE);
      await waitFor(() => {
        expect(document.title).toBe(`${TEST_TITLE} — Conspect`);
      });
      second.unmount();
    } finally {
      i18n.activate('en');
    }
  });

  it('falls back to the source heading for a route absent from the injected catalog', async () => {
    i18n.load(TEST_LOCALE, testMessages);
    i18n.load('en', enMessages);
    try {
      const view = renderAt('/routing');
      i18n.activate(TEST_LOCALE);
      await waitFor(() => {
        expect(i18n.locale).toBe(TEST_LOCALE);
        expect(document.title).toBe('Routing & outputs — Conspect');
      });
      view.unmount();
    } finally {
      i18n.activate('en');
    }
  });
});
