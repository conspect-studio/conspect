// Audio — document-level audio routing: program-mix membership + discrete tracks.
//
// Manages the singleton `[audio]` block (PUT /api/v1/audio-routing): the
// working sample rate and one route per input (program-bus include/gain/mute,
// optional discrete track + language/title). The page also LISTS the resulting
// selectable tracks — the program bus "prog" plus every declared track —
// because that is exactly the set each output's audio selection (Outputs page)
// resolves against. Saves replace the WHOLE document with `If-Match`. On a run
// with program audio, program-mix changes (include, gain, mute) apply to the
// running bus at once on a short click-free ramp; the server's
// X-Multiview-Apply header says which applied, and the page reports exactly
// that (a changed sample rate, or an input with no audio, needs a restart).
import { useState } from 'react';
import type { JSX } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import { Plus, Trash2 } from 'lucide-react';

import { useSources } from '../resources/queries';
import {
  useAudioRouting,
  useOutputAudioSelections,
  useSaveAudioRouting,
} from '../resources/audioRouting';
import type { OutputsUpdatedEntry } from '../resources/audioRouting';
import {
  audioFormFromDocument,
  audioFormToDocument,
  AUDIO_CHANNELS,
  declaredTracks,
  emptyAudioRoute,
  pendingTrackImpacts,
  PROGRAM_TRACK,
  validateAudioForm,
} from '../resources/audioForms';
import type {
  AudioChannelsKind,
  AudioFormErrors,
  AudioFormState,
  AudioRouteRow,
  PendingTrackImpact,
} from '../resources/audioForms';
import {
  ApplySemanticsCallout,
  CheckboxField,
  ExportConfigButton,
  FieldErrorMessage,
  FormField,
  SelectField,
} from '../resources/FormControls';
import { HelpLink } from '../components/HelpLink';
import { LoudnessMeter } from './LoudnessMeter';
import { ProgramMeters } from './ProgramMeters';
import { PageHeader } from '../components/PageHeader';
import { Badge } from '../components/ui/badge';
import { Button } from '../components/ui/button';
import {
  Table,
  TableBody,
  TableCaption,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '../components/ui/table';

/** A channel layout rendered as text (text carries the meaning). */
function channelsLabel(kind: AudioChannelsKind): JSX.Element {
  switch (kind) {
    case 'mono':
      return <Trans>Mono</Trans>;
    case 'stereo':
      return <Trans>Stereo</Trans>;
    case 'five_point_one':
      return <Trans>5.1 surround</Trans>;
  }
}

/** Replace the route at `index` with `next`. */
function withRoute(
  form: AudioFormState,
  index: number,
  next: AudioRouteRow,
): AudioFormState {
  return {
    ...form,
    routes: form.routes.map((route, i) => (i === index ? next : route)),
  };
}

/** One editable route row. */
function RouteRow({
  route,
  index,
  errors,
  sourceIds,
  onChange,
  onRemove,
}: {
  readonly route: AudioRouteRow;
  readonly index: number;
  readonly errors: AudioFormErrors['routes'][number] | undefined;
  readonly sourceIds: readonly string[];
  readonly onChange: (next: AudioRouteRow) => void;
  readonly onRemove: () => void;
}): JSX.Element {
  const { t } = useLingui();
  const row = index + 1;
  // The input picker offers the managed sources; a route bound to an id the
  // list does not carry (authored elsewhere / source deleted) stays selectable
  // so editing never silently rewrites it.
  const inputOptions =
    route.inputId !== '' && !sourceIds.includes(route.inputId)
      ? [route.inputId, ...sourceIds]
      : sourceIds;
  return (
    <TableRow data-testid="audio-route-row">
      <TableCell className="min-w-44 align-top">
        {inputOptions.length > 0 ? (
          <SelectField<string>
            label={t`Input (route ${row})`}
            labelHidden
            value={route.inputId}
            options={inputOptions}
            placeholder={t`Choose an input`}
            error={errors?.inputId}
            onChange={(next): void => {
              onChange({ ...route, inputId: next });
            }}
          />
        ) : (
          // No managed sources to pick from (fresh deployment): fall back to a
          // typed source id so routing can still be authored first.
          <FormField
            id={`audio-route-${String(row)}-input`}
            label={t`Input (route ${row})`}
            labelHidden
            value={route.inputId}
            required
            placeholder={t`source id`}
            error={errors?.inputId}
            onChange={(next): void => {
              onChange({ ...route, inputId: next });
            }}
          />
        )}
      </TableCell>
      <TableCell className="min-w-36 align-top">
        <SelectField<AudioChannelsKind>
          label={t`Channels (route ${row})`}
          labelHidden
          value={route.channels}
          options={AUDIO_CHANNELS}
          optionLabel={channelsLabel}
          onChange={(next): void => {
            onChange({ ...route, channels: next });
          }}
        />
      </TableCell>
      <TableCell className="align-top">
        <CheckboxField
          id={`audio-route-${String(row)}-program`}
          label={t`On the program mix (route ${row})`}
          labelHidden
          checked={route.includeInProgramBus}
          onChange={(next): void => {
            onChange({ ...route, includeInProgramBus: next });
          }}
        />
      </TableCell>
      <TableCell className="w-24 align-top">
        <FormField
          id={`audio-route-${String(row)}-gain`}
          label={t`Gain in dB (route ${row})`}
          labelHidden
          value={route.gainDb}
          placeholder="0"
          error={errors?.gainDb}
          onChange={(next): void => {
            onChange({ ...route, gainDb: next });
          }}
        />
      </TableCell>
      <TableCell className="align-top">
        <CheckboxField
          id={`audio-route-${String(row)}-mute`}
          label={t`Mute on the program mix (route ${row})`}
          labelHidden
          checked={route.mute}
          onChange={(next): void => {
            onChange({ ...route, mute: next });
          }}
        />
      </TableCell>
      <TableCell className="min-w-36 align-top">
        <FormField
          id={`audio-route-${String(row)}-track`}
          label={t`Discrete track name (route ${row})`}
          labelHidden
          value={route.targetTrack}
          placeholder={t`e.g. cam1-clean`}
          error={errors?.targetTrack}
          onChange={(next): void => {
            onChange({ ...route, targetTrack: next });
          }}
        />
      </TableCell>
      <TableCell className="w-24 align-top">
        <FormField
          id={`audio-route-${String(row)}-language`}
          label={t`Language (route ${row})`}
          labelHidden
          value={route.language}
          placeholder="eng"
          onChange={(next): void => {
            onChange({ ...route, language: next });
          }}
        />
      </TableCell>
      <TableCell className="min-w-32 align-top">
        <FormField
          id={`audio-route-${String(row)}-title`}
          label={t`Track title (route ${row})`}
          labelHidden
          value={route.title}
          placeholder={t`Camera 1`}
          onChange={(next): void => {
            onChange({ ...route, title: next });
          }}
        />
      </TableCell>
      <TableCell className="align-top">
        <Button
          type="button"
          variant="ghost"
          size="icon"
          data-testid="audio-remove-route"
          aria-label={t`Remove route ${row}`}
          onClick={onRemove}
        >
          <Trash2 aria-hidden="true" />
        </Button>
      </TableCell>
    </TableRow>
  );
}

/** The selectable-tracks summary: "prog" + every declared discrete track. */
function SelectableTracks({ form }: { readonly form: AudioFormState }): JSX.Element {
  const { t } = useLingui();
  return (
    <section aria-labelledby="audio-tracks-heading" className="rounded-md border p-4">
      <h2 id="audio-tracks-heading" className="text-sm font-medium">
        <Trans>Selectable tracks</Trans>{' '}
        <HelpLink
          to="/help/concepts/glossary"
          label={t`Glossary: program bus and tracks`}
          compact
        />
      </h2>
      <ul data-testid="audio-tracks-list" className="mt-2 flex flex-wrap gap-2">
        {declaredTracks(form).map((track) => (
          <li key={track}>
            <Badge variant={track === PROGRAM_TRACK ? 'default' : 'secondary'}>
              {track}
              {track === PROGRAM_TRACK ? (
                <span className="font-normal">
                  {' '}
                  — <Trans>the mixed program bus, always available</Trans>
                </span>
              ) : null}
            </Badge>
          </li>
        ))}
      </ul>
      <p className="mt-2 text-xs text-muted-foreground">
        <Trans>
          Each output picks its audio from this set: “program” mode carries the
          prog mix; “tracks” mode selects any of the names above. Manage that
          per-output choice on the Outputs page.
        </Trans>
      </p>
    </section>
  );
}

/**
 * BEFORE-save warning (ADR-R005 §4.3 UX): an output currently carries a track
 * the edited form is about to rename or remove. The save itself is no longer
 * blocked (the server propagates the change atomically), but the operator
 * should see which outputs will be rewritten before clicking Save.
 */
function PendingTrackWarning({
  impacts,
}: {
  readonly impacts: readonly PendingTrackImpact[];
}): JSX.Element | null {
  if (impacts.length === 0) {
    return null;
  }
  return (
    <section
      role="alert"
      aria-labelledby="audio-pending-impact-heading"
      className="rounded-md border border-amber-500/60 bg-amber-500/10 p-4 text-sm"
    >
      <h2 id="audio-pending-impact-heading" className="font-medium">
        <Trans>This save will update the following outputs</Trans>
      </h2>
      <ul className="mt-2 flex flex-col gap-1">
        {impacts.map((impact) => (
          <li key={impact.outputId} data-testid="audio-pending-impact">
            {impact.renamed.map(([from, to]) => (
              <span key={`${from}->${to}`} className="block">
                <Trans>
                  Output "{impact.outputName}" carries track {from}; it will be
                  updated to {to}.
                </Trans>
              </span>
            ))}
            {impact.removed.map((track) => (
              <span key={track} className="block">
                <Trans>
                  Output "{impact.outputName}" carries track {track}; it will
                  be removed from its selection.
                </Trans>
              </span>
            ))}
          </li>
        ))}
      </ul>
    </section>
  );
}

/** AFTER-save note: the server's description of every output the rename/
 * removal propagation actually rewrote (authoritative — this is what
 * happened, not a client-side prediction). */
function OutputsUpdatedNote({
  entries,
}: {
  readonly entries: readonly OutputsUpdatedEntry[];
}): JSX.Element | null {
  if (entries.length === 0) {
    return null;
  }
  return (
    <p role="status" data-testid="audio-outputs-updated-note" className="text-sm text-muted-foreground">
      <Trans>Updated outputs:</Trans>{' '}
      {entries
        .map((entry) => {
          const parts: string[] = [
            ...entry.renamed.map((r) => `${r.from} → ${r.to}`),
            ...entry.removed.map((track) => `${track} removed`),
          ];
          return `${entry.output} (${parts.join(', ')})`;
        })
        .join('; ')}
    </p>
  );
}

/** Audio routing management (the document the per-output selections resolve against). */
export function AudioPage(): JSX.Element {
  const { t } = useLingui();
  const sources = useSources();
  const routing = useAudioRouting();
  const outputSelections = useOutputAudioSelections();
  const save = useSaveAudioRouting();

  // The operator's edits own the form once they start; until then the form is
  // DERIVED from the fetched document (no state-sync effect needed). The etag
  // tracks the same way: a successful save overrides the fetched one until the
  // invalidated query re-reads it.
  const [edited, setEdited] = useState<AudioFormState | undefined>(undefined);
  const [savedEtag, setSavedEtag] = useState<string | undefined>(undefined);
  const [errors, setErrors] = useState<AudioFormErrors | undefined>(undefined);
  const [status, setStatus] = useState<'idle' | 'live' | 'partial' | 'saved' | 'error'>('idle');
  const [saveError, setSaveError] = useState<string | undefined>(undefined);
  const [outputsUpdated, setOutputsUpdated] = useState<
    readonly OutputsUpdatedEntry[]
  >([]);

  const form =
    edited ??
    (routing.data !== undefined
      ? audioFormFromDocument(routing.data.state.routing)
      : undefined);
  const etag = savedEtag ?? routing.data?.etag;

  const sourceIds = (sources.data ?? []).map((source) => source.id);

  // The document currently on the server (unedited) — the baseline the
  // pending-impact warning diffs the operator's in-progress edits against.
  const serverForm =
    routing.data !== undefined ? audioFormFromDocument(routing.data.state.routing) : undefined;
  const pendingImpacts =
    form !== undefined && serverForm !== undefined && outputSelections.data !== undefined
      ? pendingTrackImpacts(serverForm, form, outputSelections.data)
      : [];

  const update = (next: AudioFormState): void => {
    setEdited(next);
    setStatus('idle');
    setOutputsUpdated([]);
    if (errors !== undefined) {
      setErrors(validateAudioForm(next));
    }
  };

  const onSave = (): void => {
    if (form === undefined) {
      return;
    }
    const validation = validateAudioForm(form);
    if (validation.hasErrors) {
      setErrors(validation);
      return;
    }
    setErrors(undefined);
    save.mutate(
      { document: audioFormToDocument(form), etag },
      {
        onSuccess: (result): void => {
          setSavedEtag(result.etag);
          setStatus(result.apply === 'live' ? 'live' : result.apply === 'partial' ? 'partial' : 'saved');
          setSaveError(undefined);
          setOutputsUpdated(result.state.outputs_updated ?? []);
        },
        onError: (error): void => {
          setStatus('error');
          setSaveError(error.message);
        },
      },
    );
  };

  return (
    <div>
      <PageHeader
        title={<Trans>Audio</Trans>}
        description={
          <span className="inline-flex flex-wrap items-center gap-x-3 gap-y-1">
            <Trans>
              Route each input's audio: program-mix membership and discrete tracks.
            </Trans>
            <HelpLink to="/help/features#outputs" label={t`About outputs and audio`} />
          </span>
        }
        actions={<ExportConfigButton compact />}
      />
      <ApplySemanticsCallout
        helpTo="/help/features#outputs"
        helpLabel={t`How configuration applies`}
        message={
          <Trans>
            Program mix gain, mute, and source membership apply on the next
            audio tick with a short fade. New, removed, or renamed discrete
            tracks can remain pending while the program mix applies live.
            A changed output sample rate or unknown mixed source requires a
            config export and restart. X-Multiview-Apply reports live, partial,
            or restart.
          </Trans>
        }
      />

      {routing.isPending || form === undefined ? (
        routing.isError ? (
          <p role="alert" className="text-sm text-destructive">
            <Trans>Could not load the audio routing:</Trans>{' '}
            {routing.error.message}
          </p>
        ) : (
          <p className="text-sm text-muted-foreground">
            <Trans>Loading audio routing…</Trans>
          </p>
        )
      ) : (
        <div className="flex flex-col gap-6">
          <div className="max-w-xs">
            <FormField
              id="audio-sample-rate"
              label={t`Sample rate (Hz)`}
              type="number"
              value={form.sampleRateHz}
              required
              placeholder="48000"
              error={errors?.sampleRateHz}
              hint={
                <Trans>
                  The working rate every input is resampled to (48000 is the
                  broadcast default).
                </Trans>
              }
              onChange={(next): void => {
                update({ ...form, sampleRateHz: next });
              }}
            />
          </div>

          {errors?.program !== undefined ? (
            <p role="alert" className="text-sm text-destructive">
              <FieldErrorMessage code={errors.program} />
            </p>
          ) : null}

          <Table aria-label={t`Audio routes`}>
            <TableCaption>
              <Trans>
                One route per input. No track name means the input contributes
                to the program mix only.
              </Trans>
            </TableCaption>
            <TableHeader>
              <TableRow>
                <TableHead scope="col">
                  <Trans>Input</Trans>
                </TableHead>
                <TableHead scope="col">
                  <Trans>Channels</Trans>
                </TableHead>
                <TableHead scope="col">
                  <Trans>Program mix</Trans>
                </TableHead>
                <TableHead scope="col">
                  <Trans>Gain (dB)</Trans>
                </TableHead>
                <TableHead scope="col">
                  <Trans>Mute</Trans>
                </TableHead>
                <TableHead scope="col">
                  <Trans>Discrete track</Trans>
                </TableHead>
                <TableHead scope="col">
                  <Trans>Language</Trans>
                </TableHead>
                <TableHead scope="col">
                  <Trans>Title</Trans>
                </TableHead>
                <TableHead scope="col">
                  <span className="sr-only">
                    <Trans>Actions</Trans>
                  </span>
                </TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {form.routes.length === 0 ? (
                <TableRow>
                  <TableCell colSpan={9} className="text-muted-foreground">
                    <Trans>
                      No routes yet — without routes the multiview carries no
                      managed audio.
                    </Trans>
                  </TableCell>
                </TableRow>
              ) : (
                form.routes.map((route, index) => (
                  <RouteRow
                    // Index keying is deliberate: rows are positional edits of
                    // one document, not identity-bearing records.
                    key={index}
                    route={route}
                    index={index}
                    errors={errors?.routes[index]}
                    sourceIds={sourceIds}
                    onChange={(next): void => {
                      update(withRoute(form, index, next));
                    }}
                    onRemove={(): void => {
                      update({
                        ...form,
                        routes: form.routes.filter((_, i) => i !== index),
                      });
                    }}
                  />
                ))
              )}
            </TableBody>
          </Table>

          <PendingTrackWarning impacts={pendingImpacts} />

          <div className="flex flex-wrap items-center gap-2">
            <Button
              type="button"
              variant="outline"
              data-testid="audio-add-route"
              onClick={(): void => {
                update({ ...form, routes: [...form.routes, emptyAudioRoute()] });
              }}
            >
              <Plus aria-hidden="true" />
              <Trans>Add route</Trans>
            </Button>
            <Button
              type="button"
              data-testid="audio-save"
              disabled={save.isPending}
              onClick={onSave}
            >
              <Trans>Save audio routing</Trans>
            </Button>
            <p role="status" aria-live="polite" className="text-sm">
              {status === 'live' ? (
                <span className="text-muted-foreground">
                  <Trans>
                    Saved and applied live: the program mix is now following
                    the new routing.
                  </Trans>
                </span>
              ) : null}
              {status === 'partial' ? (
                <span className="text-muted-foreground">
                  <Trans>
                    Saved: program mix gain, mute, and source routes apply live.
                    New, removed, reordered, or renamed output tracks remain pending
                    until output reconfiguration. The saved document is not fully live.
                  </Trans>
                </span>
              ) : null}
              {status === 'saved' ? (
                <span className="text-muted-foreground">
                  <Trans>
                    Stored, not applied live: this change needs a restart (for
                    example a changed sample rate, or an input with no audio on
                    this run). The running program mix is unchanged until then.
                  </Trans>
                </span>
              ) : null}
              {status === 'error' && saveError !== undefined ? (
                <span className="text-destructive">
                  <Trans>Could not save the audio routing:</Trans> {saveError}
                </span>
              ) : null}
            </p>
          </div>
          <OutputsUpdatedNote entries={outputsUpdated} />

          <SelectableTracks form={form} />

          {/* Live program-bus loudness compliance meter (AUD-8): subscribes to
              the engine's conflated `audio.loudness` realtime topic and renders
              M/S/I/LRA/dBTP against the compliance target, ballistics applied
              client-side. UI-only — it consumes telemetry, never the engine path
              (invariant #10). */}
          <ProgramMeters />
          <LoudnessMeter />
        </div>
      )}
    </div>
  );
}
