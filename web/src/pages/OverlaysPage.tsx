// Overlays — kind-specific typed management of the overlay layers.
//
// The config `Overlay` (crates/multiview-config/src/schema.rs) is `id` + `kind`
// + `target` + `z` with the kind params flattened verbatim. The params rendered
// here are exactly the ones the Rust side consumes/documents: the clock
// face/tz/placement read by multiview-cli's `analog_clock_from_config`, and the
// tally_border width/colour/binding from the shipped examples. label / image /
// subtitle define no params in Rust yet, so none are invented — any verbatim
// params an authored document carries are preserved across an edit.
import type { JSX } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import type { ColumnDef } from '@tanstack/react-table';

import { useOverlays } from '../resources/queries';
import type { SaveResourceVars } from '../resources/queries';
import type { OverlayKind, OverlayView } from '../resources/types';
import { OVERLAY_KINDS } from '../resources/types';
import {
  OVERLAY_CLOCK_FACES,
  emptyOverlayForm,
  overlayFormFromRecord,
  overlayFormToBody,
  validateOverlayForm,
  withOverlayKind,
} from '../resources/forms';
import type {
  ClockFace,
  FieldErrors,
  OverlayField,
  OverlayFormState,
} from '../resources/forms';
import { CrudPage, KindCell, NameCell, RowActions } from '../resources/CrudPage';
import { FormField, SelectField } from '../resources/FormControls';
import { HelpLink } from '../components/HelpLink';

/** The kind-specific parameter fields. */
function OverlayKindFields({
  form,
  setForm,
  errors,
}: {
  readonly form: OverlayFormState;
  readonly setForm: (next: OverlayFormState) => void;
  readonly errors: FieldErrors<OverlayField>;
}): JSX.Element {
  const { t } = useLingui();
  switch (form.kind) {
    case 'clock':
      return (
        <>
          <SelectField<ClockFace>
            label={t`Face`}
            value={form.clockFace}
            options={OVERLAY_CLOCK_FACES}
            onChange={(next): void => {
              setForm({ ...form, clockFace: next });
            }}
          />
          <FormField
            id="overlay-clock-tz"
            label={t`Timezone offset (minutes from UTC, optional)`}
            type="number"
            value={form.clockTzMinutes}
            placeholder="0"
            error={errors.clockTzMinutes}
            hint={<Trans>Blank renders UTC.</Trans>}
            onChange={(next): void => {
              setForm({ ...form, clockTzMinutes: next });
            }}
          />
          <div className="grid grid-cols-3 gap-3">
            <FormField
              id="overlay-clock-x"
              label={t`Centre x (px)`}
              type="number"
              value={form.clockX}
              placeholder={t`auto`}
              error={errors.clockX}
              onChange={(next): void => {
                setForm({ ...form, clockX: next });
              }}
            />
            <FormField
              id="overlay-clock-y"
              label={t`Centre y (px)`}
              type="number"
              value={form.clockY}
              placeholder={t`auto`}
              error={errors.clockY}
              onChange={(next): void => {
                setForm({ ...form, clockY: next });
              }}
            />
            <FormField
              id="overlay-clock-radius"
              label={t`Radius (px)`}
              type="number"
              value={form.clockRadius}
              placeholder={t`auto`}
              error={errors.clockRadius}
              onChange={(next): void => {
                setForm({ ...form, clockRadius: next });
              }}
            />
          </div>
        </>
      );
    case 'tally_border':
      return (
        <>
          <FormField
            id="overlay-tally-width"
            label={t`Border width (px)`}
            type="number"
            value={form.tallyWidthPx}
            placeholder="6"
            error={errors.tallyWidthPx}
            onChange={(next): void => {
              setForm({ ...form, tallyWidthPx: next });
            }}
          />
          <FormField
            id="overlay-tally-color"
            label={t`Border colour`}
            value={form.tallyColor}
            placeholder="#FF0000"
            error={errors.tallyColor}
            onChange={(next): void => {
              setForm({ ...form, tallyColor: next });
            }}
          />
          <FormField
            id="overlay-tally-binding"
            label={t`Tally binding (optional)`}
            value={form.tallyBinding}
            placeholder="tally://cell_big"
            hint={<Trans>The external tally state that drives this border.</Trans>}
            onChange={(next): void => {
              setForm({ ...form, tallyBinding: next });
            }}
          />
        </>
      );
    default:
      // label / image / subtitle: no kind params are defined in Rust yet.
      return (
        <p className="text-sm text-muted-foreground">
          <Trans>
            This overlay kind has no configurable parameters in this build.
            Parameters authored in a config file are preserved unchanged.
          </Trans>
        </p>
      );
  }
}

/** Overlays + subtitles management. */
export function OverlaysPage(): JSX.Element {
  const { t } = useLingui();
  const overlays = useOverlays();

  const columns = (
    onEdit: (row: OverlayView) => void,
    onDelete: (row: OverlayView) => void,
  ): ColumnDef<OverlayView>[] => [
    {
      accessorKey: 'name',
      header: t`Name`,
      cell: (ctx): JSX.Element => <NameCell value={ctx.row.original.name} />,
    },
    {
      accessorKey: 'kind',
      header: t`Kind`,
      cell: (ctx): JSX.Element => <KindCell value={ctx.row.original.rawKind} />,
    },
    {
      accessorKey: 'target',
      header: t`Target`,
      cell: (ctx): JSX.Element => (
        <code className="text-xs text-muted-foreground" lang="" dir="auto">
          {ctx.row.original.target}
        </code>
      ),
    },
    {
      accessorKey: 'z',
      header: t`Stacking`,
      cell: (ctx): JSX.Element => (
        <span className="tabular-nums">{ctx.row.original.z}</span>
      ),
    },
    {
      id: 'actions',
      header: t`Actions`,
      cell: (ctx): JSX.Element => (
        <RowActions
          row={ctx.row.original}
          name={ctx.row.original.name}
          editLabel={t`Edit overlay`}
          deleteLabel={t`Delete overlay`}
          editDisabledReason={
            ctx.row.original.editable
              ? undefined
              : t`kind "${ctx.row.original.rawKind}" isn't editable in this UI; the document is preserved as authored`
          }
          onEdit={onEdit}
          onDelete={onDelete}
        />
      ),
    },
  ];

  return (
    <CrudPage<OverlayView, OverlayFormState, OverlayField>
      kind="overlays"
      title={<Trans>Overlays</Trans>}
      description={
        <span className="inline-flex flex-wrap items-center gap-x-3 gap-y-1">
          <Trans>Manage overlay layers and subtitles.</Trans>
          <HelpLink to="/help/features#overlays" label={t`About overlays`} />
        </span>
      }
      newLabel={t`New overlay`}
      dialogCreateTitle={t`New overlay`}
      dialogEditTitle={t`Edit overlay`}
      dialogDescription={t`An overlay is a layer composited over the program at a stacking order.`}
      caption={t`Configured overlay layers.`}
      emptyMessage={<Trans>No overlays configured.</Trans>}
      loadingMessage={<Trans>Loading overlays…</Trans>}
      errorPrefix={<Trans>Could not load overlays:</Trans>}
      savedDescription={t`Stored. It goes live via config export + restart; the running engine is unchanged until then.`}
      deletedDescription={t`Removed from the store. The running engine is unchanged until a config export + restart.`}
      list={overlays.data ?? []}
      isPending={overlays.isPending}
      isError={overlays.isError}
      errorMessage={overlays.error?.message}
      columns={columns}
      rowId={(row): string => row.id}
      rowName={(row): string => row.name}
      emptyForm={emptyOverlayForm}
      formFromRecord={overlayFormFromRecord}
      validate={validateOverlayForm}
      toSaveVars={(form, creating): SaveResourceVars => ({
        id: creating ? form.id.trim() : form.id,
        create: creating,
        input: { name: form.name.trim(), body: overlayFormToBody(form) },
      })}
      renderFields={(form, setForm, creating, errors): JSX.Element => (
        <>
          <FormField
            id="overlay-id"
            label={t`Identifier`}
            value={form.id}
            disabled={!creating}
            required={creating}
            placeholder={t`e.g. wall-clock`}
            error={errors.id}
            onChange={(next): void => {
              setForm({ ...form, id: next });
            }}
          />
          <FormField
            id="overlay-name"
            label={t`Name`}
            value={form.name}
            required
            error={errors.name}
            onChange={(next): void => {
              setForm({ ...form, name: next });
            }}
          />
          <SelectField<OverlayKind>
            label={t`Kind`}
            value={form.kind}
            options={OVERLAY_KINDS}
            onChange={(next): void => {
              setForm(withOverlayKind(form, next));
            }}
          />
          <FormField
            id="overlay-target"
            label={t`Target`}
            value={form.target}
            required
            placeholder={t`canvas or a cell id`}
            error={errors.target}
            onChange={(next): void => {
              setForm({ ...form, target: next });
            }}
          />
          <FormField
            id="overlay-z"
            label={t`Stacking order`}
            type="number"
            value={form.z}
            error={errors.z}
            onChange={(next): void => {
              setForm({ ...form, z: next });
            }}
          />
          <OverlayKindFields form={form} setForm={setForm} errors={errors} />
        </>
      )}
    />
  );
}
