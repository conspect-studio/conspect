// Outputs — kind-specific typed management of the program sinks/servers.
//
// Form fields map 1:1 onto the config `Output` schema
// (crates/multiview-config/src/schema.rs) via ../resources/forms. HONESTY:
// `rtsp_server` and `ndi` outputs are accepted by the config but NOT yet
// runnable — `build_outputs` in crates/multiview-cli/src/pipeline.rs warns and
// skips them — so the table and the form flag those kinds with a clear,
// non-alarming "Not yet runnable in this build" note (text + icon, never
// colour alone). hls / ll_hls / rtmp / srt run today; `display` (the local
// DRM/KMS head, DEV-B1/ADR-0044) runs only in a `display-kms` build and is
// badged "Requires a display-kms build" (a default build fails the run with a
// clear error rather than skipping the output).
import type { JSX } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import { CircleCheck, Hourglass, MonitorCog } from 'lucide-react';
import type { ColumnDef } from '@tanstack/react-table';

import { useOutputs } from '../resources/queries';
import type { SaveResourceVars } from '../resources/queries';
import type { OutputKind, OutputView } from '../resources/types';
import { OUTPUT_KINDS } from '../resources/types';
import {
  emptyOutputForm,
  OUTPUT_RUNNABLE,
  outputFormFromRecord,
  outputFormToBody,
  PIN_VENDORS,
  validateOutputForm,
} from '../resources/forms';
import type {
  DisplayModeChoice,
  FieldErrors,
  OutputAudioChoice,
  OutputField,
  OutputFormState,
  PinVendor,
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

const AUDIO_CHOICES: readonly OutputAudioChoice[] = ['default', 'program', 'tracks'];

/** The curated codec choices; `custom` reveals free entry (the schema string is open). */
const CODEC_PRESETS = ['h264', 'hevc'] as const;
type CodecChoice = (typeof CODEC_PRESETS)[number] | 'custom';
const CODEC_CHOICES: readonly CodecChoice[] = ['h264', 'hevc', 'custom'];

function codecChoiceOf(codec: string): CodecChoice {
  return CODEC_PRESETS.find((preset) => preset === codec) ?? 'custom';
}

/** Runnability of an output kind in this build, as text + icon. */
function RunnabilityNote({ kind }: { readonly kind: OutputKind }): JSX.Element {
  switch (OUTPUT_RUNNABLE[kind]) {
    case 'runnable':
      return (
        <span
          className="inline-flex items-center gap-1 text-xs text-muted-foreground"
          data-testid="output-runnability"
          data-runnable="true"
        >
          <CircleCheck className="size-3.5" aria-hidden="true" />
          <Trans>Runnable</Trans>
        </span>
      );
    case 'requires-feature':
      return (
        <span
          className="inline-flex items-center gap-1 text-xs text-muted-foreground"
          data-testid="output-runnability"
          data-runnable="feature"
        >
          <MonitorCog className="size-3.5" aria-hidden="true" />
          <Trans>Requires a display-kms build</Trans>
        </span>
      );
    case 'unbuilt':
      return (
        <span
          className="inline-flex items-center gap-1 text-xs text-muted-foreground"
          data-testid="output-runnability"
          data-runnable="false"
        >
          <Hourglass className="size-3.5" aria-hidden="true" />
          <Trans>Not yet runnable in this build</Trans>
        </span>
      );
  }
}

/** The kind-specific destination + tuning fields. */
function OutputKindFields({
  form,
  setForm,
  errors,
}: {
  readonly form: OutputFormState;
  readonly setForm: (next: OutputFormState) => void;
  readonly errors: FieldErrors<OutputField>;
}): JSX.Element {
  const { t } = useLingui();
  switch (form.kind) {
    case 'rtsp':
      return (
        <>
          <FormField
            id="output-mount"
            label={t`Mount point`}
            value={form.mount}
            required
            placeholder="/multiview"
            error={errors.mount}
            onChange={(next): void => {
              setForm({ ...form, mount: next });
            }}
          />
          <FormField
            id="output-latency-profile"
            label={t`Latency profile (optional)`}
            value={form.latencyProfile}
            placeholder="low_latency"
            onChange={(next): void => {
              setForm({ ...form, latencyProfile: next });
            }}
          />
        </>
      );
    case 'hls':
      return (
        <>
          <FormField
            id="output-path"
            label={t`Output path`}
            value={form.path}
            required
            placeholder="/var/www/hls/multiview"
            error={errors.path}
            onChange={(next): void => {
              setForm({ ...form, path: next });
            }}
          />
          <FormField
            id="output-segment-ms"
            label={t`Segment duration (ms, optional)`}
            type="number"
            value={form.segmentMs}
            placeholder="4000"
            error={errors.segmentMs}
            onChange={(next): void => {
              setForm({ ...form, segmentMs: next });
            }}
          />
        </>
      );
    case 'll-hls':
      return (
        <>
          <FormField
            id="output-path"
            label={t`Output path`}
            value={form.path}
            required
            placeholder="/var/www/hls/multiview"
            error={errors.path}
            onChange={(next): void => {
              setForm({ ...form, path: next });
            }}
          />
          <div className="grid grid-cols-3 gap-3">
            <FormField
              id="output-part-ms"
              label={t`Part target (ms)`}
              type="number"
              value={form.partTargetMs}
              placeholder="200"
              error={errors.partTargetMs}
              onChange={(next): void => {
                setForm({ ...form, partTargetMs: next });
              }}
            />
            <FormField
              id="output-segment-ms"
              label={t`Segment (ms)`}
              type="number"
              value={form.segmentMs}
              placeholder="2000"
              error={errors.segmentMs}
              onChange={(next): void => {
                setForm({ ...form, segmentMs: next });
              }}
            />
            <FormField
              id="output-gop-ms"
              label={t`GOP (ms)`}
              type="number"
              value={form.gopMs}
              placeholder="1000"
              error={errors.gopMs}
              onChange={(next): void => {
                setForm({ ...form, gopMs: next });
              }}
            />
          </div>
        </>
      );
    case 'ndi':
      return (
        <FormField
          id="output-ndi-name"
          label={t`NDI source name to advertise`}
          value={form.ndiName}
          required
          placeholder={t`Multiview Program`}
          error={errors.ndiName}
          onChange={(next): void => {
            setForm({ ...form, ndiName: next });
          }}
        />
      );
    case 'rtmp':
      return (
        <FormField
          id="output-url"
          label={t`Destination URL`}
          value={form.url}
          required
          placeholder="rtmp://ingest.example/app/key"
          error={errors.url}
          onChange={(next): void => {
            setForm({ ...form, url: next });
          }}
        />
      );
    case 'srt':
      return (
        <FormField
          id="output-url"
          label={t`Destination URL`}
          value={form.url}
          required
          placeholder="srt://[2001:db8::1]:7001"
          error={errors.url}
          onChange={(next): void => {
            setForm({ ...form, url: next });
          }}
        />
      );
    case 'display':
      return <DisplayKindFields form={form} setForm={setForm} errors={errors} />;
  }
}

/** The display-head fields: connector + the mutually-exclusive mode choice. */
function DisplayKindFields({
  form,
  setForm,
  errors,
}: {
  readonly form: OutputFormState;
  readonly setForm: (next: OutputFormState) => void;
  readonly errors: FieldErrors<OutputField>;
}): JSX.Element {
  const { t } = useLingui();
  const modeLabel = (choice: DisplayModeChoice): JSX.Element => {
    switch (choice) {
      case 'auto':
        return <Trans>Automatic (EDID preferred mode)</Trans>;
      case 'override':
        return <Trans>Pick an exact EDID mode</Trans>;
      case 'forced':
        return <Trans>Forced CVT-RB timing (EDID-less screen)</Trans>;
    }
  };
  return (
    <>
      <FormField
        id="output-connector"
        label={t`Connector`}
        value={form.connector}
        required
        placeholder="DP-1"
        error={errors.connector}
        hint={
          <Trans>
            The KMS connector name (DP-1, HDMI-A-1). Use "auto" for the first
            connected screen.
          </Trans>
        }
        onChange={(next): void => {
          setForm({ ...form, connector: next });
        }}
      />
      <SelectField<DisplayModeChoice>
        label={t`Mode`}
        value={form.displayModeChoice}
        options={['auto', 'override', 'forced'] as const}
        optionLabel={modeLabel}
        onChange={(next): void => {
          setForm({ ...form, displayModeChoice: next });
        }}
      />
      {form.displayModeChoice === 'auto' ? null : (
        <div className="grid grid-cols-3 gap-3">
          <FormField
            id="output-display-width"
            label={t`Width (px)`}
            type="number"
            value={form.displayModeWidth}
            required
            placeholder="1920"
            error={errors.displayModeWidth}
            onChange={(next): void => {
              setForm({ ...form, displayModeWidth: next });
            }}
          />
          <FormField
            id="output-display-height"
            label={t`Height (px)`}
            type="number"
            value={form.displayModeHeight}
            required
            placeholder="1080"
            error={errors.displayModeHeight}
            onChange={(next): void => {
              setForm({ ...form, displayModeHeight: next });
            }}
          />
          <FormField
            id="output-display-refresh"
            label={t`Refresh (exact rational)`}
            value={form.displayModeRefresh}
            required
            placeholder="60000/1001"
            error={errors.displayModeRefresh}
            hint={<Trans>Exact rational Hz (60000/1001) or a whole number — never a decimal.</Trans>}
            onChange={(next): void => {
              setForm({ ...form, displayModeRefresh: next });
            }}
          />
        </div>
      )}
    </>
  );
}

/** The codec selector with the custom free-entry escape. */
function CodecFields({
  form,
  setForm,
  errors,
}: {
  readonly form: OutputFormState;
  readonly setForm: (next: OutputFormState) => void;
  readonly errors: FieldErrors<OutputField>;
}): JSX.Element | null {
  const { t } = useLingui();
  if (form.kind === 'ndi' || form.kind === 'display') {
    // NDI and the local display head carry frames, not an encoded rendition —
    // no codec field exists.
    return null;
  }
  const choice = codecChoiceOf(form.codec);
  return (
    <>
      <SelectField<CodecChoice>
        label={t`Codec`}
        value={choice}
        options={CODEC_CHOICES}
        optionLabel={(option): JSX.Element =>
          option === 'custom' ? <Trans>Custom…</Trans> : <>{option}</>
        }
        trailing={
          <HelpLink
            to="/help/concepts/codecs#what-is-transcoding"
            label={t`About codecs and transcoding`}
            compact
          />
        }
        onChange={(next): void => {
          setForm({ ...form, codec: next === 'custom' ? '' : next });
        }}
      />
      {choice === 'custom' ? (
        <FormField
          id="output-codec"
          label={t`Custom codec name`}
          value={form.codec}
          required
          placeholder="av1"
          error={errors.codec}
          onChange={(next): void => {
            setForm({ ...form, codec: next });
          }}
        />
      ) : null}
    </>
  );
}

/** The collapsible Advanced block (audio selection + GPU pin). */
function OutputAdvancedFields({
  form,
  setForm,
  errors,
}: {
  readonly form: OutputFormState;
  readonly setForm: (next: OutputFormState) => void;
  readonly errors: FieldErrors<OutputField>;
}): JSX.Element {
  const { t } = useLingui();
  const audioLabel = (choice: OutputAudioChoice): JSX.Element => {
    switch (choice) {
      case 'default':
        return <Trans>Engine default (mixed program bus)</Trans>;
      case 'program':
        return <Trans>Program bus only</Trans>;
      case 'tracks':
        return <Trans>Explicit track list</Trans>;
    }
  };
  return (
    <AdvancedSection summary={t`Advanced`}>
      <SelectField<OutputAudioChoice>
        label={t`Audio`}
        value={form.audioMode}
        options={AUDIO_CHOICES}
        optionLabel={audioLabel}
        onChange={(next): void => {
          setForm({ ...form, audioMode: next });
        }}
      />
      {form.audioMode === 'tracks' ? (
        <FormField
          id="output-audio-tracks"
          label={t`Tracks (comma-separated)`}
          value={form.audioTracks}
          placeholder="prog, commentary"
          error={errors.audioTracks}
          hint={
            <Trans>
              Names declared by the audio routing block; the program bus is
              always available as "prog".
            </Trans>
          }
          onChange={(next): void => {
            setForm({ ...form, audioTracks: next });
          }}
        />
      ) : null}
      <CheckboxField
        id="output-gpu-pin"
        label={t`Pin encode to a specific GPU`}
        checked={form.gpuPinEnabled}
        onChange={(next): void => {
          setForm({ ...form, gpuPinEnabled: next });
        }}
      />
      {form.gpuPinEnabled ? (
        <div className="grid grid-cols-2 gap-3">
          <SelectField<PinVendor>
            label={t`GPU vendor`}
            value={form.gpuPinVendor}
            options={PIN_VENDORS}
            onChange={(next): void => {
              setForm({ ...form, gpuPinVendor: next });
            }}
          />
          <FormField
            id="output-gpu-stable-id"
            label={t`Stable device id`}
            value={form.gpuPinStableId}
            placeholder="GPU-9d3b…  /  0000:03:00.0"
            error={errors.gpuPinStableId}
            hint={<Trans>The vendor's stable handle (UUID / PCI bus id), never the index.</Trans>}
            onChange={(next): void => {
              setForm({ ...form, gpuPinStableId: next });
            }}
          />
        </div>
      ) : null}
    </AdvancedSection>
  );
}

/** Outputs management. */
export function OutputsPage(): JSX.Element {
  const { t } = useLingui();
  const outputs = useOutputs();

  const columns = (
    onEdit: (row: OutputView) => void,
    onDelete: (row: OutputView) => void,
  ): ColumnDef<OutputView>[] => [
    {
      accessorKey: 'name',
      header: t`Name`,
      cell: (ctx): JSX.Element => <NameCell value={ctx.row.original.name} />,
    },
    {
      accessorKey: 'kind',
      header: t`Transport`,
      cell: (ctx): JSX.Element => <KindCell value={ctx.row.original.rawKind} />,
    },
    {
      accessorKey: 'target',
      header: t`Destination`,
      cell: (ctx): JSX.Element => (
        <code className="text-xs text-muted-foreground" lang="" dir="auto">
          {ctx.row.original.target ?? '—'}
        </code>
      ),
    },
    {
      accessorKey: 'codec',
      header: t`Codec`,
      cell: (ctx): JSX.Element => (
        <span className="text-sm text-muted-foreground">
          {ctx.row.original.codec ?? '—'}
        </span>
      ),
    },
    {
      id: 'runnability',
      header: t`Status`,
      cell: (ctx): JSX.Element =>
        // Runnability is defined for the kinds this build knows; an unknown
        // kind gets an honest dash, never a claim about the folded kind.
        ctx.row.original.editable ? (
          <RunnabilityNote kind={ctx.row.original.kind} />
        ) : (
          <span className="text-sm text-muted-foreground" aria-hidden="true">
            —
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
          editLabel={t`Edit output`}
          deleteLabel={t`Delete output`}
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
    <CrudPage<OutputView, OutputFormState, OutputField>
      kind="outputs"
      title={<Trans>Outputs</Trans>}
      description={
        <span className="inline-flex flex-wrap items-center gap-x-3 gap-y-1">
          <Trans>Configure output servers and renditions.</Trans>
          <HelpLink
            to="/help/concepts/latency#protocol-latency"
            label={t`Latency & protocol guide`}
          />
        </span>
      }
      newLabel={t`New output`}
      dialogCreateTitle={t`New output`}
      dialogEditTitle={t`Edit output`}
      dialogDescription={t`An output is a sink/server that publishes the program.`}
      caption={t`Configured output sinks.`}
      emptyMessage={<Trans>No outputs configured.</Trans>}
      loadingMessage={<Trans>Loading outputs…</Trans>}
      errorPrefix={<Trans>Could not load outputs:</Trans>}
      headerExtras={<ExportConfigButton compact />}
      callout={
        <ApplySemanticsCallout
          helpTo="/help/config#outputs"
          helpLabel={t`How configuration applies`}
        />
      }
      savedDescription={t`Stored. It goes live via config export + restart; the running engine is unchanged until then.`}
      deletedDescription={t`Removed from the store. The running engine is unchanged until a config export + restart.`}
      list={outputs.data ?? []}
      isPending={outputs.isPending}
      isError={outputs.isError}
      errorMessage={outputs.error?.message}
      columns={columns}
      rowId={(row): string => row.id}
      rowName={(row): string => row.name}
      emptyForm={emptyOutputForm}
      formFromRecord={outputFormFromRecord}
      validate={validateOutputForm}
      toSaveVars={(form, creating): SaveResourceVars => ({
        id: creating ? form.id.trim() : form.id,
        create: creating,
        input: { name: form.name.trim(), body: outputFormToBody(form) },
      })}
      renderFields={(form, setForm, creating, errors): JSX.Element => (
        <>
          <FormField
            id="output-id"
            label={t`Identifier`}
            value={form.id}
            disabled={!creating}
            required={creating}
            placeholder={t`e.g. program-hls`}
            error={errors.id}
            onChange={(next): void => {
              setForm({ ...form, id: next });
            }}
          />
          <FormField
            id="output-name"
            label={t`Name`}
            value={form.name}
            required
            error={errors.name}
            onChange={(next): void => {
              setForm({ ...form, name: next });
            }}
          />
          <SelectField<OutputKind>
            label={t`Transport`}
            value={form.kind}
            options={OUTPUT_KINDS}
            onChange={(next): void => {
              setForm({ ...form, kind: next });
            }}
          />
          <RunnabilityNote kind={form.kind} />
          <OutputKindFields form={form} setForm={setForm} errors={errors} />
          <CodecFields form={form} setForm={setForm} errors={errors} />
          <OutputAdvancedFields form={form} setForm={setForm} errors={errors} />
        </>
      )}
    />
  );
}
