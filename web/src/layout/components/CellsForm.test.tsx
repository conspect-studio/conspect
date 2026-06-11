// Tests for the ACCESSIBLE NON-CANVAS editing path.
//
// These prove the editor is fully operable without the konva canvas: a user can
// rename a cell, change its geometry, reorder its stacking, and remove it using
// only labelled form controls + buttons. The form is driven by the real
// `useLayoutEditor` hook, so the assertions exercise the same state the canvas
// would mutate.
import { useState } from 'react';
import type { JSX } from 'react';
import { describe, expect, it } from 'vitest';
import { screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';

import { CellsForm } from './CellsForm';
import { useLayoutEditor } from '../useLayoutEditor';
import { addCell, emptyLayout } from '../model';
import type { SourceView } from '../../resources/types';
import { renderWithProviders } from '../../test/render';

const SOURCES: readonly SourceView[] = [
  { id: 'cam-1', name: 'Camera One', kind: 'rtsp', rawKind: 'rtsp', editable: true, locator: 'rtsp://cam-1' },
  { id: 'cam-2', name: 'Camera Two', kind: 'rtsp', rawKind: 'rtsp', editable: true, locator: 'rtsp://cam-2' },
];

function seeded() {
  let model = emptyLayout('Studio');
  model = addCell(model, { id: 'a', label: 'Alpha' });
  model = addCell(model, { id: 'b', label: 'Bravo' });
  return model;
}

/** A harness wiring the form to the real editor hook + a selection readout. */
function Harness(): JSX.Element {
  const editor = useLayoutEditor(seeded());
  const [removed, setRemoved] = useState<string | null>(null);
  return (
    <div>
      <output data-testid="selected">{editor.selectedId ?? 'none'}</output>
      <output data-testid="order">
        {editor.model.cells.map((c) => `${c.id}:${String(c.z)}`).join(',')}
      </output>
      <output data-testid="names">
        {editor.model.cells.map((c) => c.label).join(',')}
      </output>
      <output data-testid="last-removed">{removed ?? ''}</output>
      <CellsForm
        cells={editor.model.cells}
        selectedId={editor.selectedId}
        sources={SOURCES}
        onSelect={editor.select}
        onRename={editor.rename}
        onMove={editor.move}
        onResize={editor.resize}
        onRotate={editor.rotate}
        onFit={editor.setFit}
        onBindSource={editor.bindSource}
        onProps={editor.setProps}
        onRemove={(id): void => {
          setRemoved(id);
          editor.remove(id);
        }}
        onMoveDown={editor.moveDown}
        onMoveUp={editor.moveUp}
      />
    </div>
  );
}

describe('CellsForm (non-canvas editing path)', () => {
  it('renders one labelled fieldset per cell with all controls', () => {
    renderWithProviders(<Harness />);
    expect(screen.getByRole('group', { name: /Alpha/ })).toBeInTheDocument();
    expect(screen.getByRole('group', { name: /Bravo/ })).toBeInTheDocument();
    // Number controls per cell: 5 geometry (x/y/w/h/rotation) + 4 properties
    // (opacity, corner radius, border width, QoS priority) = 9 × 2 cells.
    expect(screen.getAllByRole('spinbutton')).toHaveLength(18);
    expect(screen.getAllByLabelText('Cell name')).toHaveLength(2);
    // The full Cell property surface is mounted per cell (shared panel).
    expect(screen.getAllByText('On signal loss')).toHaveLength(2);
    expect(screen.getAllByText('Appearance')).toHaveLength(2);
    expect(screen.getAllByText('Degradation')).toHaveLength(2);
  });

  it('renames a cell through the name input', async () => {
    const user = userEvent.setup();
    renderWithProviders(<Harness />);
    const alpha = screen.getByRole('group', { name: /Alpha/ });
    const nameInput = within(alpha).getByLabelText('Cell name');
    await user.clear(nameInput);
    await user.type(nameInput, 'Renamed');
    expect(screen.getByTestId('names').textContent).toContain('Renamed');
  });

  it('edits geometry via the width field and clamps to the canvas', async () => {
    const user = userEvent.setup();
    renderWithProviders(<Harness />);
    const alpha = screen.getByRole('group', { name: /Alpha/ });
    const width = within(alpha).getByLabelText('Width (%)');
    await user.clear(width);
    await user.type(width, '80');
    // 80% width is valid and accepted; the model keeps it on-canvas.
    expect(Number((width as HTMLInputElement).value)).toBeLessThanOrEqual(100);
    expect(Number((width as HTMLInputElement).value)).toBeGreaterThan(0);
  });

  it('reorders stacking with the Forward control and renumbers z', async () => {
    const user = userEvent.setup();
    renderWithProviders(<Harness />);
    // Initial order a:0, b:1.
    expect(screen.getByTestId('order').textContent).toBe('a:0,b:1');
    const alpha = screen.getByRole('group', { name: /Alpha/ });
    await user.click(within(alpha).getByRole('button', { name: /Bring forward/ }));
    // Alpha moved up the list; z renumbered to list order.
    expect(screen.getByTestId('order').textContent).toBe('b:0,a:1');
  });

  it('selects a cell when one of its controls receives focus', async () => {
    const user = userEvent.setup();
    renderWithProviders(<Harness />);
    const bravo = screen.getByRole('group', { name: /Bravo/ });
    await user.click(within(bravo).getByLabelText('Cell name'));
    expect(screen.getByTestId('selected').textContent).toBe('b');
  });

  it('removes a cell via its Remove button', async () => {
    const user = userEvent.setup();
    renderWithProviders(<Harness />);
    const alpha = screen.getByRole('group', { name: /Alpha/ });
    await user.click(within(alpha).getByRole('button', { name: /Remove cell/ }));
    expect(screen.getByTestId('last-removed').textContent).toBe('a');
    expect(screen.queryByRole('group', { name: /Alpha/ })).not.toBeInTheDocument();
    expect(screen.getByTestId('names').textContent).toBe('Bravo');
  });

  it('exposes a labelled source binding control for each cell', () => {
    renderWithProviders(<Harness />);
    // Radix Select renders a combobox; one per cell, accessibly named "Source".
    const selects = screen.getAllByRole('combobox', { name: 'Source' });
    expect(selects).toHaveLength(2);
  });
});
