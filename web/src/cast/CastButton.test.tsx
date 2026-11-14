// CastButton + CastProvider: the browser-cast flow end to end with a mocked
// Cast Web Sender SDK, the cast config over MSW and a fake stream API.
import { StrictMode } from 'react';
import type { ReactElement } from 'react';
import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it } from 'vitest';
import { act, render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';

import { CastButton } from './CastButton';
import { CastController } from './castController';
import { CastProvider } from './CastProvider';
import { CAST_NAMESPACE } from './castReport';
import { resetCastSdkLoaderForTests } from './sdkLoader';
import { fakeApi, installMemoryStorage, TOKEN_PATH } from './testApi';
import type { FakeApi } from './testApi';
import { installMockCastSdk } from './testMocks';
import type { MockCastSdk } from './testMocks';
import { TestProviders } from '../test/render';

const TARGET = { key: 'feed:bus:program', name: 'Program', feed: { bus: 'program' as const } };

const server = setupServer(
  http.get('http://localhost:3000/api/v1/cast/config', () =>
    HttpResponse.json({
      receiver_app_id: '8E05190C',
      receiver_path: '/cast/receiver/',
      namespace: CAST_NAMESPACE,
      outputs: [{ id: 'cast-1', active: false, ready: true, streams: 0 }],
    }),
  ),
);

beforeAll(() => {
  server.listen({ onUnhandledRequest: 'error' });
});
afterEach(() => {
  server.resetHandlers();
});
afterAll(() => {
  server.close();
});

describe('CastButton', () => {
  let sdk: MockCastSdk;
  let api: FakeApi;
  let controller: CastController;

  function renderButton(wrap: (ui: ReactElement) => ReactElement = (ui) => ui): ReturnType<typeof render> {
    return render(
      wrap(
        <TestProviders>
          <CastProvider controller={controller}>
            <CastButton target={TARGET} />
          </CastProvider>
        </TestProviders>,
      ),
    );
  }

  beforeEach(() => {
    installMemoryStorage();
    resetCastSdkLoaderForTests();
    sdk = installMockCastSdk({ deviceName: 'Living Room TV' });
    api = fakeApi();
    controller = new CastController(api);
  });

  afterEach(() => {
    delete window.cast;
    delete window.chrome;
    delete window.__onGCastApiAvailable;
  });

  it('casts the token URL on the configured receiver, shows "Casting to <device>", then stops', async () => {
    const user = userEvent.setup();
    renderButton();
    const button = await screen.findByRole('button', { name: /^Cast: Program$/ });
    await waitFor(() => {
      expect(button).toBeEnabled();
    });
    await user.click(button);
    await waitFor(() => {
      expect(sdk.loadMedia).toHaveBeenCalledTimes(1);
    });
    expect(sdk.context.setOptions).toHaveBeenCalledWith(expect.objectContaining({ receiverApplicationId: '8E05190C' }));
    const request = sdk.loadMedia.mock.calls[0]?.[0] as { media: { contentId: string } };
    expect(request.media.contentId).toBe(`${window.location.origin}${TOKEN_PATH}`);
    act(() => {
      sdk.emitMessage(CAST_NAMESPACE, { v: 1, state: 'playing', error: null, stats: null });
    });
    expect(await screen.findByText(/Casting to Living Room TV/)).toBeInTheDocument();

    await user.click(screen.getByRole('button', { name: /Stop casting/ }));
    await waitFor(() => {
      expect(sdk.endSession).toHaveBeenCalledWith(true);
    });
    expect(api.remove).toHaveBeenCalledWith('cs-1');
    expect(await screen.findByRole('button', { name: /^Cast: Program$/ })).toBeInTheDocument();
  });

  it('returns to idle with no error message when the user cancels the device picker', async () => {
    // eslint-disable-next-line @typescript-eslint/prefer-promise-reject-errors -- CAF rejects with the bare code string
    sdk.requestSession.mockImplementationOnce(() => Promise.reject('cancel'));
    const user = userEvent.setup();
    renderButton();
    const button = await screen.findByRole('button', { name: /^Cast: Program$/ });
    await waitFor(() => {
      expect(button).toBeEnabled();
    });
    await user.click(button);
    await waitFor(() => {
      expect(api.remove).toHaveBeenCalledWith('cs-1');
    });
    expect(screen.getByRole('button', { name: /^Cast: Program$/ })).toBeInTheDocument();
    expect(screen.queryByRole('alert')).not.toBeInTheDocument();
  });

  it('says Cast needs Chrome or Edge and never loads the SDK elsewhere', () => {
    delete window.chrome;
    renderButton();
    expect(screen.getByRole('button', { name: 'Cast needs Chrome or Edge on a computer or Android' })).toBeDisabled();
    expect(sdk.requestSession).not.toHaveBeenCalled();
  });

  it('still casts after the StrictMode dev re-mount', async () => {
    const user = userEvent.setup();
    renderButton((ui) => <StrictMode>{ui}</StrictMode>);
    const button = await screen.findByRole('button', { name: /^Cast: Program$/ });
    await waitFor(() => {
      expect(button).toBeEnabled();
    });
    await user.click(button);
    await waitFor(() => {
      expect(sdk.loadMedia).toHaveBeenCalledTimes(1);
    });
  });
});
