import { describe, expect, it } from 'vitest';
import { screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { useState } from 'react';
import { renderWithProviders } from '../../test/render';
import { MediaSurface } from './MediaSurface';
import type { MediaTab } from './MediaSurface';

function Harness(): React.JSX.Element {
  const [tab, setTab] = useState<MediaTab>('transport');
  return <MediaSurface tab={tab} onTabChange={setTab} transport={<p>transport-body</p>} />;
}

describe('MediaSurface', () => {
  it('switches tabs and shows explicit unavailable capture and library states', async () => {
    const { container } = renderWithProviders(<Harness />);
    expect(container.querySelector('[data-approved-page="media"]')).not.toBeNull();
    expect(screen.getByText('transport-body')).toBeInTheDocument();
    await userEvent.click(screen.getByRole('button', { name: 'Capture' }));
    expect(screen.queryByText('transport-body')).toBeNull();
    expect(screen.getByTestId('media-capture-unavailable')).toBeInTheDocument();
    await userEvent.click(screen.getByRole('button', { name: 'Clip library' }));
    expect(screen.getByTestId('library-listing-unavailable')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Clip library' })).toHaveAttribute('aria-pressed', 'true');
  });
});
