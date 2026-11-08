import { describe, expect, it, vi } from 'vitest';
import { fireEvent } from '@testing-library/react';
import { renderWithProviders } from '../test/render';
import { SourcePreviewButton } from './SourcePreviewButton';
vi.mock('./capabilities', () => ({ usePreviewCapabilities: () => ({ data: undefined }) }));
vi.mock('./PreviewSurface', () => ({ PreviewSurface: () => <div data-testid="source-preview-player" /> }));
describe('SourcePreviewButton', () => {
  it('opens a preview only on request and removes it when closed', () => {
    const view = renderWithProviders(<SourcePreviewButton id="cam-1" name="North camera" />);
    expect(view.queryByTestId('source-preview-player')).toBeNull();
    fireEvent.click(view.getByRole('button', { name: 'Preview: North camera' }));
    expect(view.getByTestId('source-preview-player')).toBeInTheDocument();
    fireEvent.keyDown(view.getByRole('dialog'), { key: 'Escape' });
    expect(view.queryByTestId('source-preview-player')).toBeNull();
  });
});
