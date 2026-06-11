// Shared test render helper: wraps a UI under the providers it needs (Lingui
// i18n for the `t`/`<Trans>` macros, and a fresh React Query client). Component
// tests use this so macros resolve exactly as they do in the running app.
import type { ReactElement, ReactNode } from 'react';
import { render } from '@testing-library/react';
import type { RenderResult } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { i18n } from '@lingui/core';
import { I18nProvider } from '@lingui/react';

import { messages } from '../locales/en/messages';

i18n.load('en', messages);
i18n.activate('en');

/** Wrap children with the i18n + query providers used in tests. */
export function TestProviders({ children }: { children: ReactNode }): ReactElement {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  return (
    <QueryClientProvider client={client}>
      <I18nProvider i18n={i18n}>{children}</I18nProvider>
    </QueryClientProvider>
  );
}

/** Render a UI with the standard provider stack. */
export function renderWithProviders(ui: ReactElement): RenderResult {
  return render(ui, { wrapper: TestProviders });
}
