// Tests for the composed LayoutEditor: live validation gates saving, and a
// valid edit serializes to the expected opaque config body. Drives only the
// accessible (default) Cells tab so no konva canvas is mounted.
import { describe, expect, it, vi } from 'vitest';
import { screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';

import { LayoutEditor } from './LayoutEditor';
import type { LayoutSavePayload } from './LayoutEditor';
import { emptyLayout, addCell } from '../model';
import type { LayoutModel } from '../model';
import type { SourceView } from '../../resources/types';
import { renderWithProviders } from '../../test/render';

const SOURCES: readonly SourceView[] = [
  { id: 'cam-1', name: 'Camera One', kind: 'rtsp', rawKind: 'rtsp', editable: true, locator: 'rtsp://cam-1' },
];

function namedLayout(): LayoutModel {
  return addCell(emptyLayout('Studio'), {
    id: 'a',
    label: 'Alpha',
    sourceId: 'cam-1',
  });
}

describe('LayoutEditor', () => {
  it('disables Save while the layout is invalid and lists the issues', () => {
    const onSave = vi.fn();
    // An empty draft: no name, no cells -> two validation issues.
    renderWithProviders(
      <LayoutEditor sources={SOURCES} overlays={[]} onSave={onSave} />,
    );
    expect(screen.getByRole('button', { name: 'Save layout' })).toBeDisabled();
    const alert = screen.getByRole('alert');
    expect(alert.textContent).toContain('layout name');
    expect(alert.textContent).toContain('at least one cell');
  });

  it('enables Save for a valid layout and emits the serialized body', async () => {
    const user = userEvent.setup();
    const onSave = vi.fn<(p: LayoutSavePayload) => void>();
    renderWithProviders(
      <LayoutEditor
        initial={namedLayout()}
        sources={SOURCES}
        overlays={[]}
        onSave={onSave}
      />,
    );
    const save = screen.getByRole('button', { name: 'Save layout' });
    expect(save).toBeEnabled();
    await user.click(save);
    expect(onSave).toHaveBeenCalledTimes(1);
    const payload = onSave.mock.calls[0]?.[0];
    expect(payload?.name).toBe('Studio');
    // The opaque body matches the multiview-config absolute-layout shape.
    expect(payload?.body).toMatchObject({
      schema_version: 1,
      layout: { kind: 'absolute' },
      canvas: { fps: '30/1' },
    });
    const cells = (payload?.body as { cells: unknown[] }).cells;
    expect(cells).toHaveLength(1);
    expect(cells[0]).toMatchObject({ id: 'a', source: { input_id: 'cam-1' } });
  });

  it('adds a cell via the Add cell button', async () => {
    const user = userEvent.setup();
    const onSave = vi.fn();
    renderWithProviders(
      <LayoutEditor
        initial={emptyLayout('Studio')}
        sources={SOURCES}
        overlays={[]}
        onSave={onSave}
      />,
    );
    // Scope to the accessible Cells region: the editor toolbar now carries its
    // own fieldsets (canvas controls + preset seeds), which are also `group`s.
    const cellsRegion = (): HTMLElement => screen.getByRole('region', { name: 'Cells' });
    expect(within(cellsRegion()).queryByRole('group')).not.toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: 'Add cell' }));
    // A new cell fieldset now exists, and the no-cells issue is cleared. The
    // row nests further groups (Border fieldset + the property disclosures),
    // so count the cell rows by their accessible (legend) name.
    expect(
      within(cellsRegion()).getAllByRole('group', { name: /New cell/ }),
    ).toHaveLength(1);
    expect(screen.getByRole('button', { name: 'Save layout' })).toBeEnabled();
  });
});
