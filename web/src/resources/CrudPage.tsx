// The generic CRUD page scaffold shared by Sources / Outputs / Overlays.
//
// Each page lists its resource (read through the projected query hooks) and
// offers create / edit / delete: a labelled, accessible Dialog form for create
// and edit (edit loads the current record via GET, prefills, and PUTs with
// `If-Match`), and a confirmation Dialog for delete — all with success/error
// toasts. Validation is per-field: the page's pure validator returns machine
// codes which render INLINE at each field (aria-describedby), plus a summary
// alert for screen-reader context. Status is conveyed by text + glyph, never
// colour alone. Visible strings are i18n'd via Lingui.
import { useState } from 'react';
import type { JSX, ReactNode } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import { Pencil, Plus, Trash2 } from 'lucide-react';
import type { ColumnDef } from '@tanstack/react-table';

import { getResource } from './api';
import { useDeleteResource, useSaveResource } from './queries';
import type { ResourceContext, SaveResourceVars } from './queries';
import type { ResourceKind, ResourceRecord } from './types';
import type { FieldErrors } from './forms';
import { ResourceTable } from './ResourceTable';
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
import { toast } from '../components/ui/use-toast';

/** A row's display name, rendered bidi-safe. */
export function NameCell({ value }: { readonly value: string }): JSX.Element {
  return (
    <span lang="" dir="auto" className="font-medium">
      {value}
    </span>
  );
}

/** A row's kind tag. */
export function KindCell({ value }: { readonly value: string }): JSX.Element {
  return <Badge variant="outline">{value}</Badge>;
}

/** The per-row Edit/Delete action buttons. */
export function RowActions<View>({
  row,
  name,
  editLabel,
  deleteLabel,
  editDisabledReason,
  onEdit,
  onDelete,
}: {
  readonly row: View;
  readonly name: string;
  readonly editLabel: string;
  readonly deleteLabel: string;
  /**
   * When set, Edit is refused with this explanation (e.g. an unknown kind the
   * typed forms cannot round-trip). `aria-disabled` (not `disabled`) keeps the
   * control focusable so the reason is discoverable by keyboard/SR users; the
   * click is a guarded no-op and the stored document stays as authored.
   */
  readonly editDisabledReason?: string | undefined;
  readonly onEdit: (row: View) => void;
  readonly onDelete: (row: View) => void;
}): JSX.Element {
  const editRefused = editDisabledReason !== undefined && editDisabledReason !== '';
  return (
    <div className="flex items-center gap-2">
      <Button
        variant="outline"
        size="sm"
        data-testid="row-edit"
        aria-label={
          editRefused
            ? `${editLabel}: ${name} — ${editDisabledReason}`
            : `${editLabel}: ${name}`
        }
        aria-disabled={editRefused}
        {...(editRefused ? { title: editDisabledReason } : {})}
        {...(editRefused ? { className: 'cursor-not-allowed opacity-50' } : {})}
        onClick={(): void => {
          if (editRefused) {
            return;
          }
          onEdit(row);
        }}
      >
        <Pencil aria-hidden="true" />
        <Trans>Edit</Trans>
      </Button>
      <Button
        variant="ghost"
        size="sm"
        data-testid="row-delete"
        aria-label={`${deleteLabel}: ${name}`}
        onClick={(): void => {
          onDelete(row);
        }}
      >
        <Trash2 aria-hidden="true" />
        <Trans>Delete</Trans>
      </Button>
    </div>
  );
}

/** Props for {@link CrudPage}. */
export interface CrudPageProps<View, Form, Field extends string> {
  readonly kind: ResourceKind;
  readonly title: ReactNode;
  readonly description: ReactNode;
  readonly newLabel: string;
  readonly dialogCreateTitle: string;
  readonly dialogEditTitle: string;
  readonly dialogDescription: string;
  readonly caption: string;
  readonly emptyMessage: JSX.Element;
  readonly list: readonly View[];
  readonly isPending: boolean;
  readonly isError: boolean;
  readonly errorMessage: string | undefined;
  readonly loadingMessage: ReactNode;
  readonly errorPrefix: ReactNode;
  /** Extra header actions rendered before the New button (e.g. export). */
  readonly headerExtras?: ReactNode;
  /** An informational callout rendered above the table (apply semantics). */
  readonly callout?: ReactNode;
  /** The success-toast description (the honest apply-semantics line). */
  readonly savedDescription: string;
  readonly deletedDescription: string;
  readonly columns: (
    onEdit: (row: View) => void,
    onDelete: (row: View) => void,
  ) => ColumnDef<View>[];
  /** A fresh, empty form for a create. */
  readonly emptyForm: () => Form;
  /**
   * Project a server record onto the editable form (for edit prefill).
   * `undefined` = the record's kind has no typed form (the row's Edit action
   * is already refused; this is the defensive backstop).
   */
  readonly formFromRecord: (record: ResourceRecord) => Form | undefined;
  /** Validate the form, returning per-field machine codes (empty = valid). */
  readonly validate: (form: Form, creating: boolean) => FieldErrors<Field>;
  /** Build the save vars (id + payload) from a valid form. */
  readonly toSaveVars: (form: Form, creating: boolean) => SaveResourceVars;
  /** The id of a view row (for delete + edit addressing). */
  readonly rowId: (row: View) => string;
  /** The display name of a view row (for the delete confirmation). */
  readonly rowName: (row: View) => string;
  /** Render the kind-specific form fields with the live per-field errors. */
  readonly renderFields: (
    form: Form,
    setForm: (next: Form) => void,
    creating: boolean,
    errors: FieldErrors<Field>,
  ) => ReactNode;
}

const RESOURCE_CONTEXT: ResourceContext = {};

const NO_ERRORS = {};

/** The shared CRUD page. */
export function CrudPage<View, Form, Field extends string>(
  props: CrudPageProps<View, Form, Field>,
): JSX.Element {
  const { t } = useLingui();
  const save = useSaveResource(props.kind, RESOURCE_CONTEXT);
  const remove = useDeleteResource(props.kind, RESOURCE_CONTEXT);

  const [form, setForm] = useState<Form | null>(null);
  const [creating, setCreating] = useState(true);
  const [pendingDelete, setPendingDelete] = useState<View | null>(null);
  const [errors, setErrors] = useState<FieldErrors<Field>>(NO_ERRORS);

  const openCreate = (): void => {
    setCreating(true);
    setErrors(NO_ERRORS);
    setForm(props.emptyForm());
  };

  const openEdit = (row: View): void => {
    setCreating(false);
    setErrors(NO_ERRORS);
    void getResource(props.kind, props.rowId(row), RESOURCE_CONTEXT)
      .then((result): void => {
        const parsed = props.formFromRecord(result.record);
        if (parsed === undefined) {
          // Defensive backstop: the row-level Edit is already refused for an
          // unknown kind; never open a form that would rewrite the document.
          toast({
            title: t`Not editable in this UI`,
            description: t`This entry's kind has no form here; the stored document is preserved as authored.`,
          });
          return;
        }
        setForm(parsed);
      })
      .catch((error: unknown): void => {
        toast({
          title: t`Could not load for editing`,
          description: error instanceof Error ? error.message : String(error),
          variant: 'destructive',
        });
      });
  };

  const columns = props.columns(openEdit, setPendingDelete);

  const closeForm = (): void => {
    setForm(null);
    setErrors(NO_ERRORS);
  };

  const submitForm = (): void => {
    if (form === null) {
      return;
    }
    const found = props.validate(form, creating);
    setErrors(found);
    if (Object.values(found).some((code) => code !== undefined)) {
      return;
    }
    save.mutate(props.toSaveVars(form, creating), {
      onSuccess: (): void => {
        toast({
          title: creating ? t`Created` : t`Saved`,
          description: props.savedDescription,
        });
        closeForm();
      },
      onError: (error): void => {
        toast({
          title: creating ? t`Could not create` : t`Could not save`,
          description: error.message,
          variant: 'destructive',
        });
      },
    });
  };

  const confirmDelete = (): void => {
    const target = pendingDelete;
    if (target === null) {
      return;
    }
    remove.mutate(props.rowId(target), {
      onSuccess: (): void => {
        toast({ title: t`Deleted`, description: props.deletedDescription });
      },
      onError: (error): void => {
        toast({
          title: t`Could not delete`,
          description: error.message,
          variant: 'destructive',
        });
      },
    });
    setPendingDelete(null);
  };

  const hasErrors = Object.values(errors).some((code) => code !== undefined);

  return (
    <>
      <PageHeader
        title={props.title}
        description={props.description}
        actions={
          <>
            {props.headerExtras}
            <Button data-testid="crud-new" onClick={openCreate}>
              <Plus aria-hidden="true" />
              {props.newLabel}
            </Button>
          </>
        }
      />

      {props.callout}

      {props.isPending ? (
        <p role="status" aria-live="polite" className="text-sm text-muted-foreground">
          {props.loadingMessage}
        </p>
      ) : props.isError ? (
        <p role="alert" className="text-sm text-destructive">
          {props.errorPrefix} {props.errorMessage ?? ''}
        </p>
      ) : (
        <ResourceTable
          rows={props.list}
          columns={columns}
          caption={props.caption}
          emptyMessage={props.emptyMessage}
        />
      )}

      <Dialog
        open={form !== null}
        onOpenChange={(open): void => {
          if (!open) {
            closeForm();
          }
        }}
      >
        <DialogContent>
          <DialogHeader>
            <DialogTitle>
              {creating ? props.dialogCreateTitle : props.dialogEditTitle}
            </DialogTitle>
            <DialogDescription>{props.dialogDescription}</DialogDescription>
          </DialogHeader>
          {form !== null ? (
            <form
              className="flex flex-col gap-4"
              noValidate
              onSubmit={(event): void => {
                event.preventDefault();
                submitForm();
              }}
            >
              <div className="flex max-h-[60vh] flex-col gap-4 overflow-y-auto pe-1">
                {props.renderFields(form, setForm, creating, errors)}
              </div>
              {hasErrors ? (
                <p role="alert" className="text-sm text-destructive">
                  <Trans>Fix the highlighted fields, then save again.</Trans>
                </p>
              ) : null}
              <DialogFooter>
                <Button type="button" variant="outline" onClick={closeForm}>
                  <Trans>Cancel</Trans>
                </Button>
                <Button type="submit" disabled={save.isPending}>
                  {creating ? <Trans>Create</Trans> : <Trans>Save</Trans>}
                </Button>
              </DialogFooter>
            </form>
          ) : null}
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
              <Trans>Delete this resource?</Trans>
            </DialogTitle>
            <DialogDescription>
              <Trans>
                This permanently removes the stored resource. The running
                engine is not affected until a new configuration is applied.
              </Trans>
            </DialogDescription>
          </DialogHeader>
          {pendingDelete !== null ? (
            <p className="text-sm">
              <span lang="" dir="auto" className="font-medium">
                {props.rowName(pendingDelete)}
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
            <Button
              variant="destructive"
              disabled={remove.isPending}
              onClick={confirmDelete}
            >
              <Trans>Delete</Trans>
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </>
  );
}
