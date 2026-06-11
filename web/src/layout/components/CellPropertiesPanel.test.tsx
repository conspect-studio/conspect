// Tests for the SHARED cell-properties panel (one implementation, mounted by
// both the absolute CellsForm and the grid per-area panel): the failover
// slate selector drives `on_loss`, and the appearance/degradation fields
// round-trip through the pure CellProperties model.
import { useState } from 'react';
import type { JSX } from 'react';
import { describe, expect, it } from 'vitest';
import { screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';

import { CellPropertiesPanel } from './CellPropertiesPanel';
import {
  emptyCellProperties,
  parseCellProperties,
  serializeCellProperties,
} from '../cellProps';
import type { CellProperties } from '../cellProps';
import { renderWithProviders } from '../../test/render';

function Harness({ initial }: { readonly initial?: CellProperties }): JSX.Element {
  const [value, setValue] = useState<CellProperties>(
    initial ?? emptyCellProperties(),
  );
  return (
    <div>
      <output data-testid="serialized">
        {JSON.stringify(serializeCellProperties(value))}
      </output>
      <CellPropertiesPanel idPrefix="cell-a" value={value} onChange={setValue} />
    </div>
  );
}

function serialized(): unknown {
  return JSON.parse(screen.getByTestId('serialized').textContent);
}

describe('CellPropertiesPanel', () => {
  it('drives on_loss through the failover selector and clears back to default', async () => {
    const user = userEvent.setup();
    renderWithProviders(<Harness />);
    expect(serialized()).toEqual({});

    const select = screen.getByRole('combobox', { name: 'On signal loss' });
    await user.click(select);
    await user.click(screen.getByRole('option', { name: '"No signal" card' }));
    expect(serialized()).toEqual({ on_loss: { slate: 'no_signal' } });

    await user.click(select);
    await user.click(screen.getByRole('option', { name: 'Black' }));
    expect(serialized()).toEqual({ on_loss: { slate: 'black' } });

    // Back to the default: the key is OMITTED (engine defaults to bars).
    await user.click(select);
    await user.click(screen.getByRole('option', { name: 'Default (colour bars)' }));
    expect(serialized()).toEqual({});
  });

  it('edits appearance fields into their snake_case body keys', async () => {
    const user = userEvent.setup();
    renderWithProviders(<Harness />);

    await user.type(screen.getByLabelText('Opacity (0–1)'), '0.5');
    await user.type(screen.getByLabelText('Corner radius (px)'), '8');
    await user.type(screen.getByLabelText('Width (px)'), '2');
    await user.type(screen.getByLabelText('Colour (hex)'), '#ff0000');
    expect(serialized()).toEqual({
      opacity: 0.5,
      corner_radius: 8,
      border: { width_px: 2, color: '#ff0000' },
    });
  });

  it('edits degradation (priority + strategy) under the qos key', async () => {
    const user = userEvent.setup();
    renderWithProviders(<Harness />);

    await user.type(screen.getByLabelText('Priority'), '10');
    const strategy = screen.getByRole('combobox', { name: 'Strategy' });
    await user.click(strategy);
    await user.click(screen.getByRole('option', { name: 'maintain-fps' }));
    expect(serialized()).toEqual({
      qos: { priority: 10, degradation: 'maintain-fps' },
    });
  });

  it('clearing the last border field drops the border key entirely', async () => {
    const user = userEvent.setup();
    renderWithProviders(<Harness />);
    const width = screen.getByLabelText('Width (px)');
    await user.type(width, '2');
    expect(serialized()).toEqual({ border: { width_px: 2 } });
    await user.clear(width);
    expect(serialized()).toEqual({});
  });

  it('shows unknown scaler/degradation tokens as custom-preserved, never "Default"', async () => {
    const user = userEvent.setup();
    renderWithProviders(
      <Harness
        initial={parseCellProperties({
          scaler: 'sharp',
          qos: { degradation: 'shed-everything' },
        })}
      />,
    );
    // The selectors tell the truth about the stored tokens…
    expect(
      screen.getByRole('combobox', { name: 'Scaler' }).textContent,
    ).toContain('sharp');
    expect(
      screen.getByRole('combobox', { name: 'Strategy' }).textContent,
    ).toContain('shed-everything');
    // …and an unrelated edit leaves them untouched (no silent replacement).
    await user.type(screen.getByLabelText('Opacity (0–1)'), '0.5');
    expect(serialized()).toEqual({
      opacity: 0.5,
      scaler: 'sharp',
      qos: { degradation: 'shed-everything' },
    });
    // Picking a known value still replaces the token explicitly.
    const scaler = screen.getByRole('combobox', { name: 'Scaler' });
    await user.click(scaler);
    await user.click(screen.getByRole('option', { name: 'lanczos' }));
    expect(serialized()).toEqual({
      opacity: 0.5,
      scaler: 'lanczos',
      qos: { degradation: 'shed-everything' },
    });
  });
});
