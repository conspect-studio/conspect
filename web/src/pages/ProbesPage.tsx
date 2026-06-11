// Probes — kind-specific typed management of the per-cell fail-state
// detectors (black / freeze / silence / loudness).
//
// Every form field maps 1:1 onto the config `Probe` schema
// (crates/multiview-config/src/probe.rs) via the pure mapping in
// ../resources/forms (unit-tested); the control plane 422s anything else
// (ADR-W015). The cell picker is fed from the working layout's cells (the same
// layout documents the editor reads); when no layout declares cells it
// degrades to a free-text id field, because honesty beats an empty picker.
import { useMemo } from 'react';
import type { JSX } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import type { ColumnDef } from '@tanstack/react-table';

import { createApiClient } from '../api/client';
import { useLayouts } from '../api/queries';
import { useProbes } from '../resources/queries';
import type { SaveResourceVars } from '../resources/queries';
import type { ProbeView } from '../resources/types';
import { PROBE_KINDS } from '../resources/types';
import type { ProbeKind } from '../resources/types';
import {
  cellIdsFromLayouts,
  emptyProbeForm,
  LOUDNESS_STANDARDS,
  PROBE_SEVERITIES,
  probeFormFromRecord,
  probeFormToBody,
  validateProbeForm,
  withProbeKind,
  withProbeLoudnessStandard,
} from '../resources/forms';
import type {
  FieldErrors,
  LoudnessStandard,
  ProbeField,
  ProbeFormState,
  ProbeSeverity,
} from '../resources/forms';
import { CrudPage, KindCell, NameCell, RowActions } from '../resources/CrudPage';
import {
  AdvancedSection,
  ApplySemanticsCallout,
  CheckboxField,
  ExportConfigButton,
  FormField,
  SelectField,
} from '../resources/FormControls';
import { HelpLink } from '../components/HelpLink';
import { Badge } from '../components/ui/badge';

/** The localized display label for a probe kind. */
function probeKindLabel(kind: ProbeKind): JSX.Element {
  switch (kind) {
    case 'black':
      return <Trans>Black picture</Trans>;
    case 'freeze':
      return <Trans>Frozen picture</Trans>;
    case 'silence':
      return <Trans>Audio silence</Trans>;
    case 'loudness':
      return <Trans>Loudness violation</Trans>;
  }
}

/** The localized display label for an X.733 severity. */
function severityLabel(severity: ProbeSeverity): JSX.Element {
  switch (severity) {
    case 'Cleared':
      return <Trans>Cleared (no alarm)</Trans>;
    case 'Indeterminate':
      return <Trans>Indeterminate</Trans>;
    case 'Warning':
      return <Trans>Warning</Trans>;
    case 'Minor':
      return <Trans>Minor</Trans>;
    case 'Major':
      return <Trans>Major</Trans>;
    case 'Critical':
      return <Trans>Critical</Trans>;
  }
}

/** The localized display label for a loudness compliance standard. */
function loudnessStandardLabel(standard: LoudnessStandard): JSX.Element {
  switch (standard) {
    case 'r128':
      return <Trans>EBU R128 (−23 LUFS)</Trans>;
    case 'a85':
      return <Trans>ATSC A/85 (−24 LKFS)</Trans>;
  }
}

/** The cell picker: a select fed from the layout cells, or a free-text field. */
function CellField({
  form,
  setForm,
  errors,
  cells,
}: {
  readonly form: ProbeFormState;
  readonly setForm: (next: ProbeFormState) => void;
  readonly errors: FieldErrors<ProbeField>;
  readonly cells: readonly string[];
}): JSX.Element {
  const { t } = useLingui();
  // Keep a stored cell selectable even when it is not in any layout (stale
  // reference): the operator sees it as authored instead of a blank control.
  const options = useMemo(() => {
    const current = form.cell.trim();
    return current !== '' && !cells.includes(current) ? [...cells, current] : cells;
  }, [cells, form.cell]);
  if (options.length === 0) {
    return (
      <FormField
        id="probe-cell"
        label={t`Cell`}
        value={form.cell}
        required
        placeholder={t`e.g. cell-1`}
        error={errors.cell}
        hint={<Trans>No layout cells found — enter the cell id to watch.</Trans>}
        onChange={(next): void => {
          setForm({ ...form, cell: next });
        }}
      />
    );
  }
  return (
    <div className="flex flex-col gap-1">
      <SelectField<string>
        label={t`Cell`}
        value={form.cell}
        options={options}
        testId="probe-cell-select"
        onChange={(next): void => {
          setForm({ ...form, cell: next });
        }}
      />
      {errors.cell !== undefined ? (
        <p className="text-sm text-destructive">
          <Trans>Pick the cell this probe watches.</Trans>
        </p>
      ) : null}
    </div>
  );
}

/** The detection-zone disclosure shared by the black and freeze kinds. */
function ZoneFields({
  form,
  setForm,
  errors,
}: {
  readonly form: ProbeFormState;
  readonly setForm: (next: ProbeFormState) => void;
  readonly errors: FieldErrors<ProbeField>;
}): JSX.Element {
  const { t } = useLingui();
  return (
    <>
      <CheckboxField
        id="probe-zone-enabled"
        label={t`Limit detection to a zone (default: the full frame)`}
        checked={form.zoneEnabled}
        onChange={(next): void => {
          setForm({ ...form, zoneEnabled: next });
        }}
      />
      {form.zoneEnabled ? (
        <div className="grid grid-cols-2 gap-3">
          <FormField
            id="probe-zone-x"
            label={t`Zone left (x, 0–1)`}
            value={form.zoneX}
            placeholder="0"
            error={errors.zoneX}
            onChange={(next): void => {
              setForm({ ...form, zoneX: next });
            }}
          />
          <FormField
            id="probe-zone-y"
            label={t`Zone top (y, 0–1)`}
            value={form.zoneY}
            placeholder="0"
            error={errors.zoneY}
            onChange={(next): void => {
              setForm({ ...form, zoneY: next });
            }}
          />
          <FormField
            id="probe-zone-w"
            label={t`Zone width (0–1)`}
            value={form.zoneW}
            placeholder="1"
            error={errors.zoneW}
            onChange={(next): void => {
              setForm({ ...form, zoneW: next });
            }}
          />
          <FormField
            id="probe-zone-h"
            label={t`Zone height (0–1)`}
            value={form.zoneH}
            placeholder="1"
            error={errors.zoneH}
            hint={
              <Trans>
                Fractions of the tile, so a static logo or lower-third cannot
                mask a black or frozen background.
              </Trans>
            }
            onChange={(next): void => {
              setForm({ ...form, zoneH: next });
            }}
          />
        </div>
      ) : null}
    </>
  );
}

/** The kind-specific threshold fields. */
function ProbeKindFields({
  form,
  setForm,
  errors,
}: {
  readonly form: ProbeFormState;
  readonly setForm: (next: ProbeFormState) => void;
  readonly errors: FieldErrors<ProbeField>;
}): JSX.Element {
  const { t } = useLingui();
  switch (form.kind) {
    case 'black':
      return (
        <>
          <FormField
            id="probe-luma"
            label={t`Luma threshold (0–255)`}
            type="number"
            value={form.lumaThreshold}
            required
            placeholder="16"
            error={errors.lumaThreshold}
            hint={
              <Trans>
                Pixels at or below this 8-bit luma count as black (16 = broadcast
                black).
              </Trans>
            }
            onChange={(next): void => {
              setForm({ ...form, lumaThreshold: next });
            }}
          />
          <ZoneFields form={form} setForm={setForm} errors={errors} />
        </>
      );
    case 'freeze':
      return (
        <>
          <FormField
            id="probe-difference"
            label={t`Difference threshold (0–1000 per-mille)`}
            type="number"
            value={form.differenceThreshold}
            required
            placeholder="5"
            error={errors.differenceThreshold}
            hint={
              <Trans>
                Successive frames differing by less than this per-mille of
                full-scale luma count as frozen.
              </Trans>
            }
            onChange={(next): void => {
              setForm({ ...form, differenceThreshold: next });
            }}
          />
          <ZoneFields form={form} setForm={setForm} errors={errors} />
        </>
      );
    case 'silence':
      return (
        <FormField
          id="probe-level"
          label={t`Silence level (dBFS)`}
          value={form.levelDbfs}
          required
          placeholder="-60"
          error={errors.levelDbfs}
          hint={<Trans>Audio at or below this level counts as silent (e.g. −60).</Trans>}
          onChange={(next): void => {
            setForm({ ...form, levelDbfs: next });
          }}
        />
      );
    case 'loudness':
      return (
        <>
          <SelectField<LoudnessStandard>
            label={t`Compliance standard`}
            value={form.loudnessStandard}
            options={LOUDNESS_STANDARDS}
            optionLabel={loudnessStandardLabel}
            onChange={(next): void => {
              setForm(withProbeLoudnessStandard(form, next));
            }}
          />
          <div className="grid grid-cols-2 gap-3">
            <FormField
              id="probe-loudness-target"
              label={
                form.loudnessStandard === 'a85'
                  ? t`Integrated target (LKFS)`
                  : t`Integrated target (LUFS)`
              }
              value={form.loudnessTarget}
              required
              error={errors.loudnessTarget}
              onChange={(next): void => {
                setForm({ ...form, loudnessTarget: next });
              }}
            />
            <FormField
              id="probe-loudness-peak"
              label={t`Max true-peak (dBTP)`}
              value={form.loudnessTruePeak}
              required
              error={errors.loudnessTruePeak}
              onChange={(next): void => {
                setForm({ ...form, loudnessTruePeak: next });
              }}
            />
          </div>
        </>
      );
  }
}

/** The dwell / severity / latching policy block shared by every kind. */
function ProbePolicyFields({
  form,
  setForm,
  errors,
}: {
  readonly form: ProbeFormState;
  readonly setForm: (next: ProbeFormState) => void;
  readonly errors: FieldErrors<ProbeField>;
}): JSX.Element {
  const { t } = useLingui();
  return (
    <AdvancedSection summary={t`Alarm policy (dwell, severity, latching)`}>
      <div className="grid grid-cols-2 gap-3">
        <FormField
          id="probe-dwell-up"
          label={t`Raise dwell (ms)`}
          type="number"
          value={form.dwellUpMs}
          error={errors.dwellUpMs}
          hint={<Trans>The condition must persist this long before the alarm raises.</Trans>}
          onChange={(next): void => {
            setForm({ ...form, dwellUpMs: next });
          }}
        />
        <FormField
          id="probe-dwell-down"
          label={t`Clear dwell (ms)`}
          type="number"
          value={form.dwellDownMs}
          error={errors.dwellDownMs}
          hint={<Trans>The condition must stay clear this long before the alarm clears.</Trans>}
          onChange={(next): void => {
            setForm({ ...form, dwellDownMs: next });
          }}
        />
      </div>
      <SelectField<ProbeSeverity>
        label={t`Severity (X.733)`}
        value={form.severity}
        options={PROBE_SEVERITIES}
        optionLabel={severityLabel}
        onChange={(next): void => {
          setForm({ ...form, severity: next });
        }}
      />
      <CheckboxField
        id="probe-latched"
        label={t`Latch the alarm (held until explicitly reset)`}
        checked={form.latched}
        onChange={(next): void => {
          setForm({ ...form, latched: next });
        }}
      />
    </AdvancedSection>
  );
}

/** Probes management (per-cell fail-state detection). */
export function ProbesPage(): JSX.Element {
  const { t } = useLingui();
  const probes = useProbes();
  const client = useMemo(() => createApiClient(), []);
  const layouts = useLayouts(client);
  const cells = useMemo(() => cellIdsFromLayouts(layouts.data ?? []), [layouts.data]);

  const columns = (
    onEdit: (row: ProbeView) => void,
    onDelete: (row: ProbeView) => void,
  ): ColumnDef<ProbeView>[] => [
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
      accessorKey: 'cell',
      header: t`Cell`,
      cell: (ctx): JSX.Element => (
        <code className="text-xs text-muted-foreground" lang="" dir="auto">
          {ctx.row.original.cell !== '' ? ctx.row.original.cell : '—'}
        </code>
      ),
    },
    {
      accessorKey: 'severity',
      header: (): JSX.Element => (
        <span className="inline-flex items-center gap-1.5">
          <Trans>Severity</Trans>
          <HelpLink to="/help/features#alarms" label={t`About probes and alarms`} compact />
        </span>
      ),
      cell: (ctx): JSX.Element => (
        <span className="inline-flex items-center gap-1.5">
          <Badge variant="outline">{ctx.row.original.severity}</Badge>
          {ctx.row.original.latched ? (
            <span className="text-xs text-muted-foreground">
              <Trans>latched</Trans>
            </span>
          ) : null}
        </span>
      ),
    },
    {
      id: 'actions',
      header: t`Actions`,
      cell: (ctx): JSX.Element => (
        <RowActions
          row={ctx.row.original}
          name={ctx.row.original.name}
          editLabel={t`Edit probe`}
          deleteLabel={t`Delete probe`}
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
    <CrudPage<ProbeView, ProbeFormState, ProbeField>
      kind="probes"
      title={<Trans>Probes</Trans>}
      description={
        <span className="inline-flex flex-wrap items-center gap-x-3 gap-y-1">
          <Trans>
            Detect per-cell fail states — black, freeze, silence, and loudness
            violations — and raise X.733 alarms.
          </Trans>
          <HelpLink to="/help/features#alarms" label={t`About probes and alarms`} />
        </span>
      }
      newLabel={t`New probe`}
      dialogCreateTitle={t`New probe`}
      dialogEditTitle={t`Edit probe`}
      dialogDescription={t`A probe watches one cell for a fail state and raises an alarm after its dwell window.`}
      caption={t`Configured fail-state probes.`}
      emptyMessage={<Trans>No probes configured.</Trans>}
      loadingMessage={<Trans>Loading probes…</Trans>}
      errorPrefix={<Trans>Could not load probes:</Trans>}
      headerExtras={<ExportConfigButton compact />}
      callout={
        <ApplySemanticsCallout
          helpTo="/help/features#alarms"
          helpLabel={t`How configuration applies`}
        />
      }
      savedDescription={t`Stored. It goes live via config export + restart; the running engine is unchanged until then.`}
      deletedDescription={t`Removed from the store. The running engine is unchanged until a config export + restart.`}
      list={probes.data ?? []}
      isPending={probes.isPending}
      isError={probes.isError}
      errorMessage={probes.error?.message}
      columns={columns}
      rowId={(row): string => row.id}
      rowName={(row): string => row.name}
      emptyForm={emptyProbeForm}
      formFromRecord={probeFormFromRecord}
      validate={validateProbeForm}
      toSaveVars={(form, creating): SaveResourceVars => ({
        id: creating ? form.id.trim() : form.id,
        create: creating,
        input: { name: form.name.trim(), body: probeFormToBody(form) },
      })}
      renderFields={(form, setForm, creating, errors): JSX.Element => (
        <>
          <FormField
            id="probe-id"
            label={t`Identifier`}
            value={form.id}
            disabled={!creating}
            required={creating}
            placeholder={t`e.g. black-cam-1`}
            error={errors.id}
            onChange={(next): void => {
              setForm({ ...form, id: next });
            }}
          />
          <FormField
            id="probe-name"
            label={t`Name`}
            value={form.name}
            required
            error={errors.name}
            onChange={(next): void => {
              setForm({ ...form, name: next });
            }}
          />
          <CellField form={form} setForm={setForm} errors={errors} cells={cells} />
          <SelectField<ProbeKind>
            label={t`Kind`}
            value={form.kind}
            options={PROBE_KINDS}
            optionLabel={probeKindLabel}
            trailing={
              <HelpLink to="/help/features#alarms" label={t`About probes and alarms`} compact />
            }
            onChange={(next): void => {
              setForm(withProbeKind(form, next));
            }}
          />
          <ProbeKindFields form={form} setForm={setForm} errors={errors} />
          <ProbePolicyFields form={form} setForm={setForm} errors={errors} />
        </>
      )}
    />
  );
}
