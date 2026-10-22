// The device lifecycle badge must convey every state with icon + TEXT — never
// colour alone (WCAG 2.1 SC 1.4.1; managed-devices.md §9). All six states from
// the wire (DISCOVERED / ADOPTING / ONLINE / DEGRADED / AUTH_FAILED /
// UNREACHABLE) render a human-readable label.
import { describe, expect, it } from 'vitest';
import { screen } from '@testing-library/react';

import { DeviceStateBadge } from './DeviceStateBadge';
import { renderWithProviders } from '../test/render';

describe('DeviceStateBadge', () => {
  it.each([
    ['DISCOVERED', 'Discovered'],
    ['ADOPTING', 'Adopting'],
    ['ONLINE', 'Online'],
    ['DEGRADED', 'Degraded'],
    ['AUTH_FAILED', 'Auth failed'],
    ['UNREACHABLE', 'Unreachable'],
  ] as const)('renders %s as visible text', (state, label) => {
    renderWithProviders(<DeviceStateBadge state={state} />);
    expect(screen.getByText(label)).toBeInTheDocument();
  });
});
