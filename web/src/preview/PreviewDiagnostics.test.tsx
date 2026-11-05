import { afterEach, describe, expect, it, vi } from 'vitest';
import { fireEvent, screen, waitFor } from '@testing-library/react';
import { renderWithProviders } from '../test/render';
import { PreviewDiagnostics } from './PreviewDiagnostics';
import { safeStats } from './diagnostics';

afterEach(() => vi.unstubAllGlobals());
describe('diagnostic clipboard', () => {
  it.each([true, false])('reports clipboard success=%s without raw errors', async (success) => {
    const writeText = success ? vi.fn().mockResolvedValue(undefined) : vi.fn().mockRejectedValue(new Error('secret URL token'));
    vi.stubGlobal('navigator', { clipboard: { writeText } });
    renderWithProviders(<PreviewDiagnostics diagnostics={{ stage: 'ice-timeout', httpStatus: 201, elapsedMs: 15000, signalingMs: 20, connectedMs: null, ice: 'checking', peer: 'connecting', media: safeStats() }} />);
    fireEvent.click(screen.getByRole('button', { name: 'Copy diagnostics' }));
    await waitFor(() => expect(screen.getByRole('status')).toHaveTextContent(success ? 'Diagnostics copied.' : 'Could not copy diagnostics.'));
    expect(writeText).toHaveBeenCalledOnce();
    expect(document.body.textContent).not.toContain('secret');
  });
});
