import { describe, expect, it } from 'vitest';
import { screen } from '@testing-library/react';
import { MemoryRouter } from 'react-router-dom';
import { renderWithProviders } from '../../test/render';
import { ShowPage } from './ShowPage';

describe('ShowPage', () => {
  it('renders an explicit unavailable Show with disabled controls and real links', () => {
    const { container } = renderWithProviders(<MemoryRouter><ShowPage /></MemoryRouter>);
    expect(container.querySelector('[data-approved-page="show"]')).not.toBeNull();
    expect(screen.getByRole('heading', { level: 1, name: 'Show rundown' })).toBeInTheDocument();
    for (const name of ['Take next', 'Stop live item', 'Skip cue', 'Hold cue', 'Arm cue']) {
      expect(screen.getByRole('button', { name })).toBeDisabled();
    }
    expect(screen.getByText(/No production rundown API exists/, { selector: 'p' })).toBeInTheDocument();
    expect(screen.getByRole('link', { name: 'Graphics' })).toHaveAttribute('href', '/graphics');
    expect(screen.getByRole('link', { name: 'Salvos' })).toHaveAttribute('href', '/salvos');
    expect(screen.getByTestId('show-zone').textContent).not.toBe('');
    expect(screen.queryByText('OVERRUN')).toBeNull();
  });

  it('tones disabled controls with the design tokens and states the reason inline', () => {
    renderWithProviders(<MemoryRouter><ShowPage /></MemoryRouter>);
    expect(screen.getByRole('button', { name: 'Take next' })).toHaveClass('show-btn-take');
    expect(screen.getByRole('button', { name: 'Stop live item' })).toHaveClass('show-btn-stop');
    expect(screen.getByRole('button', { name: 'Arm cue' })).toHaveClass('show-btn-arm');
    expect(screen.getByTestId('cue-unavailable')).toHaveTextContent('Unavailable');
  });
});
