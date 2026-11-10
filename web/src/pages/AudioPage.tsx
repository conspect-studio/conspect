// Audio — document-level audio routing as a broadcast console: the whole-mix
// overview + channel strips (program-bus membership, gain, mute) + discrete
// tracks. This is the adopted designer direction (see
// .worktrees/ui-design/designer/design/broadcast-ui — approved as the visual
// and interaction reference), rebuilt honest to the REAL API:
//
//   * The console edits a DRAFT of the singleton `[audio]` document
//     (PUT /api/v1/audio-routing replaces the WHOLE document with `If-Match`).
//     Gain drags, nudges, MUTE/PGM keys and field edits stage the draft only —
//     nothing is sent until "Save audio routing", and nothing on the strip
//     ever claims a live apply. Draft vs Saved is explicit on every readout.
//   * Meters are the engine's REAL telemetry only: per-input presence bars
//     come from the conflated `audio.meter` topic (pre-fader — independent
//     of gain, mute and program include), the master bus meters and the LUFS
//     compliance readout from the actual ProgramMeters/LoudnessMeter. No
//     level is ever derived from the fader or synthesized. There is no fake
//     PFL/listen: the control plane has no monitoring path, and it says so.
//   * Strip-scoped Undo is a LOCAL draft revert (back to the saved row) —
//     never an on-air event undo.
//   * Fader: one linear dB mapping shared by ticks, knob and readout; 0 dB at
//     the top, −60 dB at the bottom; ±1/±6 dB tap nudges (48 px) and keyboard
//     (arrows, PageUp/PageDown ±6, Home/End; Escape reverts the draft gain).
//     The fader range is a display bound — the document accepts any finite
//     gain and the readout always carries the exact value.
//   * Every advanced field (input, channels, exact gain, discrete track,
//     language, title, remove) stays reachable behind the strip's Details;
//     the sample rate lives in "Bus setup". No function was removed.
import { useRef, useState } from 'react';
import type { JSX, ReactNode } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import { ChevronRight, Plus, Trash2, Undo2 } from 'lucide-react';

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
  ExportConfigButton,
  FieldErrorMessage,
  FormField,
  SelectField,
} from '../resources/FormControls';
import { HelpLink } from '../components/HelpLink';
import { LoudnessMeter } from './LoudnessMeter';
import { ProgramMeters } from './ProgramMeters';
import {
  documentDraftDiff,
  faderDisplayDb,
  formatDb,
  faderPos,
  GAIN_FADER_MAX_DB,
  GAIN_FADER_MIN_DB,
  GAIN_SCALE_TICKS_DB,
  inputMeterSample,
  inputPeakDb,
  nudgeGainDb,
  parseGainDb,
} from './audioPresentation';
import type { RouteDraftDiff } from './audioPresentation';
import { meterFillPercent, useAudioMeter } from '../realtime/useAudioMeter';
import type { AudioMeterSample } from '../realtime/useAudioMeter';
import {
  AudioMasterRegion,
  AudioMixerLayout,
  AudioPhoneMaster,
  AudioSurface,
} from './approved/AudioSurface';
import { useAudioLoudness } from '../realtime/useAudioLoudness';
import { Badge } from '../components/ui/badge';
import { Button } from '../components/ui/button';

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

/**
 * A keyboard-native collapsible disclosure (a real <details>/<summary>, so it
 * needs no extra ARIA wiring) whose open state is synced through React — the
 * console can open it (never hide a validation error) without fighting the
 * operator's own toggles.
 */
function DetailsDisclosure({
  testId,
  summary,
  open,
  onOpenChange,
  children,
}: {
  readonly testId: string;
  readonly summary: string;
  readonly open: boolean;
  readonly onOpenChange: (next: boolean) => void;
  readonly children: ReactNode;
}): JSX.Element {
  return (
    <details
      data-testid={testId}
      open={open}
      onToggle={(event): void => {
        onOpenChange(event.currentTarget.open);
      }}
      className="group rounded-md border p-3"
    >
      <summary
        className="flex min-h-12 cursor-pointer list-none items-center gap-1 text-sm font-medium [&::-webkit-details-marker]:hidden"
      >
        <ChevronRight
          className="size-4 shrink-0 transition-transform group-open:rotate-90"
          aria-hidden="true"
        />
        {summary}
      </summary>
      <div className="mt-3 flex flex-col gap-3">{children}</div>
    </details>
  );
}

/** The vertical pre-fader INPUT presence meter beside a strip's fader — REAL
 * `audio.meter` telemetry only. The bar is decorative; the numeric peak and
 * the Clip word carry the meaning. No level is ever derived from the draft. */
function InputMeterColumn({
  sample,
}: {
  readonly sample: AudioMeterSample | undefined;
}): JSX.Element {
  const { t } = useLingui();
  const peak = sample === undefined ? undefined : inputPeakDb(sample);
  return (
    <div className="flex shrink-0 flex-col items-center gap-1">
      <div
        aria-hidden="true"
        data-testid="audio-in-meter"
        className="relative h-60 w-3 overflow-hidden rounded bg-muted"
      >
        {peak !== undefined ? (
          <div
            className="absolute bottom-0 left-0 w-full bg-status-live"
            style={{ height: `${String(meterFillPercent(peak))}%` }}
          />
        ) : null}
      </div>
      <span className="text-xs text-muted-foreground">{t`IN`}</span>
      <span
        data-testid="audio-in-peak"
        className="font-mono text-xs tabular-nums text-muted-foreground"
      >
        {peak === undefined ? '—' : peak.toFixed(1)}
      </span>
      {sample?.clip ? (
        <span className="text-xs font-semibold text-destructive">
          <Trans>Clip</Trans>
        </span>
      ) : null}
      {peak === undefined ? (
        <span className="sr-only">
          <Trans>No input telemetry</Trans>
        </span>
      ) : null}
    </div>
  );
}

/**
 * One channel strip: the broadcast operating surface for one route. Renders
 * the DRAFT (what the operator is staging) with the SAVED row always explicit
 * beside it; every control writes the draft through `onChange` — no control
 * here ever sends a request. The advanced per-route fields stay reachable in
 * the Details disclosure.
 */
function ChannelStrip({
  route,
  saved,
  diff,
  index,
  label,
  errors,
  sourceIds,
  inputSample,
  detailsOpen,
  onDetailsOpenChange,
  onChange,
  onRemove,
  registerFader,
  idPrefix,
}: {
  readonly route: AudioRouteRow;
  /** The saved row associated with this local row id; absent for a new route. */
  readonly saved: AudioRouteRow | undefined;
  readonly diff: RouteDraftDiff | undefined;
  readonly index: number;
  readonly label: string;
  readonly errors: AudioFormErrors['routes'][number] | undefined;
  readonly sourceIds: readonly string[];
  readonly inputSample: AudioMeterSample | undefined;
  readonly detailsOpen: boolean;
  readonly onDetailsOpenChange: (index: number, next: boolean) => void;
  readonly onChange: (next: AudioRouteRow) => void;
  readonly onRemove: () => void;
  /** Registers the fader element (the xl bank wires overview taps to it). */
  readonly registerFader?: (element: HTMLInputElement | null) => void;
  /** Unique field-id prefix for this surface (bank vs focused). */
  readonly idPrefix: string;
}): JSX.Element {
  const { t } = useLingui();
  const row = index + 1;
  const track = route.targetTrack.trim();
  const draftDb = parseGainDb(route.gainDb);
  const savedDb = saved !== undefined ? parseGainDb(saved.gainDb) : undefined;
  const routeHasErrors = errors !== undefined && Object.keys(errors).length > 0;
  // The input picker offers the managed sources; a route bound to an id the
  // list does not carry (authored elsewhere / source deleted) stays selectable
  // so editing never silently rewrites it.
  const inputOptions =
    route.inputId !== '' && !sourceIds.includes(route.inputId)
      ? [route.inputId, ...sourceIds]
      : sourceIds;

  const setGain = (next: string): void => {
    onChange({ ...route, gainDb: next });
  };

  return (
    <section
      data-testid="audio-strip"
      aria-label={t`Channel strip — ${label}`}
      className="flex w-full min-w-0 flex-col gap-2 rounded-md border p-3"
    >
      <header className="flex min-w-0 flex-col gap-1">
        <span
          data-testid="audio-strip-name"
          className="truncate text-sm font-semibold"
        >
          {label}
        </span>
        {track !== '' ? (
          <Badge variant="secondary" className="w-fit font-mono">
            {track}
          </Badge>
        ) : null}
      </header>

      {/* Draft vs Saved — always explicit, never an apply claim. */}
      <p
        data-testid="audio-strip-readout"
        className="flex flex-wrap items-baseline gap-x-2 text-sm"
      >
        <span className="text-xs font-medium uppercase tracking-wide text-muted-foreground">
          <Trans>Draft</Trans>
        </span>
        {' '}
        <span className="font-mono text-base font-semibold tabular-nums">
          {draftDb === undefined ? <Trans>invalid gain</Trans> : <>{formatDb(draftDb)}{' '}<span className="text-xs font-normal text-muted-foreground">dB</span></>}
        </span>
        {savedDb !== undefined ? (
          <span className="text-xs text-muted-foreground">
            {' · '}
            <Trans>Saved</Trans>
            {' '}
            <span className="font-mono tabular-nums">{formatDb(savedDb)} dB</span>
          </span>
        ) : (
          <span className="text-xs text-muted-foreground">
            {' · '}
            <Trans>not saved yet</Trans>
          </span>
        )}
      </p>

      {/* At tablet widths, put the keys beside the full-height fader rather
          than below it. Keep source/focus order and the 48 px target floor. */}
      <div data-testid="audio-strip-essential" className="flex min-w-0 flex-col gap-2 md:max-xl:grid md:max-xl:grid-cols-2 md:max-xl:items-center">
      {/* The fader: ticks, knob and readout share ONE linear dB mapping; the
          48 px knob hit box travels a 240 px track; 0 dB is at the TOP. The
          range is a display bound only — the readout above carries the exact
          draft, and the Details field accepts any finite value. */}
      <div className="flex items-end justify-center gap-2 md:max-xl:row-span-2">
        <div className="relative h-60 w-7" aria-hidden="true">
          {GAIN_SCALE_TICKS_DB.map((tick) => (
            <span
              key={tick}
              className="absolute right-0 -translate-y-1/2 font-mono text-xs tabular-nums text-muted-foreground"
              style={{ top: `calc(24px + (100% - 48px) * ${String(1 - faderPos(tick))})` }}
            >
              {tick}
            </span>
          ))}
        </div>
        <div className="relative h-60 w-12">
          <input
            type="range"
            ref={registerFader}
            data-testid="audio-fader"
            min={GAIN_FADER_MIN_DB}
            max={GAIN_FADER_MAX_DB}
            step={1}
            value={faderDisplayDb(draftDb ?? 0)}
            disabled={draftDb === undefined}
            aria-label={t`${label} gain fader (0 dB at the top, −60 dB at the bottom)`}
            aria-valuetext={
              draftDb === undefined
                ? t`invalid gain; edit exact gain in Details`
                : savedDb !== undefined && savedDb !== draftDb
                  ? t`draft ${formatDb(draftDb)} dB, saved ${formatDb(savedDb)} dB`
                  : t`${formatDb(draftDb)} dB`
            }
            className="absolute left-1/2 top-1/2 h-12 w-60 max-w-none -translate-x-1/2 -translate-y-1/2 -rotate-90 cursor-pointer accent-primary"
            onChange={(event): void => {
              setGain(formatDb(Number(event.target.value)));
            }}
            onKeyDown={(event): void => {
              if (event.key === 'PageUp') {
                event.preventDefault();
                setGain(nudgeGainDb(route.gainDb, 6));
              } else if (event.key === 'PageDown') {
                event.preventDefault();
                setGain(nudgeGainDb(route.gainDb, -6));
              } else if (event.key === 'Escape' && saved !== undefined) {
                // Escape reverts the DRAFT gain to the saved one — a local,
                // reversible draft edit, never an on-air event undo.
                event.preventDefault();
                setGain(saved.gainDb);
              }
            }}
          />
        </div>
        <InputMeterColumn sample={inputSample} />
      </div>

      {/* The strip keys: MUTE and PGM (program-bus include) — draft toggles,
          pressed state visible by style AND text in the status line. */}
      <div className="flex flex-wrap items-center gap-2">
        <Button
          type="button"
          variant="outline"
          data-testid="audio-mute"
          aria-pressed={route.mute}
          aria-label={t`${label} MUTE`}
          className="h-12 min-w-12 px-4 aria-pressed:border-destructive/60 aria-pressed:bg-destructive/15 aria-pressed:text-destructive"
          onClick={(): void => {
            onChange({ ...route, mute: !route.mute });
          }}
        >
          {t`MUTE`}
        </Button>
        <Button
          type="button"
          variant="outline"
          data-testid="audio-pgm"
          aria-pressed={route.includeInProgramBus}
          aria-label={t`${label} PGM (include in the program mix)`}
          className="h-12 min-w-12 px-4 aria-pressed:border-status-live/60 aria-pressed:bg-status-live/15"
          onClick={(): void => {
            onChange({ ...route, includeInProgramBus: !route.includeInProgramBus });
          }}
        >
          {t`PGM`}
        </Button>
      </div>

      {/* ±1 / ±6 dB tap nudges (48 px), staged exactly like the drag. */}
      <div className="flex flex-wrap items-center gap-1" data-testid="audio-nudges">
        {([6, 1, -1, -6] as const).map((delta) => (
          <Button
            key={delta}
            type="button"
            variant="outline"
            data-testid="audio-nudge"
            aria-label={
              delta > 0
                ? t`${label} gain +${delta} dB`
                : t`${label} gain −${Math.abs(delta)} dB`
            }
            className="size-12 p-0 font-mono"
            disabled={draftDb === undefined}
            onClick={(): void => {
              setGain(nudgeGainDb(route.gainDb, delta));
            }}
          >
            {delta > 0 ? `+${String(delta)}` : `−${String(Math.abs(delta))}`}
          </Button>
        ))}
      </div>

      </div>

      {/* Reserved local Undo region: the strip-scoped draft revert appears in
          a fixed slot, so it can never move the keys or nudges (designer
          correction round 3). Only a LOCAL draft reversible — the saved row
          is restored as-is; nothing is sent. */}
      <div className="min-h-12">
        {diff?.changed === true && saved !== undefined ? (
          <Button
            type="button"
            variant="outline"
            data-testid="audio-strip-undo"
            aria-label={t`Undo draft edits for ${label}`}
            className="h-12 px-3 text-xs"
            onClick={(): void => {
              onChange(saved);
            }}
          >
            <Undo2 aria-hidden="true" />
            <Trans>Undo draft edits</Trans>
          </Button>
        ) : null}
      </div>

      <p data-testid="audio-strip-status" className="text-xs text-muted-foreground">
        {route.mute ? (
          <span className="font-semibold text-destructive">
            <Trans>MUTED</Trans>
          </span>
        ) : (
          <Trans>open</Trans>
        )}
        {' · '}
        {route.includeInProgramBus ? <Trans>in PGM</Trans> : <Trans>off PGM</Trans>}
        {track !== '' ? (
          <>
            {' · '}
            <span className="font-mono">{track}</span>
          </>
        ) : null}
      </p>

      {routeHasErrors ? (
        <p role="alert" className="text-sm text-destructive">
          <Trans>This channel has draft errors — they are marked inside Details.</Trans>
        </p>
      ) : null}

      <DetailsDisclosure
        testId="audio-strip-details"
        summary={t`Channel details`}
        open={detailsOpen}
        onOpenChange={(next): void => {
          onDetailsOpenChange(index, next);
        }}
      >
        {inputOptions.length > 0 ? (
          <SelectField<string>
            label={t`Input (route ${row})`}
            value={route.inputId}
            options={inputOptions}
            placeholder={t`Choose an input`}
            error={errors?.inputId}
            onChange={(next): void => {
              onChange({ ...route, inputId: next });
            }}
          />
        ) : (
          // No managed sources to pick from (fresh deployment): fall back to
          // a typed source id so routing can still be authored first.
          <FormField
            id={`${idPrefix}-input`}
            label={t`Input (route ${row})`}
            value={route.inputId}
            required
            placeholder={t`source id`}
            error={errors?.inputId}
            onChange={(next): void => {
              onChange({ ...route, inputId: next });
            }}
          />
        )}
        <SelectField<AudioChannelsKind>
          label={t`Channels (route ${row})`}
          value={route.channels}
          options={AUDIO_CHANNELS}
          optionLabel={channelsLabel}
          onChange={(next): void => {
            onChange({ ...route, channels: next });
          }}
        />
        <FormField
          id={`${idPrefix}-gain`}
          label={t`Gain in dB (route ${row})`}
          value={route.gainDb}
          placeholder="0"
          error={errors?.gainDb}
          hint={
            <Trans>
              The exact program-bus gain for this input (any finite value;
              the strip fader covers −60…0 dB).
            </Trans>
          }
          onChange={(next): void => {
            setGain(next);
          }}
        />
        <FormField
          id={`${idPrefix}-track`}
          label={t`Discrete track name (route ${row})`}
          value={route.targetTrack}
          placeholder={t`e.g. cam1-clean`}
          error={errors?.targetTrack}
          onChange={(next): void => {
            onChange({ ...route, targetTrack: next });
          }}
        />
        <FormField
          id={`${idPrefix}-language`}
          label={t`Language (route ${row})`}
          value={route.language}
          placeholder="eng"
          onChange={(next): void => {
            onChange({ ...route, language: next });
          }}
        />
        <FormField
          id={`${idPrefix}-title`}
          label={t`Track title (route ${row})`}
          value={route.title}
          placeholder={t`Camera 1`}
          onChange={(next): void => {
            onChange({ ...route, title: next });
          }}
        />
        <div>
          <Button
            type="button"
            variant="ghost"
            size="icon"
            data-testid="audio-remove-route"
            aria-label={t`Remove route ${row}`}
            className="size-12"
            onClick={onRemove}
          >
            <Trash2 aria-hidden="true" />
          </Button>
        </div>
      </DetailsDisclosure>
    </section>
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

/** One whole-mix overview row (SAVED state + real input meter + draft mark). */
function OverviewRow({
  draft,
  saved,
  changed,
  label,
  sample,
  onRide,
}: {
  readonly draft: AudioRouteRow;
  readonly saved: AudioRouteRow | undefined;
  readonly changed: boolean;
  readonly label: string;
  readonly sample: AudioMeterSample | undefined;
  readonly onRide: () => void;
}): JSX.Element {
  const { t } = useLingui();
  const peak = sample === undefined ? undefined : inputPeakDb(sample);
  return (
    <li>
      <button
        type="button"
        data-testid="audio-overview-row"
        aria-label={t`Ride ${label}`}
        onClick={onRide}
        className="flex min-h-12 w-full flex-wrap items-center gap-x-3 gap-y-1 rounded-md border p-2 text-left transition-colors hover:bg-accent focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
      >
        <span className="w-32 shrink-0 truncate text-sm font-medium">{label}</span>
        {draft.targetTrack.trim() !== '' ? (
          <span className="shrink-0 rounded bg-secondary px-1.5 font-mono text-xs text-secondary-foreground">
            {draft.targetTrack.trim()}
          </span>
        ) : null}
        <span className="flex shrink-0 items-center gap-1">
          <span
            aria-hidden="true"
            data-testid="audio-overview-meter"
            className="h-2 w-20 overflow-hidden rounded bg-muted"
          >
            {peak !== undefined ? (
              <span
                className="block h-full bg-status-live"
                style={{ width: `${String(meterFillPercent(peak))}%` }}
              />
            ) : null}
          </span>
          <span className="w-10 font-mono text-xs tabular-nums text-muted-foreground">
            {peak === undefined ? '—' : peak.toFixed(1)}
          </span>
          {peak === undefined ? (
            <span className="sr-only">
              <Trans>No input telemetry</Trans>
            </span>
          ) : null}
        </span>
        {saved === undefined ? (
          <span className="text-xs text-muted-foreground">
            <Trans>new</Trans>
          </span>
        ) : (
          <>
            <span className="font-mono text-xs tabular-nums text-muted-foreground">
              {formatDb(parseGainDb(saved.gainDb) ?? 0)} dB
            </span>
            {saved.mute ? (
              <span className="text-xs font-semibold text-destructive">
                <Trans>MUTED</Trans>
              </span>
            ) : null}
            <span className="text-xs text-muted-foreground">
              {saved.includeInProgramBus ? t`in PGM` : t`off PGM`}
            </span>
          </>
        )}
        {changed ? (
          <span
            data-testid="audio-draft-mark"
            className="rounded border border-status-stale/60 bg-status-stale/15 px-1.5 text-xs font-medium"
          >
            <Trans>edited</Trans>
          </span>
        ) : null}
      </button>
    </li>
  );
}

/** The whole-mix overview: the pinned PROGRAM row + one row per route, each
 * showing the SAVED state, a REAL pre-fader input meter, and an edited mark
 * while the operator carries a draft. Tap a row to ride that channel. */
function MixOverview({
  form,
  savedRows,
  rowIds,
  diff,
  sourceNames,
  inputs,
  onRide,
}: {
  readonly form: AudioFormState;
  readonly savedRows: readonly (AudioRouteRow | undefined)[];
  readonly rowIds: readonly number[];
  readonly diff: ReturnType<typeof documentDraftDiff> | undefined;
  readonly sourceNames: ReadonlyMap<string, string>;
  readonly inputs: readonly AudioMeterSample[];
  readonly onRide: (index: number) => void;
}): JSX.Element {
  const { t } = useLingui();
  const included = form.routes.filter((route) => route.includeInProgramBus).length;
  const muted = form.routes.filter((route) => route.mute).length;
  return (
    <section
      aria-labelledby="audio-mix-heading"
      data-testid="audio-mix-overview"
      className="rounded-md border p-4"
    >
      <h2 id="audio-mix-heading" className="text-sm font-medium">
        <Trans>Whole mix</Trans>
      </h2>
      <p className="mt-1 text-xs text-muted-foreground">
        <Trans>
          Saved state with real input meters (pre-fader). Tap a channel to
          ride its strip.
        </Trans>
      </p>
      <ul className="mt-3 flex flex-col gap-1">
        <li
          data-testid="audio-overview-program"
          className="flex flex-wrap items-center gap-x-3 gap-y-1 rounded-md border bg-card p-2 text-sm"
        >
          <span className="font-semibold">
            <Trans>PROGRAM</Trans>
          </span>
          <span className="text-xs text-muted-foreground">
            <Trans>the mixed program bus, always available</Trans>
          </span>
          <span className="text-xs text-muted-foreground">
            <Trans>
              inputs on the mix: {included} of {form.routes.length} · muted:{' '}
              {muted}
            </Trans>
          </span>
          <span className="text-xs text-muted-foreground">
            <Trans>Live program meters are rendered below the console.</Trans>
          </span>
        </li>
        {form.routes.length === 0 ? (
          <li className="p-2 text-sm text-muted-foreground">
            <Trans>
              No routes yet — without routes the multiview carries no managed
              audio.
            </Trans>
          </li>
        ) : (
          form.routes.map((route, index) => (
            <OverviewRow
              key={rowIds[index]}
              draft={route}
              saved={savedRows[index]}
              changed={diff?.routes[index]?.changed === true}
              label={
                route.inputId.trim() === ''
                  ? t`Route ${index + 1}`
                  : sourceNames.get(route.inputId.trim()) ?? route.inputId.trim()
              }
              sample={inputMeterSample(inputs, route.inputId.trim())}
              onRide={(): void => {
                onRide(index);
              }}
            />
          ))
        )}
      </ul>
    </section>
  );
}

/** Audio routing management (the document the per-output selections resolve
 * against), rendered as the broadcast console the designer direction
 * approved: whole-mix overview, channel strips (focused below xl, bank at
 * xl+), one document-level Save. */
export function AudioPage(): JSX.Element {
  const { t } = useLingui();
  const sources = useSources();
  const routing = useAudioRouting();
  const outputSelections = useOutputAudioSelections();
  const save = useSaveAudioRouting();
  // The real pre-fader input meters (engine `audio.meter` telemetry) — one
  // subscription feeds the overview rows and every strip's IN column.
  const meter = useAudioMeter();
  const loudness = useAudioLoudness();

  // Before the first edit the form follows the query. Once edited, the draft
  // and its original document/ETag stay bound through query invalidations;
  // a successful save replaces both with the canonical server response.
  const [edited, setEdited] = useState<AudioFormState | undefined>(undefined);
  // The draft and its If-Match baseline are one snapshot. A query refresh may
  // never lend a newer ETag to an older whole-document draft.
  const [baseline, setBaseline] = useState<{
    form: AudioFormState;
    etag: string | undefined;
    rowIds: readonly number[];
    previousEtag?: string | undefined;
  } | undefined>(undefined);
  const nextRowId = useRef(0);
  // Also guards events before React commits the disabled fieldset.
  const saveLocked = useRef(false);
  const [draftRowIds, setDraftRowIds] = useState<readonly number[] | undefined>();
  const [conflicted, setConflicted] = useState(false);
  const [errors, setErrors] = useState<AudioFormErrors | undefined>(undefined);
  const [status, setStatus] = useState<'idle' | 'live' | 'partial' | 'saved' | 'error'>('idle');
  const [saveError, setSaveError] = useState<string | undefined>(undefined);
  const [outputsUpdated, setOutputsUpdated] = useState<
    readonly OutputsUpdatedEntry[]
  >([]);
  // Console state: the sub-xl strip selection, which strip Details are open,
  // and whether the Bus-setup disclosure is open (they auto-open when a
  // validation failure would otherwise hide inside them).
  const [focusedIndex, setFocusedIndex] = useState(0);
  const [openStrips, setOpenStrips] = useState<ReadonlySet<number>>(() => new Set());
  const [busSetupOpen, setBusSetupOpen] = useState(false);
  // The xl bank's faders, so an overview tap can hand keyboard focus to the
  // channel it rides (below xl the focused strip follows the tap instead).
  const bankFaders = useRef<Map<number, HTMLInputElement>>(new Map());

  const form =
    edited ??
    (routing.data !== undefined
      ? audioFormFromDocument(routing.data.state.routing)
      : undefined);
  const serverForm = baseline?.form ?? (routing.data !== undefined
    ? audioFormFromDocument(routing.data.state.routing) : undefined);
  const rowIds = draftRowIds ?? form?.routes.map((_, index) => index) ?? [];
  const savedRowFor = (index: number): AudioRouteRow | undefined => {
    const id = rowIds[index];
    const savedIndex = baseline?.rowIds.indexOf(id ?? -1) ?? index;
    return savedIndex < 0 ? undefined : serverForm?.routes[savedIndex];
  };
  const queryDiverged = baseline !== undefined && routing.data !== undefined &&
    routing.data.etag !== baseline.etag && routing.data.etag !== baseline.previousEtag;
  const savedIndices = rowIds.map((id) => {
    const index = baseline?.rowIds.indexOf(id) ?? id;
    return index < 0 ? undefined : index;
  });

  const sourceIds = (sources.data ?? []).map((source) => source.id);
  const sourceNames = new Map(
    (sources.data ?? []).map((source) => [source.id, source.name] as const),
  );

  // The captured saved document, not a separately refreshed query document,
  // drives both pending impact and Draft/Saved comparisons.
  const pendingImpacts =
    form !== undefined && serverForm !== undefined && outputSelections.data !== undefined
      ? pendingTrackImpacts(serverForm, form, outputSelections.data)
      : [];
  const draftDiff =
    form !== undefined && serverForm !== undefined
      ? documentDraftDiff(serverForm, form, savedIndices)
      : undefined;

  const update = (next: AudioFormState, nextIds: readonly number[] = rowIds): void => {
    if (saveLocked.current) return;
    if (baseline === undefined && routing.data !== undefined && serverForm !== undefined) {
      setBaseline({ form: serverForm, etag: routing.data.etag, rowIds });
      nextRowId.current = Math.max(nextRowId.current, ...rowIds.map((id) => id + 1));
    }
    setDraftRowIds(nextIds);
    setEdited(next);
    setStatus('idle');
    setOutputsUpdated([]);
    if (errors !== undefined) {
      setErrors(validateAudioForm(next));
    }
  };

  const setStripDetailsOpen = (index: number, next: boolean): void => {
    setOpenStrips((prev): ReadonlySet<number> => {
      const updated = new Set(prev);
      if (next) {
        updated.add(index);
      } else {
        updated.delete(index);
      }
      return updated;
    });
  };

  /** Ride one channel: focus its bank fader on xl+, otherwise the focused
   * strip follows the tap. */
  const ride = (index: number): void => {
    setFocusedIndex(index);
    bankFaders.current.get(index)?.focus();
  };

  // The focused strip selection, clamped to the routes the draft carries (a
  // removed last route must not leave a dangling selection).
  const focused =
    form === undefined
      ? 0
      : Math.min(focusedIndex, Math.max(0, form.routes.length - 1));
  const focusedRoute = form === undefined ? undefined : form.routes[focused];
  const [overviewOpen, setOverviewOpen] = useState(false);
  const [bankExpanded, setBankExpanded] = useState(true);

  /** The strip's display label: the managed source's name when known, the
   * raw input id otherwise, and a positional label for an unwritten route. */
  const labelFor = (route: AudioRouteRow, row: number): string => {
    const id = route.inputId.trim();
    if (id === '') {
      return t`Route ${row}`;
    }
    return sourceNames.get(id) ?? id;
  };

  const onSave = (): void => {
    if (saveLocked.current || form === undefined || conflicted || queryDiverged) {
      return;
    }
    const validation = validateAudioForm(form);
    if (validation.hasErrors) {
      setErrors(validation);
      // Never hide a validation error inside a closed disclosure: open the
      // bus setup when the sample rate is bad, and every strip with errors.
      if (validation.sampleRateHz !== undefined) {
        setBusSetupOpen(true);
      }
      setOpenStrips((prev): ReadonlySet<number> => {
        const updated = new Set(prev);
        validation.routes.forEach((routeErrors, index) => {
          if (Object.keys(routeErrors).length > 0) {
            updated.add(index);
          }
        });
        return updated;
      });
      return;
    }
    setErrors(undefined);
    setStatus('idle');
    saveLocked.current = true;
    const submitted = form;
    save.mutate(
      { document: audioFormToDocument(submitted), etag: baseline?.etag ?? routing.data?.etag },
      {
        onSuccess: (result): void => {
          saveLocked.current = false;
          const canonical = audioFormFromDocument(result.state.routing);
          const canonicalIds = canonical.routes.map((_, index) => rowIds[index] ?? nextRowId.current++);
          setBaseline({ form: canonical, etag: result.etag, rowIds: canonicalIds, previousEtag: baseline?.etag ?? routing.data?.etag });
          setDraftRowIds(canonicalIds);
          setEdited(canonical);
          setConflicted(false);
          setStatus(result.apply === 'live' ? 'live' : result.apply === 'partial' ? 'partial' : 'saved');
          setSaveError(undefined);
          setOutputsUpdated(result.state.outputs_updated ?? []);
        },
        onError: (error): void => {
          saveLocked.current = false;
          setStatus('error');
          if (error.status === 412) {
            setConflicted(true);
          }
          setSaveError(error.message);
        },
      },
    );
  };

  return (
    <AudioSurface
      loudness={loudness}
      sub={
        <>
          <Trans>
            Program bus and discrete tracks — edits stage a draft; Save audio
            routing stores the whole document.
          </Trans>
          <HelpLink to="/help/features#outputs" label={t`About outputs and audio`} />
        </>
      }
    >
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
        <fieldset disabled={save.isPending} aria-busy={save.isPending} className="flex min-w-0 flex-col gap-3">
          {errors?.program !== undefined ? (
            <p role="alert" className="text-sm text-destructive">
              <FieldErrorMessage code={errors.program} />
            </p>
          ) : null}

          {/* Touch-first surface (below xl): pinned chip picker + ONE
              focused strip — every channel stays one tap away in the
              overview below (designer round 4). */}
          <AudioPhoneMaster loudness={loudness} />
          {form.routes.length > 0 ? (
            <div
              role="group"
              aria-label={t`Channel picker — tap a channel to ride its strip`}
              data-testid="audio-strip-picker"
              className="flex flex-wrap gap-2 xl:hidden"
            >
              {form.routes.map((route, index) => (
                <Button
                  key={rowIds[index]}
                  type="button"
                  variant="outline"
                  data-testid="audio-strip-chip"
                  aria-pressed={index === focused}
                  className="h-auto min-h-12 min-w-12 max-w-full whitespace-normal break-words px-4 aria-pressed:bg-secondary aria-pressed:font-semibold"
                  onClick={(): void => {
                    ride(index);
                  }}
                >
                  {labelFor(route, index + 1)}
                </Button>
              ))}
            </div>
          ) : null}

          {/* The console's strip surfaces render in parallel: the bank shows
              only at xl+, the focused strip only below — the SAME strip
              component, the same draft, the same honest readouts. */}
          <AudioMixerLayout
            bank={
          <section
            aria-labelledby="audio-bank-heading"
            data-testid="audio-bank"
            className="hidden xl:block"
          >
            <h3 id="audio-bank-heading" className="mb-3 text-sm font-medium">
              <Trans>Mix bank</Trans>
            </h3>
            {form.routes.length > 1 ? (
              <Button type="button" variant="outline" className="mb-3 h-auto min-h-12 min-w-12 max-w-full whitespace-normal break-words" aria-expanded={bankExpanded} onClick={(): void => { setBankExpanded(!bankExpanded); }}>
                {bankExpanded ? <Trans>Show selected channel</Trans> : <Trans>Show all channels</Trans>}
              </Button>
            ) : null}
            {form.routes.length === 0 ? (
              <p className="text-sm text-muted-foreground">
                <Trans>
                  No routes yet — without routes the multiview carries no
                  managed audio.
                </Trans>
              </p>
            ) : (
              <div className="flex max-w-full gap-2 overflow-x-auto pb-2">
                {form.routes.map((route, index) => (
                  <div key={rowIds[index]} className={bankExpanded || index === focused ? 'w-44 shrink-0' : 'hidden'}>
                    <ChannelStrip
                      route={route}
                      saved={savedRowFor(index)}
                      diff={draftDiff?.routes[index]}
                      index={index}
                      label={labelFor(route, index + 1)}
                      errors={errors?.routes[index]}
                      sourceIds={sourceIds}
                      inputSample={inputMeterSample(meter.inputs, route.inputId.trim())}
                      detailsOpen={openStrips.has(index)}
                      onDetailsOpenChange={setStripDetailsOpen}
                      idPrefix={`audio-bank-route-${String(index + 1)}`}
                      registerFader={(element): void => {
                        if (element !== null) {
                          bankFaders.current.set(index, element);
                        } else {
                          bankFaders.current.delete(index);
                        }
                      }}
                      onChange={(next): void => {
                        update(withRoute(form, index, next));
                      }}
                      onRemove={(): void => {
                        update({
                          ...form,
                          routes: form.routes.filter((_, i) => i !== index),
                        }, rowIds.filter((_, i) => i !== index));
                      }}
                    />
                  </div>
                ))}
              </div>
            )}
          </section>
            }
            focused={
          focusedRoute !== undefined ? (
            <section
              aria-labelledby="audio-focused-heading"
              data-testid="audio-focused-strip"
              className="flex flex-col gap-3 xl:hidden"
            >
              <h3 id="audio-focused-heading" className="sr-only">
                <Trans>Channel strip</Trans>
              </h3>
              <ChannelStrip
                route={focusedRoute}
                saved={savedRowFor(focused)}
                diff={draftDiff?.routes[focused]}
                index={focused}
                label={labelFor(focusedRoute, focused + 1)}
                errors={errors?.routes[focused]}
                sourceIds={sourceIds}
                inputSample={inputMeterSample(meter.inputs, focusedRoute.inputId.trim())}
                detailsOpen={openStrips.has(focused)}
                onDetailsOpenChange={setStripDetailsOpen}
                idPrefix={`audio-focused-route-${String(focused + 1)}`}
                onChange={(next): void => {
                  update(withRoute(form, focused, next));
                }}
                onRemove={(): void => {
                  update({
                    ...form,
                    routes: form.routes.filter((_, i) => i !== focused),
                  }, rowIds.filter((_, i) => i !== focused));
                }}
              />
            </section>
          ) : null
            }
            rail={
          <>
          <div className="xl:hidden">
            <Button type="button" variant="outline" className="h-auto min-h-12 min-w-12 max-w-full whitespace-normal break-words" aria-expanded={overviewOpen} onClick={(): void => { setOverviewOpen(!overviewOpen); }}>
              <Trans>Whole mix overview</Trans>
            </Button>
          </div>
          <div className={overviewOpen ? '' : 'hidden xl:block'}>
          <MixOverview
            form={form}
            savedRows={form.routes.map((_, index) => savedRowFor(index))}
            rowIds={rowIds}
            diff={draftDiff}
            sourceNames={sourceNames}
            inputs={meter.inputs}
            onRide={ride}
          />
          </div>
          <AudioMasterRegion />
          </>
            }
          />

          {/* The draft banner: what is pending, in the operator's words —
              never a claim that anything is applied. */}
          {draftDiff?.anyChanged ? (
            <p
              data-testid="audio-draft-summary"
              className="rounded-md border border-status-stale/60 bg-status-stale/10 p-3 text-sm"
            >
              {draftDiff.sampleRateChanged && draftDiff.changedRouteCount > 0 ? (
                <Trans>
                  Draft edits pending — channels changed:{' '}
                  {draftDiff.changedRouteCount}; the sample rate changed too.
                  Nothing reaches the bus until you save.
                </Trans>
              ) : draftDiff.sampleRateChanged ? (
                <Trans>
                  Draft edits pending — the sample rate changed. Nothing
                  reaches the bus until you save.
                </Trans>
              ) : (
                <Trans>
                  Draft edits pending — channels changed:{' '}
                  {draftDiff.changedRouteCount}. Nothing reaches the bus
                  until you save.
                </Trans>
              )}
            </p>
          ) : null}

          <PendingTrackWarning impacts={pendingImpacts} />

          <div className="order-first sticky top-0 z-10 flex flex-wrap items-center gap-2 rounded-md border bg-background/95 p-2 shadow-sm">
            <Button
              type="button"
              variant="outline"
              data-testid="audio-add-route"
              className="h-auto min-h-12 min-w-12 max-w-full whitespace-normal break-words px-4"
              onClick={(): void => {
                update({ ...form, routes: [...form.routes, emptyAudioRoute()] }, [...rowIds, Math.max(nextRowId.current++, ...rowIds.map((id) => id + 1))]);
              }}
            >
              <Plus aria-hidden="true" />
              <Trans>Add route</Trans>
            </Button>
            <Button
              type="button"
              data-testid="audio-save"
              className="h-auto min-h-12 min-w-12 max-w-full whitespace-normal break-words px-6"
              disabled={save.isPending || conflicted || queryDiverged}
              onClick={onSave}
            >
              <Trans>Save audio routing</Trans>
            </Button>
            {(conflicted || queryDiverged) ? (
              <div role="alert" className="flex flex-wrap items-center gap-2 text-sm text-destructive">
                <Trans>Audio routing changed on the server. Your draft is preserved; review the latest version before saving again.</Trans>
                <Button type="button" variant="outline" className="h-auto min-h-12 min-w-12 max-w-full whitespace-normal break-words" onClick={(): void => {
                  if (routing.data === undefined) return;
                  const latest = audioFormFromDocument(routing.data.state.routing);
                  setBaseline({ form: latest, etag: routing.data.etag, rowIds: latest.routes.map((_, index) => index) });
                  setEdited(latest);
                  setDraftRowIds(latest.routes.map((_, index) => index));
                  nextRowId.current = latest.routes.length;
                  setConflicted(false);
                  setSaveError(undefined);
                  setStatus('idle');
                }}><Trans>Reload latest routing (discard draft)</Trans></Button>
              </div>
            ) : null}
            <p role="status" aria-live="polite" className="text-sm">
              {save.isPending ? <Trans>Saving audio routing. Draft controls are locked until the save completes.</Trans> : null}
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
          {/* Secondary context follows the operating surface, not the fader. */}
          <div data-testid="audio-page-context" className="flex min-w-0 flex-col gap-3 text-sm text-muted-foreground">
            <p className="flex flex-wrap items-center gap-x-3 gap-y-1">
              <Trans>
                Route each input's audio: program-mix membership and discrete tracks.
              </Trans>
              <HelpLink to="/help/features#outputs" label={t`About outputs and audio`} />
            </p>
            <div className="max-w-full min-w-0 [&_button]:min-h-12 [&_button]:max-w-full [&_button]:whitespace-normal [&_button]:break-words"><ExportConfigButton compact /></div>
          </div>
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
          <OutputsUpdatedNote entries={outputsUpdated} />

          {/* Bus setup: the document-level configuration out of the operating
              fold (sample rate), plus the honest monitoring note — no fake
              PFL/listen key exists anywhere on the console. */}
          <DetailsDisclosure
            testId="audio-bus-setup"
            summary={t`Bus setup`}
            open={busSetupOpen}
            onOpenChange={setBusSetupOpen}
          >
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
            <p data-testid="audio-pfl-note" className="max-w-md text-xs text-muted-foreground">
              <Trans>
                Pre-fade listen (PFL / solo monitoring) is not available on
                this control plane — there is no monitoring path to configure,
                so the console carries no listen key.
              </Trans>
            </p>
          </DetailsDisclosure>

          <SelectableTracks form={form} />

          {/* Live program-bus meters (AUD-8): subscribes to the engine's
              conflated `audio.loudness` realtime topic and renders
              M/S/I/LRA/dBTP against the compliance target, ballistics applied
              client-side. UI-only — it consumes telemetry, never the engine
              path (invariant #10). */}
          <ProgramMeters />
          <LoudnessMeter />
        </fieldset>
      )}
    </AudioSurface>
  );
}
