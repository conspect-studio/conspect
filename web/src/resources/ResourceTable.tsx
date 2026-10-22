// A small generic TanStack-Table wrapper for the resource list views.
//
// Keeps the Sources/Outputs/Overlays views DRY: pass the typed rows + columns
// and it renders an accessible table with an empty state. Row-level actions
// (edit/delete) live in an `actions` column the caller supplies.
import { useMemo } from 'react';
import type { JSX, ReactNode } from 'react';
import { getCoreRowModel, useReactTable } from '@tanstack/react-table';
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

/**
 * Render a header/cell definition. Function defs are INVOKED here rather than
 * handed to `flexRender`: flexRender mounts a function def as a COMPONENT, so
 * the per-render closures the pages build (columns are rebuilt every render to
 * capture fresh query data) become a new component type each render — React
 * then unmounts/remounts every cell's DOM on any sibling re-render (e.g. a
 * trailing status query resolving). Invoking the def renders the same stable
 * child types instead, so React updates the existing DOM in place. Cell defs
 * here are plain render functions and never call hooks.
 */
function renderDef<Ctx>(
  def: string | ((ctx: Ctx) => ReactNode) | undefined,
  ctx: Ctx,
): ReactNode {
  if (typeof def === 'function') {
    return def(ctx);
  }
  return def;
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
                  : renderDef(header.column.columnDef.header, header.getContext())}
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
                {renderDef(cell.column.columnDef.cell, cell.getContext())}
              </TableCell>
            ))}
          </TableRow>
        ))}
      </TableBody>
    </Table>
  );
}
