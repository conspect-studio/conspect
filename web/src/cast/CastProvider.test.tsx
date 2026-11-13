// The provider under React StrictMode: its dev unmount/re-mount must leave a
// working controller (the same instance survives in state).
import { StrictMode } from 'react';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { act, render, screen } from '@testing-library/react';
import { i18n } from '@lingui/core';
import { I18nProvider } from '@lingui/react';

import { messages } from '../locales/en/messages';
import { CastProvider } from './CastProvider';
import { CastButton } from './CastButton';
import { resetCastSdkLoaderForTests } from './sdkLoader';
import { installMockCastSdk } from './testMocks';
import type { MockCastSdk } from './testMocks';

i18n.load('en', messages);
i18n.activate('en');

describe('CastProvider', () => {
  let sdk: MockCastSdk;
  beforeEach(() => {
    resetCastSdkLoaderForTests();
    sdk = installMockCastSdk();
  });
  afterEach(() => {
    delete window.cast;
    delete window.chrome;
    delete window.__onGCastApiAvailable;
  });

  it('still casts after the StrictMode dev re-mount', async () => {
    render(
      <StrictMode>
        <I18nProvider i18n={i18n}>
          <CastProvider>
            <CastButton target={{ key: 'output:a', name: 'Program', path: '/hls/a/index.m3u8' }} />
          </CastProvider>
        </I18nProvider>
      </StrictMode>,
    );
    await act(async () => {
      screen.getByRole('button', { name: /cast/i }).click();
      await Promise.resolve();
    });
    expect(await screen.findByText(/Casting to Living Room TV/)).toBeInTheDocument();
    expect(sdk.loadMedia).toHaveBeenCalledTimes(1);
  });
});
