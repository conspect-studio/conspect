import { describe, expect, it, vi } from 'vitest';
import { screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { CrudPage } from './CrudPage';
import type { CrudPageProps } from './CrudPage';
import { renderWithProviders } from '../test/render';

vi.mock('./queries', () => ({
  useSaveResource: () => ({ mutate: vi.fn(), isPending: false }),
  useDeleteResource: () => ({ mutate: vi.fn(), isPending: false }),
}));
vi.mock('./api', () => ({ getResource: vi.fn(() => Promise.resolve({ record: {} })) }));

const row = { id: 'camera', name: 'Camera' };
const props: CrudPageProps<typeof row, string, 'name'> = {
  kind: 'sources', title: 'Sources', description: 'Inputs', newLabel: 'New source',
  dialogCreateTitle: 'New source', dialogEditTitle: 'Edit source', dialogDescription: 'Input form',
  caption: 'Configured sources', emptyMessage: <>No sources configured.</>, list: [row],
  isPending: false, isError: false, errorMessage: undefined, loadingMessage: 'Loading sources',
  errorPrefix: 'Could not load sources', savedDescription: 'Stored', deletedDescription: 'Removed',
  columns: () => [{ accessorKey: 'name', header: 'Name', cell: (ctx) => ctx.row.original.name }],
  emptyForm: () => '', formFromRecord: () => 'Camera', validate: () => ({}),
  toSaveVars: () => ({ id: 'camera', create: true, input: { name: 'Camera', body: {} } }),
  rowId: (item) => item.id, rowName: (item) => item.name, renderFields: () => <input aria-label="Name" />,
};

describe('CrudPage optional item renderer', () => {
  it('keeps the default table and caption without an override', () => {
    renderWithProviders(<CrudPage {...props} />);
    expect(screen.getByRole('table')).toHaveAccessibleName('Configured sources');
    expect(screen.getByRole('cell', { name: 'Camera' })).toBeInTheDocument();
  });

  it('passes the original rows and CRUD callbacks to the renderer', async () => {
    const renderer = vi.fn((rows: readonly typeof row[], edit: (item: typeof row) => void, remove: (item: typeof row) => void) => (
      <><button onClick={() => { edit(rows[0]!); }}>Card edit</button><button onClick={() => { remove(rows[0]!); }}>Card delete</button></>
    ));
    renderWithProviders(<CrudPage {...props} renderItems={renderer} />);
    expect(renderer.mock.calls[0]?.[0]).toBe(props.list);
    expect(screen.queryByRole('table')).not.toBeInTheDocument();
    await userEvent.click(screen.getByText('Card edit'));
    expect(await screen.findByRole('dialog', { name: 'Edit source' })).toBeInTheDocument();
    await userEvent.click(screen.getByRole('button', { name: 'Cancel' }));
    await userEvent.click(screen.getByText('Card delete'));
    expect(await screen.findByRole('dialog', { name: 'Delete this resource?' })).toBeInTheDocument();
  });

  it.each([
    { isPending: true, isError: false, list: [row], text: 'Loading sources' },
    { isPending: false, isError: true, list: [row], text: 'Could not load sources' },
    { isPending: false, isError: false, list: [], text: 'No sources configured.' },
  ])('retains scaffold state: $text', (state) => {
    const renderer = vi.fn(() => <p>Cards</p>);
    renderWithProviders(<CrudPage {...props} {...state} renderItems={renderer} />);
    expect(screen.getByText(state.text)).toBeInTheDocument();
    expect(renderer).not.toHaveBeenCalled();
  });
});
