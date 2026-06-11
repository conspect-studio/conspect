// Salvos — named recalls (a layout + source/tally/UMD rebindings) that an
// operator can arm, take, or cancel against the engine.
//
// Lists `GET /api/v1/salvos`, creates/edits via `PUT /api/v1/salvos/{id}` (a
// replace reads the current ETag first and sends `If-Match`), deletes via
// `DELETE /api/v1/salvos/{id}` (also `If-Match`), and arms/takes/cancels via the
// `POST .../arm|take|cancel` command endpoints (each returns `202 Accepted` + an
// operation id, which is surfaced in a toast; the outcome lands later on the
// realtime stream). The editor here manages the salvo identity, display name, and
// recalled layout; any source/tally/UMD recall lists already on a salvo are
// preserved across an edit but are not edited field-by-field in this view.
import { useMemo, useState } from 'react';
import type { JSX } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import { Pencil, Play, Plus, Square, Trash2, Zap } from 'lucide-react';

import {
  useDeleteSalvo,
  useSalvoOperation,
  useSalvos,
  useSaveSalvo,
} from '../api/salvosQueries';
import type { Salvo, SalvoAction } from '../api/salvosQueries';
import { PageHeader } from '../components/PageHeader';
import { Button } from '../components/ui/button';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '../components/ui/dialog';
import { Input } from '../components/ui/input';
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

/** Count the recall clauses on a salvo (for an at-a-glance column). */
function recallCount(salvo: Salvo): number {
  return (
    (salvo.sources?.length ?? 0) +
    (salvo.tally?.length ?? 0) +
    (salvo.umd?.length ?? 0) +
    (salvo.layout !== null && salvo.layout !== undefined && salvo.layout !== '' ? 1 : 0)
  );
}

interface EditorState {
  /** The salvo being edited, or a fresh draft for a create. */
  readonly draft: Salvo;
  /** Create (`PUT` without `If-Match`) when true; replace otherwise. */
  readonly create: boolean;
}

/** The salvos management page. */
export function SalvosPage(): JSX.Element {
  const { t } = useLingui();
  const salvos = useSalvos();
  const save = useSaveSalvo();
  const remove = useDeleteSalvo();
  const operate = useSalvoOperation();

  const [editor, setEditor] = useState<EditorState | null>(null);
  const [pendingDelete, setPendingDelete] = useState<Salvo | null>(null);

  const data = useMemo<Salvo[]>(() => salvos.data ?? [], [salvos.data]);

  const openCreate = (): void => {
    setEditor({ draft: { id: '', display_name: '', layout: '' }, create: true });
  };

  const openEdit = (salvo: Salvo): void => {
    setEditor({ draft: salvo, create: false });
  };

  const submitEditor = (): void => {
    if (editor === null) {
      return;
    }
    const id = editor.draft.id.trim();
    if (id === '') {
      toast({
        title: t`A salvo id is required`,
        variant: 'destructive',
      });
      return;
    }
    const draft: Salvo = {
      ...editor.draft,
      id,
      display_name:
        editor.draft.display_name === undefined || editor.draft.display_name === ''
          ? null
          : editor.draft.display_name,
      layout:
        editor.draft.layout === undefined || editor.draft.layout === ''
          ? null
          : editor.draft.layout,
    };
    save.mutate(
      { salvo: draft, create: editor.create },
      {
        onSuccess: (): void => {
          toast({ title: editor.create ? t`Salvo created` : t`Salvo saved` });
          setEditor(null);
        },
        onError: (error): void => {
          toast({
            title: t`Could not save salvo`,
            description: error.message,
            variant: 'destructive',
          });
        },
      },
    );
  };

  const confirmDelete = (): void => {
    const target = pendingDelete;
    if (target === null) {
      return;
    }
    remove.mutate(target.id, {
      onSuccess: (): void => {
        toast({ title: t`Salvo deleted` });
      },
      onError: (error): void => {
        toast({
          title: t`Could not delete salvo`,
          description: error.message,
          variant: 'destructive',
        });
      },
    });
    setPendingDelete(null);
  };

  const runAction = (salvo: Salvo, action: SalvoAction): void => {
    operate.mutate(
      { id: salvo.id, action },
      {
        onSuccess: (accepted): void => {
          toast({
            title: t`Command accepted`,
            description: `${t`Operation id`}: ${accepted.operation_id}`,
          });
        },
        onError: (error): void => {
          toast({
            title: t`Command failed`,
            description: error.message,
            variant: 'destructive',
          });
        },
      },
    );
  };

  return (
    <>
      <PageHeader
        title={<Trans>Salvos</Trans>}
        description={
          <Trans>
            Named recalls. Arm stages a salvo, Take applies it, and Cancel drops a
            staged one. Each command is accepted asynchronously; its outcome
            arrives on the realtime stream.
          </Trans>
        }
        actions={
          <Button onClick={openCreate}>
            <Plus aria-hidden="true" />
            <Trans>New salvo</Trans>
          </Button>
        }
      />

      {salvos.isPending ? (
        <p role="status" aria-live="polite" className="text-sm text-muted-foreground">
          <Trans>Loading salvos…</Trans>
        </p>
      ) : salvos.isError ? (
        <p role="alert" className="text-sm text-destructive">
          <Trans>Could not load salvos:</Trans> {salvos.error.message}
        </p>
      ) : data.length === 0 ? (
        <div className="rounded-md border border-dashed p-8 text-center">
          <p className="mb-3 text-sm text-muted-foreground">
            <Trans>No salvos are defined yet.</Trans>
          </p>
          <Button onClick={openCreate}>
            <Plus aria-hidden="true" />
            <Trans>Create your first salvo</Trans>
          </Button>
        </div>
      ) : (
        <Table>
          <TableCaption>{t`All configured salvos.`}</TableCaption>
          <TableHeader>
            <TableRow>
              <TableHead>{t`Name`}</TableHead>
              <TableHead>{t`Identifier`}</TableHead>
              <TableHead>{t`Recalled layout`}</TableHead>
              <TableHead>{t`Clauses`}</TableHead>
              <TableHead>{t`Commands`}</TableHead>
              <TableHead>{t`Definition`}</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {data.map((salvo) => (
              <TableRow key={salvo.id}>
                <TableCell>
                  <span lang="" dir="auto" className="font-medium">
                    {salvo.display_name !== null &&
                    salvo.display_name !== undefined &&
                    salvo.display_name !== ''
                      ? salvo.display_name
                      : salvo.id}
                  </span>
                </TableCell>
                <TableCell>
                  <code className="text-xs text-muted-foreground">{salvo.id}</code>
                </TableCell>
                <TableCell>
                  {salvo.layout !== null &&
                  salvo.layout !== undefined &&
                  salvo.layout !== '' ? (
                    <code className="text-xs">{salvo.layout}</code>
                  ) : (
                    <span className="text-xs text-muted-foreground">
                      <Trans>None</Trans>
                    </span>
                  )}
                </TableCell>
                <TableCell className="tabular-nums">{recallCount(salvo)}</TableCell>
                <TableCell>
                  <div className="flex items-center gap-1">
                    <Button
                      variant="outline"
                      size="sm"
                      disabled={operate.isPending}
                      aria-label={`${t`Arm salvo`}: ${salvo.id}`}
                      onClick={(): void => {
                        runAction(salvo, 'arm');
                      }}
                    >
                      <Zap aria-hidden="true" />
                      <Trans>Arm</Trans>
                    </Button>
                    <Button
                      variant="outline"
                      size="sm"
                      disabled={operate.isPending}
                      aria-label={`${t`Take salvo`}: ${salvo.id}`}
                      onClick={(): void => {
                        runAction(salvo, 'take');
                      }}
                    >
                      <Play aria-hidden="true" />
                      <Trans>Take</Trans>
                    </Button>
                    <Button
                      variant="ghost"
                      size="sm"
                      disabled={operate.isPending}
                      aria-label={`${t`Cancel salvo`}: ${salvo.id}`}
                      onClick={(): void => {
                        runAction(salvo, 'cancel');
                      }}
                    >
                      <Square aria-hidden="true" />
                      <Trans>Cancel</Trans>
                    </Button>
                  </div>
                </TableCell>
                <TableCell>
                  <div className="flex items-center gap-1">
                    <Button
                      variant="outline"
                      size="sm"
                      aria-label={`${t`Edit salvo`}: ${salvo.id}`}
                      onClick={(): void => {
                        openEdit(salvo);
                      }}
                    >
                      <Pencil aria-hidden="true" />
                      <Trans>Edit</Trans>
                    </Button>
                    <Button
                      variant="ghost"
                      size="sm"
                      aria-label={`${t`Delete salvo`}: ${salvo.id}`}
                      onClick={(): void => {
                        setPendingDelete(salvo);
                      }}
                    >
                      <Trash2 aria-hidden="true" />
                      <Trans>Delete</Trans>
                    </Button>
                  </div>
                </TableCell>
              </TableRow>
            ))}
          </TableBody>
        </Table>
      )}

      <Dialog
        open={editor !== null}
        onOpenChange={(open): void => {
          if (!open) {
            setEditor(null);
          }
        }}
      >
        <DialogContent>
          <DialogHeader>
            <DialogTitle>
              {editor?.create ? <Trans>New salvo</Trans> : <Trans>Edit salvo</Trans>}
            </DialogTitle>
            <DialogDescription>
              <Trans>
                The identifier is fixed once created. Source, tally, and UMD recall
                clauses already on this salvo are preserved when you save.
              </Trans>
            </DialogDescription>
          </DialogHeader>
          {editor !== null ? (
            <div className="grid gap-4">
              <div className="grid gap-1.5">
                <Label htmlFor="salvo-id">
                  <Trans>Identifier</Trans>
                </Label>
                <Input
                  id="salvo-id"
                  value={editor.draft.id}
                  disabled={!editor.create}
                  onChange={(e): void => {
                    setEditor({
                      ...editor,
                      draft: { ...editor.draft, id: e.target.value },
                    });
                  }}
                />
              </div>
              <div className="grid gap-1.5">
                <Label htmlFor="salvo-name">
                  <Trans>Display name</Trans>
                </Label>
                <Input
                  id="salvo-name"
                  value={editor.draft.display_name ?? ''}
                  onChange={(e): void => {
                    setEditor({
                      ...editor,
                      draft: { ...editor.draft, display_name: e.target.value },
                    });
                  }}
                />
              </div>
              <div className="grid gap-1.5">
                <Label htmlFor="salvo-layout">
                  <Trans>Recalled layout</Trans>
                </Label>
                <Input
                  id="salvo-layout"
                  value={editor.draft.layout ?? ''}
                  placeholder={t`Layout or preset name (optional)`}
                  onChange={(e): void => {
                    setEditor({
                      ...editor,
                      draft: { ...editor.draft, layout: e.target.value },
                    });
                  }}
                />
              </div>
            </div>
          ) : null}
          <DialogFooter>
            <Button
              variant="outline"
              onClick={(): void => {
                setEditor(null);
              }}
            >
              <Trans>Cancel</Trans>
            </Button>
            <Button onClick={submitEditor} disabled={save.isPending}>
              <Trans>Save</Trans>
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

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
              <Trans>Delete salvo?</Trans>
            </DialogTitle>
            <DialogDescription>
              <Trans>This permanently removes the salvo definition.</Trans>
            </DialogDescription>
          </DialogHeader>
          {pendingDelete !== null ? (
            <p className="text-sm">
              <code>{pendingDelete.id}</code>
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
