// Tests for the composed GridLayoutEditor: a no-op edit saves the IDENTICAL
// body (lossless), the areas matrix is keyboard-operable (arrow keys + Shift
// select a rectangle; assignment is validated against the Rust contiguity
// rule), tracks are editable as chips, and the per-area panel binds sources.
// Drives only the default "Areas & cells" tab so no konva canvas mounts.
import { describe, expect, it, vi } from 'vitest';
import { screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { MemoryRouter } from 'react-router-dom';

import { GridLayoutEditor } from './GridLayoutEditor';
import type { LayoutSavePayload } from './LayoutEditor';
import { fromGridLayoutBody } from '../gridModel';
import type { GridModel } from '../gridModel';
import type { SourceView } from '../../resources/types';
import { renderWithProviders } from '../../test/render';

const SOURCES: readonly SourceView[] = [
  { id: 'in_a', name: 'Source A', kind: 'test', rawKind: 'test', editable: true, locator: '' },
  { id: 'in_x', name: 'Source X', kind: 'rtsp', rawKind: 'rtsp', editable: true, locator: 'rtsp://x' },
];

/** The frigate demo working-layout body (the seeded 2x2 grid). */
function frigateBody(): Record<string, unknown> {
  return {
    canvas: {
      width: 1920,
      height: 1080,
      fps: '25/1',
      pixel_format: 'nv12',
      background: '#101014',
      color: { profile: 'sdr-bt709-limited' },
    },
    layout: {
      kind: 'grid',
      columns: ['1fr', '1fr'],
      rows: ['1fr', '1fr'],
      gap: 4,
      areas: ['a b', 'c d'],
    },
    cells: ['a', 'b', 'c', 'd'].map((area) => ({
      id: `cell_${area}`,
      area,
      z: 0,
      fit: 'contain',
      on_loss: { slate: 'bars' },
      source: { input_id: `in_${area}` },
    })),
  };
}

function loadModel(): GridModel {
  const model = fromGridLayoutBody('lay-1', 'Working', frigateBody());
  if (model === undefined) {
    throw new Error('fixture must parse');
  }
  return model;
}

function renderEditor(onSave = vi.fn<(p: LayoutSavePayload) => void>()): {
  onSave: ReturnType<typeof vi.fn<(p: LayoutSavePayload) => void>>;
} {
  renderWithProviders(
    // The editor carries a HelpLink (react-router <Link>), so it needs a router.
    <MemoryRouter>
      <GridLayoutEditor
        initial={loadModel()}
        sources={SOURCES}
        onSave={onSave}
        onConvertToFreeForm={vi.fn()}
      />
    </MemoryRouter>,
  );
  return { onSave };
}

describe('GridLayoutEditor', () => {
  it('a no-op edit saves the byte-identical body (lossless round-trip)', async () => {
    const user = userEvent.setup();
    const { onSave } = renderEditor();
    await user.click(screen.getByTestId('grid-save'));
    expect(onSave).toHaveBeenCalledTimes(1);
    const payload = onSave.mock.calls[0]?.[0];
    expect(payload?.name).toBe('Working');
    expect(payload?.body).toEqual(frigateBody());
  });

  it('selects a rectangle with the keyboard and assigns an area name', async () => {
    const user = userEvent.setup();
    const { onSave } = renderEditor();

    // Focus the top-left square, extend the selection one square right.
    screen.getByTestId('matrix-cell-0-0').focus();
    await user.keyboard('{Shift>}{ArrowRight}{/Shift}');
    expect(screen.getByTestId('matrix-cell-0-0')).toHaveAttribute(
      'aria-selected',
      'true',
    );
    expect(screen.getByTestId('matrix-cell-0-1')).toHaveAttribute(
      'aria-selected',
      'true',
    );
    expect(screen.getByTestId('matrix-cell-1-0')).toHaveAttribute(
      'aria-selected',
      'false',
    );

    await user.type(screen.getByLabelText('Area name'), 'main');
    await user.click(screen.getByTestId('assign-area'));
    expect(screen.getByTestId('matrix-cell-0-0')).toHaveTextContent('main');
    expect(screen.getByTestId('matrix-cell-0-1')).toHaveTextContent('main');

    // Areas a + b were overwritten: their bound cells are now orphans, the
    // engine would reject the document, so saving is blocked until resolved.
    expect(screen.getByTestId('grid-save')).toBeDisabled();
    // Re-point cell_a at the new area (keeping its source binding)…
    await user.click(screen.getByTestId('orphan-move-cell_a'));
    await user.click(screen.getByRole('option', { name: 'main' }));
    // …and drop cell_b.
    await user.click(screen.getByTestId('orphan-remove-cell_b'));

    await user.click(screen.getByTestId('grid-save'));
    const body = onSave.mock.calls[0]?.[0]?.body;
    const layout = body?.layout as Record<string, unknown>;
    expect(layout.areas).toEqual(['main main', 'c d']);
    const cells = body?.cells as Record<string, unknown>[];
    // The fresh area got an (unbound) cell on assignment, and the re-pointed
    // cell_a kept its binding.
    expect(cells.some((cell) => cell.area === 'main' && cell.id === 'cell_main')).toBe(
      true,
    );
    const cellA = cells.find((cell) => cell.id === 'cell_a');
    expect(cellA?.area).toBe('main');
    expect(cellA?.source).toEqual({ input_id: 'in_a' });
    expect(cells.some((cell) => cell.id === 'cell_b')).toBe(false);
  });

  it('rejects an assignment that would break an area, with a clear message', async () => {
    const user = userEvent.setup();
    renderEditor();
    // Assigning only the top-left square to "d" would make d two diagonal
    // squares — not a rectangle.
    screen.getByTestId('matrix-cell-0-0').focus();
    await user.type(screen.getByLabelText('Area name'), 'd');
    await user.click(screen.getByTestId('assign-area'));
    const status = screen.getByTestId('matrix-status');
    expect(status).toHaveTextContent(/rectangle/);
    // The matrix is unchanged.
    expect(screen.getByTestId('matrix-cell-0-0')).toHaveTextContent('a');
  });

  it('edits and adds tracks as chips', async () => {
    const user = userEvent.setup();
    const { onSave } = renderEditor();
    const first = screen.getByTestId('track-columns-0');
    await user.clear(first);
    await user.type(first, '2fr');
    await user.click(screen.getByTestId('add-track-rows'));
    await user.click(screen.getByTestId('grid-save'));
    const layout = onSave.mock.calls[0]?.[0]?.body.layout as Record<string, unknown>;
    expect(layout.columns).toEqual(['2fr', '1fr']);
    expect(layout.rows).toEqual(['1fr', '1fr', '1fr']);
    // The new bottom row duplicates the edge areas, staying rectangular.
    expect(layout.areas).toEqual(['a b', 'c d', 'c d']);
  });

  it('an invalid track blocks saving and lists the issue', async () => {
    const user = userEvent.setup();
    renderEditor();
    const first = screen.getByTestId('track-columns-0');
    await user.clear(first);
    await user.type(first, 'banana');
    expect(screen.getByTestId('grid-save')).toBeDisabled();
    expect(screen.getByRole('alert').textContent).toContain('1fr, 200px or 25%');
  });

  it('binds a source and the full property panel through the per-area panel', async () => {
    const user = userEvent.setup();
    const { onSave } = renderEditor();

    await user.click(screen.getByTestId('area-chip-a'));
    // The shared properties panel is mounted for the area's cell.
    expect(screen.getByTestId('cell-props-grid-cell_a')).toBeInTheDocument();

    await user.click(screen.getByTestId('area-cell-source'));
    await user.click(screen.getByRole('option', { name: /Source X/ }));

    const slate = screen.getByRole('combobox', { name: 'On signal loss' });
    await user.click(slate);
    await user.click(screen.getByRole('option', { name: 'Black' }));

    await user.click(screen.getByTestId('grid-save'));
    const cells = onSave.mock.calls[0]?.[0]?.body.cells as Record<string, unknown>[];
    const cellA = cells.find((cell) => cell.id === 'cell_a');
    expect(cellA).toEqual({
      id: 'cell_a',
      area: 'a',
      z: 0,
      fit: 'contain',
      on_loss: { slate: 'black' },
      source: { input_id: 'in_x' },
    });
  });

  it('renames an area across the matrix and its cell', async () => {
    const user = userEvent.setup();
    const { onSave } = renderEditor();
    await user.click(screen.getByTestId('area-chip-a'));
    await user.type(screen.getByTestId('rename-area-input'), 'hero');
    await user.click(screen.getByTestId('rename-area'));
    expect(screen.getByTestId('matrix-cell-0-0')).toHaveTextContent('hero');
    await user.click(screen.getByTestId('grid-save'));
    const body = onSave.mock.calls[0]?.[0]?.body;
    const layout = body?.layout as Record<string, unknown>;
    expect(layout.areas).toEqual(['hero b', 'c d']);
    const cells = body?.cells as Record<string, unknown>[];
    expect(cells.some((cell) => cell.area === 'hero' && cell.id === 'cell_a')).toBe(
      true,
    );
  });

  it('removing an area cell yields a warning advisory, not a save blocker', async () => {
    const user = userEvent.setup();
    renderEditor();
    await user.click(screen.getByTestId('area-chip-d'));
    await user.click(screen.getByRole('button', { name: /Remove cell/ }));
    expect(screen.getByRole('note').textContent).toContain('no cell');
    expect(screen.getByTestId('grid-save')).toBeEnabled();
  });
});
