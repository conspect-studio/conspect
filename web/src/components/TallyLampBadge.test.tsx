// TallyLampBadge: verify the lamp colour is conveyed as TEXT (+ a glyph), never
// colour alone (WCAG 1.4.1). The colour name must be present for every lamp.
import { describe, expect, it } from 'vitest';
import { screen } from '@testing-library/react';

import { renderWithProviders } from '../test/render';
import { TallyLampBadge } from './TallyLampBadge';
import type { TallyColor } from '../api/tallyQueries';

const CASES: readonly [TallyColor, string][] = [
  ['Red', 'Red'],
  ['Green', 'Green'],
  ['Amber', 'Amber'],
  ['Off', 'Off'],
];

describe('TallyLampBadge', () => {
  it.each(CASES)('renders %s as visible text', (color, label) => {
    const { unmount } = renderWithProviders(<TallyLampBadge color={color} />);
    expect(screen.getByText(label)).toBeInTheDocument();
    unmount();
  });
});
