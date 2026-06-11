// A small generic TanStack-Table wrapper for the resource list views.
//
// Keeps the Sources/Outputs/Overlays views DRY: pass the typed rows + columns
// and it renders an accessible table with an empty state. Row-level actions
// (edit/delete) live in an `actions` column the caller supplies.
import { useMemo } from 'react';
import type { JSX } from 'react';
import {
  flexRender,
  getCoreRowModel,
  useReactTable,
} from '@tanstack/react-table';
import type { ColumnDef } from '@tanstack/react-table';

import {
  Table,
  TableBody,
  TableCaption,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '../components/ui/table';

/** Props for {@link ResourceTable}. */
export interface ResourceTableProps<T> {
  /** The rows to render. */
  readonly rows: readonly T[];
  /** Column definitions (TanStack Table). */
  readonly columns: ColumnDef<T>[];
  /** Accessible table caption. */
  readonly caption: string;
  /** Message shown when there are no rows. */
  readonly emptyMessage: JSX.Element;
}

/** A read-only, accessible resource list table. */
export function ResourceTable<T>({
  rows,
  columns,
  caption,
  emptyMessage,
}: ResourceTableProps<T>): JSX.Element {
  // TanStack Table REQUIRES a referentially-stable `data` reference. Passing a
  // fresh array every render (`[...rows]`) drove `useReactTable` into an
  // UNBOUNDED re-render loop that OOM-killed the renderer the moment any sibling
  // (e.g. an open create/edit Dialog) re-rendered this page. `rows` is stable
  // across renders (it comes straight from the React Query cache), so memoizing
  // the spread on `[rows]` gives the table a stable identity and the loop is
  // gone. Verified with a headless-browser repro across every resource dialog.
  const data = useMemo<T[]>(() => [...rows], [rows]);
  // eslint-disable-next-line react-hooks/incompatible-library -- TanStack Table instance is a leaf; see LayoutsPage note.
  const table = useReactTable<T>({
    data,
    columns,
    getCoreRowModel: getCoreRowModel(),
  });

  if (rows.length === 0) {
    return <p className="text-sm text-muted-foreground">{emptyMessage}</p>;
  }

  return (
    <Table>
      <TableCaption>{caption}</TableCaption>
      <TableHeader>
        {table.getHeaderGroups().map((group) => (
          <TableRow key={group.id}>
            {group.headers.map((header) => (
              <TableHead key={header.id}>
                {header.isPlaceholder
                  ? null
                  : flexRender(
                      header.column.columnDef.header,
                      header.getContext(),
                    )}
              </TableHead>
            ))}
          </TableRow>
        ))}
      </TableHeader>
      <TableBody>
        {table.getRowModel().rows.map((row) => (
          <TableRow key={row.id}>
            {row.getVisibleCells().map((cell) => (
              <TableCell key={cell.id}>
                {flexRender(cell.column.columnDef.cell, cell.getContext())}
              </TableCell>
            ))}
          </TableRow>
        ))}
      </TableBody>
    </Table>
  );
}
