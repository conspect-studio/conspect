// The layout editor route: hosts the right editor for the body's placement
// strategy — the free-form LayoutEditor for absolute bodies, the
// GridLayoutEditor for `kind = "grid"` bodies (what every real config uses),
// and an explicit conversion choice for `kind = "preset"` bodies. It loads the
// layout via the typed query cache, maps the opaque `body` to the matching
// view-model, and saves through the CRUD mutation hooks (optimistic + ETag).
import { useMemo, useState } from 'react';
import type { JSX } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import { useNavigate, useParams } from 'react-router-dom';

import { createApiClient } from '../api/client';
import { useLayouts, useSaveLayout } from '../api/queries';
import { applyLayoutCommand, describeApplyError } from '../layout/applyLayout';
import { fromLayoutBody } from '../layout/model';
import type { LayoutModel } from '../layout/model';
import {
  fromGridLayoutBody,
  layoutBodyKind,
  presetBodyToGridModel,
  presetBodyToLayoutModel,
  presetNameOf,
} from '../layout/gridModel';
import type { GridModel } from '../layout/gridModel';
import { LayoutEditor } from '../layout/components/LayoutEditor';
import type { LayoutSavePayload } from '../layout/components/LayoutEditor';
import { GridLayoutEditor } from '../layout/components/GridLayoutEditor';
import { useOverlays, useSources } from '../resources/queries';
import { PageHeader } from '../components/PageHeader';
import { Button } from '../components/ui/button';
import { toast } from '../components/ui/use-toast';

/** The editor page (new or edit). */
export function LayoutEditorPage(): JSX.Element {
  const { t } = useLingui();
  const params = useParams();
  const navigate = useNavigate();
  const layoutId = params.id;
  const isNew = layoutId === undefined;

  const client = useMemo(() => createApiClient(), []);
  const layouts = useLayouts(client);
  const sources = useSources();
  const overlays = useOverlays();
  const save = useSaveLayout({ api: client });

  const existing = useMemo(
    () => layouts.data?.find((layout) => layout.id === layoutId),
    [layouts.data, layoutId],
  );

  // An explicit conversion (grid -> free-form, preset -> grid/free-form)
  // replaces the editor for this visit; nothing persists until saved.
  const [convertedAbsolute, setConvertedAbsolute] = useState<LayoutModel | undefined>(
    undefined,
  );
  const [convertedGrid, setConvertedGrid] = useState<GridModel | undefined>(undefined);

  const bodyKind = useMemo(
    () => (existing === undefined ? undefined : layoutBodyKind(existing.body)),
    [existing],
  );

  // Map the opaque persisted body into the matching editor view-model.
  const initial = useMemo<LayoutModel | undefined>(() => {
    if (isNew || existing === undefined || bodyKind !== 'absolute') {
      return undefined;
    }
    return fromLayoutBody(existing.id, existing.name, existing.body);
  }, [bodyKind, existing, isNew]);

  const initialGrid = useMemo<GridModel | undefined>(() => {
    if (isNew || existing === undefined || bodyKind !== 'grid') {
      return undefined;
    }
    return fromGridLayoutBody(existing.id, existing.name, existing.body);
  }, [bodyKind, existing, isNew]);

  const handleSave = (payload: LayoutSavePayload): void => {
    // The spec requires the id on both create and update
    // (`POST /api/v1/layouts/{id}`). For a fresh draft the editor uses an empty
    // id, so we generate one client-side using a random UUID. Existing layouts
    // carry their persisted id from the URL param.
    const id = payload.id !== '' ? payload.id : crypto.randomUUID();
    save.mutate(
      { id, input: { name: payload.name, body: payload.body } },
      {
        onSuccess: (): void => {
          toast({ title: t`Layout saved` });
          void navigate('/layouts');
        },
        onError: (error): void => {
          toast({
            title: t`Could not save layout`,
            description: error.message,
            variant: 'destructive',
          });
        },
      },
    );
  };

  // Save & Apply: persist first, then submit the LIVE apply-layout command
  // (ADR-W019: the route solves the stored body; a 202 means the layout swaps
  // in at the engine's next frame boundary — no export, no restart). A failed
  // apply leaves the layout saved — the toast says which step failed and why
  // (the 422 problem detail names the reason, e.g. a pinned-canvas mismatch).
  const handleSaveAndApply = (payload: LayoutSavePayload): void => {
    const id = payload.id !== '' ? payload.id : crypto.randomUUID();
    save.mutate(
      { id, input: { name: payload.name, body: payload.body } },
      {
        onSuccess: (): void => {
          applyLayoutCommand(id)
            .then((accepted): void => {
              toast({
                title: t`Layout saved & applied live`,
                description: `${t`Takes effect at the next frame boundary.`} ${t`Operation id`}: ${accepted.operation_id}`,
              });
              void navigate('/layouts');
            })
            .catch((error: unknown): void => {
              toast({
                title: t`Layout saved, but apply failed`,
                description: describeApplyError(error),
                variant: 'destructive',
              });
            });
        },
        onError: (error): void => {
          toast({
            title: t`Could not save layout`,
            description: error.message,
            variant: 'destructive',
          });
        },
      },
    );
  };

  if (!isNew && layouts.isPending) {
    return (
      <>
        <PageHeader title={<Trans>Edit layout</Trans>} />
        <p role="status" aria-live="polite" className="text-sm text-muted-foreground">
          <Trans>Loading layout…</Trans>
        </p>
      </>
    );
  }

  if (!isNew && existing === undefined) {
    return (
      <>
        <PageHeader title={<Trans>Edit layout</Trans>} />
        <p role="alert" className="text-sm text-destructive">
          <Trans>That layout no longer exists.</Trans>
        </p>
      </>
    );
  }

  // An explicit conversion replaces the loaded editor for this visit.
  if (convertedGrid !== undefined) {
    return (
      <>
        <PageHeader
          title={<Trans>Edit layout</Trans>}
          description={
            <Trans>
              Converted to a grid (not yet saved). Save to keep the grid form.
            </Trans>
          }
        />
        <GridLayoutEditor
          initial={convertedGrid}
          sources={sources.data ?? []}
          onSave={handleSave}
          onSaveAndApply={handleSaveAndApply}
          onConvertToFreeForm={setConvertedAbsolute}
          isSaving={save.isPending}
        />
      </>
    );
  }
  if (convertedAbsolute !== undefined) {
    return (
      <>
        <PageHeader
          title={<Trans>Edit layout</Trans>}
          description={
            <Trans>
              Converted to free-form (not yet saved). Save to keep the absolute
              placement.
            </Trans>
          }
        />
        <LayoutEditor
          initial={convertedAbsolute}
          sources={sources.data ?? []}
          overlays={overlays.data ?? []}
          onSave={handleSave}
          onSaveAndApply={handleSaveAndApply}
          isSaving={save.isPending}
        />
      </>
    );
  }

  // A grid body gets the grid editor.
  if (!isNew && existing !== undefined && bodyKind === 'grid') {
    if (initialGrid === undefined) {
      return (
        <>
          <PageHeader title={<Trans>Edit layout</Trans>} />
          <p role="alert" className="text-sm text-destructive">
            <Trans>
              This grid layout's body could not be parsed (its track or area
              lists are malformed). Fix the stored document via the API.
            </Trans>
          </p>
        </>
      );
    }
    return (
      <>
        <PageHeader
          title={<Trans>Edit layout</Trans>}
          description={
            <span lang="" dir="auto">
              {existing.name}
            </span>
          }
        />
        <GridLayoutEditor
          initial={initialGrid}
          sources={sources.data ?? []}
          onSave={handleSave}
          onSaveAndApply={handleSaveAndApply}
          onConvertToFreeForm={setConvertedAbsolute}
          isSaving={save.isPending}
        />
      </>
    );
  }

  // A preset body offers an explicit, never-silent conversion choice.
  if (!isNew && existing !== undefined && bodyKind === 'preset') {
    const preset = presetNameOf(existing.body) ?? '';
    const toGrid = (): void => {
      const grid = presetBodyToGridModel(existing.id, existing.name, existing.body);
      if (grid === undefined) {
        toast({
          title: t`Cannot convert this preset to a grid`,
          description: t`Its tiles overlap (picture-in-picture), which a grid cannot express. Convert to free-form instead.`,
          variant: 'destructive',
        });
        return;
      }
      setConvertedGrid(grid);
    };
    const toFreeForm = (): void => {
      const absolute = presetBodyToLayoutModel(existing.id, existing.name, existing.body);
      if (absolute === undefined) {
        toast({
          title: t`Cannot convert this preset`,
          description: t`The stored preset body could not be expanded.`,
          variant: 'destructive',
        });
        return;
      }
      setConvertedAbsolute(absolute);
    };
    return (
      <>
        <PageHeader
          title={<Trans>Edit layout</Trans>}
          description={
            <span lang="" dir="auto">
              {existing.name}
            </span>
          }
        />
        <div className="flex flex-col gap-3 rounded-md border p-4">
          <p className="text-sm">
            <Trans>
              This layout uses the factory preset "{preset}". To edit it, convert
              it to an editable form first — nothing changes until you save.
            </Trans>
          </p>
          <div className="flex gap-3">
            <Button type="button" data-testid="preset-to-grid" onClick={toGrid}>
              <Trans>Convert to grid</Trans>
            </Button>
            <Button
              type="button"
              variant="outline"
              data-testid="preset-to-freeform"
              onClick={toFreeForm}
            >
              <Trans>Convert to free-form</Trans>
            </Button>
          </div>
        </div>
      </>
    );
  }

  // An unknown layout kind (or an absolute body that fails to parse).
  if (!isNew && existing !== undefined && (bodyKind === undefined || initial === undefined)) {
    return (
      <>
        <PageHeader
          title={<Trans>Edit layout</Trans>}
          description={
            <span lang="" dir="auto">
              {existing.name}
            </span>
          }
        />
        <p role="alert" className="text-sm text-destructive">
          <Trans>
            This layout's body could not be parsed into an editable form. Fix
            the stored document via the API.
          </Trans>
        </p>
      </>
    );
  }

  return (
    <>
      <PageHeader
        title={isNew ? <Trans>New layout</Trans> : <Trans>Edit layout</Trans>}
        description={
          <Trans>
            Compose the output multiview. The Canvas tab is drag-and-drop; the Cells
            tab is the fully keyboard-operable equivalent.
          </Trans>
        }
      />
      <LayoutEditor
        {...(initial !== undefined ? { initial } : {})}
        sources={sources.data ?? []}
        overlays={overlays.data ?? []}
        onSave={handleSave}
        onSaveAndApply={handleSaveAndApply}
        isSaving={save.isPending}
      />
    </>
  );
}
