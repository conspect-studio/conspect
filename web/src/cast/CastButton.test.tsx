// CastButton + CastProvider integration: the full browser-cast flow with a
// mocked Cast Web Sender SDK (no network calls for casting itself — PR 1
// casts an existing HLS output's own playlist URL directly).
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';

import { CastButton } from './CastButton';
import { CastProvider } from './CastProvider';
import { resetCastSdkLoaderForTests } from './sdkLoader';
import { installMockCastSdk } from './testMocks';
import type { MockCastSdk } from './testMocks';
import { TestProviders } from '../test/render';

const TARGET = { key: 'output:hls-main', name: 'Program', path: '/hls/main/index.m3u8' };

function renderButton(target = TARGET): ReturnType<typeof render> {
  return render(
    <TestProviders>
      <CastProvider>
        <CastButton target={target} />
      </CastProvider>
    </TestProviders>,
  );
}

describe('CastButton', () => {
  let sdk: MockCastSdk;

  beforeEach(() => {
    resetCastSdkLoaderForTests();
    sdk = installMockCastSdk({ deviceName: 'Living Room TV' });
  });

  afterEach(() => {
    delete window.cast;
    delete window.chrome;
    delete window.__onGCastApiAvailable;
  });

  it('casts, shows "Casting to <device>", then stops', async () => {
    const user = userEvent.setup();
    renderButton();

    await user.click(screen.getByRole('button', { name: /^Cast: Program$/ }));

    await waitFor(() => {
      expect(sdk.loadMedia).toHaveBeenCalledTimes(1);
    });
    const request = sdk.loadMedia.mock.calls[0]?.[0] as { media: { contentId: string; contentType: string } };
    expect(request.media.contentId).toBe(`${window.location.origin}/hls/main/index.m3u8`);
    expect(request.media.contentType).toBe('application/vnd.apple.mpegurl');

    expect(await screen.findByText(/Casting to Living Room TV/)).toBeInTheDocument();

    await user.click(screen.getByRole('button', { name: /Stop casting/ }));

    await waitFor(() => {
      expect(sdk.endSession).toHaveBeenCalledWith(true);
    });
    expect(await screen.findByRole('button', { name: /^Cast: Program$/ })).toBeInTheDocument();
  });

  it('returns to idle with no error message when the user cancels the device picker', async () => {
    sdk.context.getCurrentSession = (): null => null;
    // The CAF sender rejects with the bare chrome.cast.ErrorCode string.
    // eslint-disable-next-line @typescript-eslint/prefer-promise-reject-errors
    sdk.requestSession.mockImplementationOnce(() => Promise.reject('cancel'));
    const user = userEvent.setup();
    renderButton();

    await user.click(screen.getByRole('button', { name: /^Cast: Program$/ }));

    await waitFor(() => {
      expect(screen.getByRole('button', { name: /^Cast: Program$/ })).toBeInTheDocument();
    });
    expect(screen.queryByRole('alert')).not.toBeInTheDocument();
  });

  it('shows "Cast needs Chrome or Edge" and never loads the SDK on Safari/Firefox', () => {
    delete window.chrome;
    renderButton();

    expect(screen.getByRole('button', { name: 'Cast needs Chrome or Edge' })).toBeDisabled();
    expect(sdk.requestSession).not.toHaveBeenCalled();
  });
});
