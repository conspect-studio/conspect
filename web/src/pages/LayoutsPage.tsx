// Layouts — a TanStack-Table list of persisted layouts with full CRUD:
// create (link to the editor), edit (link), and delete (optimistic, with a
// confirmation dialog and ETag-aware concurrency). The list reads through the
// typed client; mutations go through the CRUD hooks.
import { useMemo, useState } from 'react';
import type { JSX } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import { Link } from 'react-router-dom';
import {
  flexRender,
  getCoreRowModel,
  useReactTable,
} from '@tanstack/react-table';
import type { ColumnDef } from '@tanstack/react-table';
import { Pencil, Plus, Send, Trash2 } from 'lucide-react';

import { createApiClient } from '../api/client';
import { useDeleteLayout, useLayouts } from '../api/queries';
import type { Layout } from '../api/queries';
import { applyLayoutCommand, describeApplyError } from '../layout/applyLayout';
import { fromLayoutBody } from '../layout/model';
import { HelpLink } from '../components/HelpLink';
import { PageHeader } from '../components/PageHeader';
import { Badge } from '../components/ui/badge';
import { Button } from '../components/ui/button';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '../components/ui/dialog';
import {
  Table,
  TableBody,
  TableCaption,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '../components/ui/table';
import { toast } from '../components/ui/use-toast';

/** Count the cells in a layout body (0 when not the editable kind). */
function cellCount(layout: Layout): number {
  const model = fromLayoutBody(layout.id, layout.name, layout.body);
  return model?.cells.length ?? 0;
}

function useColumns(
  onRequestDelete: (layout: Layout) => void,
  onApply: (layout: Layout) => void,
  applying: boolean,
): ColumnDef<Layout>[] {
  const { t } = useLingui();
  return useMemo<ColumnDef<Layout>[]>(
    () => [
      {
        accessorKey: 'name',
        header: t`Name`,
        cell: (ctx): JSX.Element => (
          <span lang="" dir="auto" className="font-medium">
            {ctx.row.original.name}
          </span>
        ),
      },
      {
        accessorKey: 'id',
        header: t`Identifier`,
        cell: (ctx): JSX.Element => (
          <code className="text-xs text-muted-foreground">
            {ctx.row.original.id}
          </code>
        ),
      },
      {
        id: 'cells',
        header: t`Cells`,
        cell: (ctx): JSX.Element => (
          <span className="tabular-nums">{cellCount(ctx.row.original)}</span>
        ),
      },
      {
        id: 'actions',
        header: t`Actions`,
        cell: (ctx): JSX.Element => {
          const layout = ctx.row.original;
          return (
            <div className="flex items-center gap-2">
              <Button
                variant="outline"
                size="sm"
                disabled={applying}
                aria-label={`${t`Apply layout to engine`}: ${layout.name}`}
                onClick={(): void => {
                  onApply(layout);
                }}
              >
                <Send aria-hidden="true" />
                <Trans>Apply</Trans>
              </Button>
              <Button asChild variant="outline" size="sm">
                <Link to={`/layouts/${encodeURIComponent(layout.id)}`}>
                  <Pencil aria-hidden="true" />
                  <Trans>Edit</Trans>
                </Link>
              </Button>
              <Button
                variant="ghost"
                size="sm"
                aria-label={`${t`Delete layout`}: ${layout.name}`}
                onClick={(): void => {
                  onRequestDelete(layout);
                }}
              >
                <Trash2 aria-hidden="true" />
                <Trans>Delete</Trans>
              </Button>
            </div>
          );
        },
      },
    ],
    [t, onRequestDelete, onApply, applying],
  );
}

/** The layouts management page. */
export function LayoutsPage(): JSX.Element {
  const { t } = useLingui();
  const client = useMemo(() => createApiClient(), []);
  const layouts = useLayouts(client);
  const deleteLayout = useDeleteLayout({ api: client });
  const [pendingDelete, setPendingDelete] = useState<Layout | null>(null);
  const [applying, setApplying] = useState(false);

  // Apply is a LIVE action (ADR-W019): the stored layout is resolved + solved
  // at the route and swaps in at the next frame boundary — no export, no
  // restart. A 422 (unknown id / unsolvable body / pinned-canvas mismatch)
  // carries the reason in the problem detail.
  const applyLayout = (layout: Layout): void => {
    setApplying(true);
    applyLayoutCommand(layout.id)
      .then((accepted): void => {
        toast({
          title: t`Layout applied live`,
          description: `${t`Takes effect at the next frame boundary.`} ${t`Operation id`}: ${accepted.operation_id}`,
        });
      })
      .catch((error: unknown): void => {
        toast({
          title: t`Could not apply layout`,
          description: describeApplyError(error),
          variant: 'destructive',
        });
      })
      .finally((): void => {
        setApplying(false);
      });
  };

  const columns = useColumns(setPendingDelete, applyLayout, applying);
  const data = useMemo<Layout[]>(() => layouts.data ?? [], [layouts.data]);

  // eslint-disable-next-line react-hooks/incompatible-library -- TanStack Table instance is a leaf; see prior note.
  const table = useReactTable<Layout>({
    data,
    columns,
    getCoreRowModel: getCoreRowModel(),
  });

  const confirmDelete = (): void => {
    const target = pendingDelete;
    if (target === null) {
      return;
    }
    deleteLayout.mutate(target.id, {
      onSuccess: (): void => {
        toast({ title: t`Layout deleted` });
      },
      onError: (error): void => {
        toast({
          title: t`Could not delete layout`,
          description: error.message,
          variant: 'destructive',
        });
      },
    });
    setPendingDelete(null);
  };

  return (
    <>
      <PageHeader
        title={<Trans>Layouts</Trans>}
        description={
          <span className="inline-flex flex-wrap items-center gap-x-3 gap-y-1">
            <Trans>
              Multiview layouts. The accessible editor offers a form-based path
              equivalent to the drag-and-drop canvas. Apply is live on the
              running engine.
            </Trans>
            <HelpLink to="/help/features#layouts" label={t`About layouts`} />
          </span>
        }
        actions={
          <Button asChild>
            <Link to="/layouts/new">
              <Plus aria-hidden="true" />
              <Trans>New layout</Trans>
            </Link>
          </Button>
        }
      />

      {layouts.isPending ? (
        <p role="status" aria-live="polite" className="text-sm text-muted-foreground">
          <Trans>Loading layouts…</Trans>
        </p>
      ) : layouts.isError ? (
        <p role="alert" className="text-sm text-destructive">
          <Trans>Could not load layouts:</Trans> {layouts.error.message}
        </p>
      ) : data.length === 0 ? (
        <div className="rounded-md border border-dashed p-8 text-center">
          <p className="mb-3 text-sm text-muted-foreground">
            <Trans>No layouts are defined yet.</Trans>
          </p>
          <Button asChild>
            <Link to="/layouts/new">
              <Plus aria-hidden="true" />
              <Trans>Create your first layout</Trans>
            </Link>
          </Button>
        </div>
      ) : (
        <Table>
          <TableCaption>{t`All configured layouts.`}</TableCaption>
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
      )}

      <p className="mt-6">
        <Badge variant="outline">
          <Trans>Accessible editing path: Cells form + Inspector in the editor</Trans>
        </Badge>
      </p>

      <Dialog
        open={pendingDelete !== null}
        onOpenChange={(open): void => {
          if (!open) {
            setPendingDelete(null);
          }
        }}
      >
        <DialogContent>
          <DialogHeader>
            <DialogTitle>
              <Trans>Delete layout?</Trans>
            </DialogTitle>
            <DialogDescription>
              <Trans>
                This permanently removes the layout. Running outputs are not
                affected until a new layout is applied.
              </Trans>
            </DialogDescription>
          </DialogHeader>
          {pendingDelete !== null ? (
            <p className="text-sm">
              <span lang="" dir="auto" className="font-medium">
                {pendingDelete.name}
              </span>
            </p>
          ) : null}
          <DialogFooter>
            <Button
              variant="outline"
              onClick={(): void => {
                setPendingDelete(null);
              }}
            >
              <Trans>Cancel</Trans>
            </Button>
            <Button variant="destructive" onClick={confirmDelete}>
              <Trans>Delete</Trans>
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </>
  );
}
