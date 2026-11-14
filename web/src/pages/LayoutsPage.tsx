// Layouts — a TanStack-Table list of persisted layouts with full CRUD:
// create (link to the editor), edit (link), and delete (optimistic, with a
// confirmation dialog and ETag-aware concurrency). The list reads through the
// typed client; mutations go through the CRUD hooks.
import { useEffect, useMemo, useState } from 'react';
import type { JSX } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import { Link } from 'react-router-dom';
import { useQuery } from '@tanstack/react-query';
import {
  flexRender,
  getCoreRowModel,
  useReactTable,
} from '@tanstack/react-table';
import type { ColumnDef } from '@tanstack/react-table';
import { Layers, MonitorPlay, Pencil, Plus, Send, Trash2 } from 'lucide-react';

import { createApiClient } from '../api/client';
import type { MultiviewApiClient } from '../api/client';
import { fetchLayoutForEdit, useDeleteLayout, useLayouts, useSaveLayout } from '../api/queries';
import type { ApiError, Layout } from '../api/queries';
import { getSwitcher } from '../api/switcher';
import {
  applyLayoutCommand,
  blockedBySwitcher,
  describeApplyError,
  isSwitcherRefusal,
} from '../layout/applyLayout';
import { isRecord } from '../layout/cellProps';
import { fromLayoutBody } from '../layout/model';
import {
  DEFAULT_LAYOUT_OVERLAYS,
  overlaysFromBody,
  summarizeOverlays,
  withOverlays,
} from '../layout/overlays';
import type { LayoutOverlays } from '../layout/overlays';
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
import { Label } from '../components/ui/label';
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

/** A human-readable message for an API failure: the RFC 9457 `detail` when
 * present (the specific reason — e.g. the `routed-layout` 403's explanation
 * of which outputs/sources are missing authorization), else `error.message`. */
function describeApiError(error: ApiError): string {
  return error.detail ?? error.message;
}

/** Count the cells in a layout body (0 when not the editable kind). */
function cellCount(layout: Layout): number {
  const model = fromLayoutBody(layout.id, layout.name, layout.body);
  return model?.cells.length ?? 0;
}

/** The authored cell count of any placement kind (grid or absolute). */
function authoredCellCount(layout: Layout): number {
  const body: unknown = layout.body;
  if (typeof body !== 'object' || body === null) return 0;
  const cells = (body as { cells?: unknown }).cells;
  return Array.isArray(cells) ? cells.length : 0;
}

function useColumns(
  onRequestDelete: (layout: Layout) => void,
  onApply: (layout: Layout) => void,
  onEditLayers: (layout: Layout) => void,
  applying: boolean,
  switcherOn: boolean,
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
        id: 'layers',
        header: t`Layers`,
        cell: (ctx): JSX.Element => {
          const summary = summarizeOverlays(overlaysFromBody(ctx.row.original.body));
          return (
            <span className="text-sm">
              {summary.kind === 'all' ? (
                <Trans>All</Trans>
              ) : summary.kind === 'clean' ? (
                <Trans>Clean</Trans>
              ) : (
                <Trans>
                  {summary.on} of {summary.total}
                </Trans>
              )}
            </span>
          );
        },
      },
      {
        id: 'actions',
        header: t`Actions`,
        cell: (ctx): JSX.Element => {
          const layout = ctx.row.original;
          // #317: known in advance — with the switcher on, a multi-cell layout
          // cannot be the Program canvas. Disabled WITH the reason in text.
          const blocked = blockedBySwitcher(switcherOn, authoredCellCount(layout));
          const reasonId = `apply-blocked-${layout.id}`;
          return (
            <div className="flex items-center gap-2">
              <Button
                variant="outline"
                size="sm"
                disabled={applying || blocked}
                aria-label={`${t`Apply layout to engine`}: ${layout.name}`}
                aria-describedby={blocked ? reasonId : undefined}
                data-testid={blocked ? 'layout-apply-blocked-by-switcher' : undefined}
                onClick={(): void => {
                  onApply(layout);
                }}
              >
                <Send aria-hidden="true" />
                <Trans>Apply</Trans>
              </Button>
              {blocked ? (
                <span
                  id={reasonId}
                  className="text-xs text-muted-foreground"
                  data-testid="layout-apply-blocked-reason"
                >
                  <Trans>
                    The switcher is on: a multi-cell layout cannot be the Program canvas.
                  </Trans>
                </span>
              ) : null}
              <Button asChild variant="outline" size="sm">
                <Link to={`/layouts/${encodeURIComponent(layout.id)}`}>
                  <Pencil aria-hidden="true" />
                  <Trans>Edit</Trans>
                </Link>
              </Button>
              <Button
                variant="outline"
                size="sm"
                aria-label={`${t`Layers`}: ${layout.name}`}
                onClick={(): void => {
                  onEditLayers(layout);
                }}
              >
                <Layers aria-hidden="true" />
                <Trans>Layers</Trans>
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
    [t, onRequestDelete, onApply, onEditLayers, applying, switcherOn],
  );
}

/** One labelled overlay-layer checkbox row, with short help text. */
function LayerRow({
  id,
  label,
  help,
  checked,
  onChange,
  disabled,
}: {
  readonly id: string;
  readonly label: JSX.Element;
  readonly help: JSX.Element;
  readonly checked: boolean;
  readonly onChange: (value: boolean) => void;
  readonly disabled: boolean;
}): JSX.Element {
  return (
    <div className="flex items-start gap-2">
      <input
        id={id}
        type="checkbox"
        className="mt-1 size-4"
        checked={checked}
        disabled={disabled}
        onChange={(e): void => {
          onChange(e.target.checked);
        }}
      />
      <div className="grid gap-0.5">
        <Label htmlFor={id}>{label}</Label>
        <span className="text-xs text-muted-foreground">{help}</span>
      </div>
    </div>
  );
}

/** A fresh same-resource read this dialog session owns: the body it edits
 * and the EXACT `ETag` it must send back as `If-Match` on save. */
interface LayersEditTarget {
  readonly id: string;
  readonly name: string;
  readonly body: unknown;
  readonly etag: string;
}

/**
 * The "Layers" dialog's content for ONE open session. Mounted fresh (by a
 * `key` on the session id in the parent) every time the dialog opens, so
 * Cancel/Escape never leaves a stale draft behind and a reopen always starts
 * from the layout's true current state — never a discarded or outdated one.
 *
 * On open it reads the layout fresh via `GET /api/v1/layouts/{id}` (never the
 * list-derived row, whose body/ETag can already be behind a concurrent
 * editor save) and keeps that read's `ETag` to send back as `If-Match`. A
 * `412` means someone else saved first: nothing is retried with the stale
 * body — the operator is told to reopen, and the list is refetched.
 */
function LayersDialogSession({
  client,
  layoutId,
  layoutName,
  onClose,
  onSaved,
}: {
  readonly client: MultiviewApiClient;
  readonly layoutId: string;
  readonly layoutName: string;
  readonly onClose: () => void;
  readonly onSaved: () => void;
}): JSX.Element {
  const { t } = useLingui();
  const saveLayout = useSaveLayout({ api: client });
  const [target, setTarget] = useState<LayersEditTarget | 'loading' | 'error'>('loading');
  const [draft, setDraft] = useState<LayoutOverlays>(DEFAULT_LAYOUT_OVERLAYS);
  const [conflict, setConflict] = useState(false);

  useEffect(() => {
    let cancelled = false;
    fetchLayoutForEdit(client, layoutId)
      .then((result): void => {
        if (cancelled) {
          return;
        }
        // Fail closed: without this read's own ETag a save could not be
        // conditional on it, so a concurrent edit could be overwritten.
        const etag = result.etag?.trim();
        if (etag === undefined || etag === '') {
          setTarget('error');
          return;
        }
        setTarget({
          id: result.layout.id,
          name: result.layout.name,
          body: result.layout.body,
          etag,
        });
        setDraft(overlaysFromBody(result.layout.body));
      })
      .catch((): void => {
        if (!cancelled) {
          setTarget('error');
        }
      });
    return (): void => {
      cancelled = true;
    };
    // Runs exactly once per mounted session (the parent remounts this
    // component, via `key`, for every open — see LayersDialog below).
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const setLayer = (key: keyof LayoutOverlays, value: boolean): void => {
    setDraft((prev) => ({ ...prev, [key]: value }));
  };

  const allOff: LayoutOverlays = {
    labels: false,
    meters: false,
    flags: false,
    tally: false,
    clock: false,
    safe_area: false,
  };

  const loaded = typeof target === 'object';
  const saving = saveLayout.isPending;

  const save = (): void => {
    if (!loaded) {
      return;
    }
    const body = isRecord(target.body) ? target.body : {};
    saveLayout.mutate(
      {
        id: target.id,
        create: false,
        ifMatch: target.etag,
        input: { name: target.name, body: withOverlays(body, draft) },
      },
      {
        onSuccess: (): void => {
          toast({ title: t`Layers saved` });
          onSaved();
        },
        onError: (error): void => {
          if (error.status === 412) {
            setConflict(true);
            return;
          }
          toast({
            title: error.status === 403
              ? t`Could not save layers: this layout is routed`
              : t`Could not save layers`,
            description: describeApiError(error),
            variant: 'destructive',
          });
        },
      },
    );
  };

  return (
    <DialogContent>
      <DialogHeader>
        <DialogTitle>
          <Trans>Layers</Trans>
        </DialogTitle>
        <DialogDescription>
          <Trans>
            Which multiview layers {layoutName} bakes when a routed output
            shows it.
          </Trans>
        </DialogDescription>
      </DialogHeader>

      {target === 'loading' ? (
        <p role="status" aria-live="polite" className="text-sm text-muted-foreground">
          <Trans>Loading the current layers…</Trans>
        </p>
      ) : target === 'error' ? (
        <p role="alert" className="text-sm text-destructive">
          <Trans>Could not load this layout's current layers. Close and try again.</Trans>
        </p>
      ) : (
        <>
          {conflict ? (
            <p role="alert" className="text-sm text-destructive" data-testid="layers-conflict">
              <Trans>
                This layout changed elsewhere since it was opened here. Reopen
                Layers to edit the current version — nothing was overwritten.
              </Trans>
            </p>
          ) : null}

          <div className="grid gap-3">
            <LayerRow
              id="layer-labels"
              label={<Trans>Source names</Trans>}
              help={<Trans>Caption each cell with its bound source's name.</Trans>}
              checked={draft.labels}
              disabled={saving || conflict}
              onChange={(v): void => {
                setLayer('labels', v);
              }}
            />
            <LayerRow
              id="layer-meters"
              label={<Trans>Audio meters</Trans>}
              help={<Trans>Per-cell audio level meters.</Trans>}
              checked={draft.meters}
              disabled={saving || conflict}
              onChange={(v): void => {
                setLayer('meters', v);
              }}
            />
            <LayerRow
              id="layer-flags"
              label={<Trans>State &amp; fault flags</Trans>}
              help={
                <Trans>
                  LIVE / STALE / NO SIGNAL and BLACK / FROZEN / NO AUDIO badges.
                </Trans>
              }
              checked={draft.flags}
              disabled={saving || conflict}
              onChange={(v): void => {
                setLayer('flags', v);
              }}
            />
            <LayerRow
              id="layer-tally"
              label={<Trans>Tally borders</Trans>}
              help={
                <Trans>Red = on Program, green = on Preview.</Trans>
              }
              checked={draft.tally}
              disabled={saving || conflict}
              onChange={(v): void => {
                setLayer('tally', v);
              }}
            />
            <LayerRow
              id="layer-clock"
              label={<Trans>Clock</Trans>}
              help={<Trans>The on-screen clock.</Trans>}
              checked={draft.clock}
              disabled={saving || conflict}
              onChange={(v): void => {
                setLayer('clock', v);
              }}
            />
            <LayerRow
              id="layer-safe-area"
              label={<Trans>Safe-area markers</Trans>}
              help={<Trans>Action/graphics safe-area guide lines.</Trans>}
              checked={draft.safe_area}
              disabled={saving || conflict}
              onChange={(v): void => {
                setLayer('safe_area', v);
              }}
            />
          </div>

          <div className="flex flex-wrap gap-2">
            <Button
              type="button"
              variant="outline"
              size="sm"
              disabled={saving || conflict}
              onClick={(): void => {
                setDraft(allOff);
              }}
            >
              <Trans>Clean (no overlays)</Trans>
            </Button>
            <Button
              type="button"
              variant="outline"
              size="sm"
              disabled={saving || conflict}
              onClick={(): void => {
                setDraft(DEFAULT_LAYOUT_OVERLAYS);
              }}
            >
              <Trans>Reset to defaults</Trans>
            </Button>
          </div>
        </>
      )}

      <DialogFooter>
        <Button variant="outline" disabled={saving} onClick={onClose}>
          <Trans>Cancel</Trans>
        </Button>
        <Button disabled={saving || !loaded || conflict} onClick={save}>
          <Trans>Save</Trans>
        </Button>
      </DialogFooter>
    </DialogContent>
  );
}

/**
 * The "Layers" dialog: which multiview layers a routed layout bakes
 * (labels/meters/flags/tally/clock/safe_area — the engine `overlays`
 * sub-document, PR-A). Each open is a fresh {@link LayersDialogSession},
 * remounted via `key` on an incrementing session id so a cancelled/closed
 * draft never leaks into the next open of the same layout, and the session
 * always starts from a just-read, never a stale or cached, body + ETag.
 */
function LayersDialog({
  client,
  layout,
  onClose,
}: {
  readonly client: MultiviewApiClient;
  readonly layout: Layout | null;
  readonly onClose: () => void;
}): JSX.Element {
  // Bump on EVERY closed→open transition (not just a changed id) so React
  // remounts LayersDialogSession from scratch — this is what makes
  // Cancel/Escape-then-reopen of the SAME layout (and reopen after a
  // persisted change) start clean, reading fresh, rather than resuming a
  // discarded draft or an out-of-date one. This is the standard "adjust
  // state during render" pattern (no effect needed): React re-renders once
  // more with the bumped session before committing, so there is no
  // flicker and no extra fetch.
  const isOpen = layout !== null;
  const [session, setSession] = useState(0);
  const [wasOpen, setWasOpen] = useState(false);
  if (isOpen !== wasOpen) {
    setWasOpen(isOpen);
    if (isOpen) {
      setSession((s) => s + 1);
    }
  }

  return (
    <Dialog
      open={layout !== null}
      onOpenChange={(open): void => {
        if (!open) {
          onClose();
        }
      }}
    >
      {layout !== null ? (
        <LayersDialogSession
          key={`${layout.id}-${String(session)}`}
          client={client}
          layoutId={layout.id}
          layoutName={layout.name}
          onClose={onClose}
          onSaved={onClose}
        />
      ) : null}
    </Dialog>
  );
}

/** The layouts management page. */
export function LayoutsPage(): JSX.Element {
  const { t } = useLingui();
  const client = useMemo(() => createApiClient(), []);
  const layouts = useLayouts(client);
  const deleteLayout = useDeleteLayout({ api: client });
  const [pendingDelete, setPendingDelete] = useState<Layout | null>(null);
  const [editingLayers, setEditingLayers] = useState<Layout | null>(null);
  const [applying, setApplying] = useState(false);
  // The switcher is "on" when the server serves a live M/E state; a 409 (no
  // switcher runtime) or any failure leaves Apply enabled and the server
  // authoritative.
  const switcher = useQuery({
    queryKey: ['switcher', 'me1'],
    queryFn: () => getSwitcher(),
    retry: false,
  });
  const switcherOn = switcher.data !== undefined;

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
          title: isSwitcherRefusal(error)
            ? t`Layout not applied: the switcher is on`
            : t`Could not apply layout`,
          description: isSwitcherRefusal(error)
            ? `${describeApplyError(error)} ${t`Nothing was changed; the current layout keeps running.`}`
            : describeApplyError(error),
          variant: 'destructive',
        });
      })
      .finally((): void => {
        setApplying(false);
      });
  };

  const columns = useColumns(setPendingDelete, applyLayout, setEditingLayers, applying, switcherOn);
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
              Sources → Layouts → Outputs. The accessible editor offers a
              form-based path equivalent to the drag-and-drop canvas. Save keeps
              a layout; apply asks the running engine to use it now. Saved
              layouts are not separate live renderers.
            </Trans>
            <HelpLink to="/help/features#layouts" label={t`About layouts`} />
          </span>
        }
        actions={
          <div className="flex flex-wrap gap-2">
            <Button asChild variant="outline">
              <Link to="/monitoring">
                <MonitorPlay aria-hidden="true" />
                <Trans>View current engine program</Trans>
              </Link>
            </Button>
            <Button asChild>
            <Link to="/layouts/new">
              <Plus aria-hidden="true" />
              <Trans>New layout</Trans>
            </Link>
            </Button>
          </div>
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

      <LayersDialog
        client={client}
        layout={editingLayers}
        onClose={(): void => {
          setEditingLayers(null);
        }}
      />
    </>
  );
}
