// Sources — kind-specific typed management of the ingest inputs.
//
// Every form field maps 1:1 onto the config `Source` schema
// (crates/multiview-config/src/schema.rs) via the pure mapping in
// ../resources/forms (unit-tested); the control plane 422s anything else
// (ADR-W015). The LIVE STATUS column reads the realtime tile map the WebSocket
// hook mirrors into the Query cache; a source outside the running engine shows
// "—" with an explanation, because honesty beats a fabricated state.
import { M3uImportButton } from './M3uImportDialog';
import { useState } from 'react';
import type { JSX } from 'react';
import { Link, useLocation } from 'react-router-dom';
import { Trans, useLingui } from '@lingui/react/macro';

import { useSources } from '../resources/queries';
import type { SaveResourceVars } from '../resources/queries';
import type { SourceView } from '../resources/types';
import { SOURCE_KINDS } from '../resources/types';
import {
  captionModesForValue,
  CLOCK_FACES,
  emptySourceForm,
  EPG_MODES,
  IANA_TIMEZONES,
  PIN_VENDORS,
  sourceFormFromRecord,
  sourceFormToBody,
  sourceKindHasUrl,
  TIMER_DIRECTIONS,
  TIMER_FORMATS,
  TIMER_ON_TARGETS,
  TIMER_TARGET_TYPES,
  validateSourceForm,
  withSourceKind,
  withTimerTargetType,
} from '../resources/forms';
import type {
  CaptionsMode,
  ClockFace,
  EpgModeChoice,
  FieldErrors,
  PinVendor,
  RtspTransport,
  SourceField,
  SourceFormKind,
  SourceFormState,
  TimerDirection,
  TimerFormat,
  TimerOnTarget,
  TimerTargetType,
  SourceActivationChoice,
  WallClockChoice,
} from '../resources/forms';
import { CrudPage } from '../resources/CrudPage';
import {
  AdvancedSection,
  ApplySemanticsCallout,
  CheckboxField,
  DerivedUrlField,
  ExportConfigButton,
  FormField,
  SecretField,
  SelectField,
} from '../resources/FormControls';
import { whipPublishUrl } from '../resources/api';
import type { CrudSeed } from '../resources/CrudPage';
import { SourceFromDeviceSection } from '../devices/FromDevice';
import { tileForSource, useLiveTiles } from '../resources/useLiveTiles';
import type { LiveTile } from '../realtime/useEngineEvents';
import { Button } from '../components/ui/button';
import { useCatalogueLinks } from '../api/catalogueQueries';
import { OriginPanel } from '../iptv/SourceLinks';
import type { SourceFormState as SeedForm } from '../resources/forms';
import { HelpLink } from '../components/HelpLink';
import { useSourceStatus } from '../api/sourceStatus';
import { useSystemCapabilities } from '../api/systemQueries';
import { Info, TriangleAlert } from 'lucide-react';
import { SourcesSurface } from './approved/SourcesSurface';

/** The form-kind options offered in the picker (no legacy alias). */
const FORM_KINDS: readonly SourceFormKind[] = SOURCE_KINDS.filter(
  (kind): kind is SourceFormKind => kind !== 'test',
);

// Radix Select forbids an empty item value, so the "engine default" choice
// rides a sentinel and maps onto the form's '' (= omit the rtsp block).
type RtspTransportChoice = 'auto' | 'tcp' | 'udp';
const RTSP_TRANSPORT_CHOICES: readonly RtspTransportChoice[] = ['auto', 'tcp', 'udp'];

function toTransportChoice(value: RtspTransport): RtspTransportChoice {
  return value === '' ? 'auto' : value;
}

function fromTransportChoice(value: RtspTransportChoice): RtspTransport {
  return value === 'auto' ? '' : value;
}
const WALLCLOCK_CHOICES: readonly WallClockChoice[] = ['default', 'use', 'discard'];
const ACTIVATION_CHOICES: readonly SourceActivationChoice[] = ['always_on', 'on_demand'];

/**
 * The `youtube` source kind's live-only note + VT redirect, plus an honest
 * capability warning when this server cannot actually resolve a YouTube URL
 * right now (the `youtube` build feature off, or `yt-dlp` not found on the
 * server). Rendered under the URL field so an operator sees both the scope
 * limitation and the current server capability before saving.
 */
function YoutubeKindNote(): JSX.Element {
  const { data: capabilities } = useSystemCapabilities();
  const youtube = capabilities?.youtube ?? undefined;
  return (
    <div className="flex flex-col gap-2">
      <p className="flex items-start gap-2 text-xs text-muted-foreground">
        <Info className="mt-0.5 size-3.5 shrink-0" aria-hidden="true" />
        <span>
          <Trans>
            Live streams only: this resolves a currently-broadcasting YouTube
            URL, never a pre-recorded/past video. To play a pre-recorded video,
            use a VT clip instead (not built yet).
          </Trans>
        </span>
      </p>
      {youtube !== undefined && !youtube.available ? (
        <p
          role="note"
          className="flex items-start gap-2 rounded-md border border-status-stale/40 bg-status-stale/15 p-2 text-xs"
        >
          <TriangleAlert className="mt-0.5 size-3.5 shrink-0" aria-hidden="true" />
          <span>
            {youtube.compiled ? (
              <Trans>
                This server cannot resolve YouTube sources right now: `yt-dlp`
                was not found. A saved source will stay `NO_SIGNAL` until the
                server operator installs it.
              </Trans>
            ) : (
              <Trans>
                This server build does not include YouTube live ingest. A
                saved source will stay `NO_SIGNAL` until it is rebuilt with
                the `youtube` feature.
              </Trans>
            )}
          </span>
        </p>
      ) : null}
    </div>
  );
}

/** The kind-specific locator + parameter fields. */
function SourceKindFields({
  form,
  setForm,
  errors,
}: {
  readonly form: SourceFormState;
  readonly setForm: (next: SourceFormState) => void;
  readonly errors: FieldErrors<SourceField>;
}): JSX.Element | null {
  const { t } = useLingui();
  if (sourceKindHasUrl(form.kind)) {
    const placeholder = ((): string => {
      switch (form.kind) {
        case 'rtsp':
          return 'rtsp://[2001:db8::1]:8554/stream';
        case 'hls':
          return 'https://example.com/live/master.m3u8';
        case 'youtube':
          return 'https://www.youtube.com/watch?v=…';
        case 'srt':
          return 'srt://[2001:db8::2]:7001';
        case 'rist':
          return 'rist://[2001:db8::2]:5000';
        case 'rtmp':
          return 'rtmp://ingest.example/app/key';
        default:
          return 'udp://[ff3e::1]:5004';
      }
    })();
    return (
      <>
        <FormField
          id="source-url"
          label={form.kind === 'youtube' ? t`Watch / live / channel URL` : t`Source URL`}
          value={form.url}
          required
          placeholder={placeholder}
          {...(form.kind === 'srt'
            ? {
                hint: t`To receive from a remote encoder, use srt://[::]:7001?mode=listener: one socket accepts an IPv6 or IPv4 caller, one at a time. 0.0.0.0 is IPv4 only.`,
              }
            : {})}
          error={errors.url}
          onChange={(next): void => {
            setForm({ ...form, url: next });
          }}
        />
        {form.kind === 'youtube' ? <YoutubeKindNote /> : null}
        {form.kind === 'rtsp' ? (
          <SelectField<RtspTransportChoice>
            label={t`RTSP transport`}
            value={toTransportChoice(form.rtspTransport)}
            options={RTSP_TRANSPORT_CHOICES}
            optionLabel={(option): JSX.Element =>
              option === 'auto' ? <Trans>Automatic (default)</Trans> : <>{option}</>
            }
            onChange={(next): void => {
              setForm({ ...form, rtspTransport: fromTransportChoice(next) });
            }}
          />
        ) : null}
      </>
    );
  }
  switch (form.kind) {
    case 'ndi':
      return (
        <FormField
          id="source-ndi-name"
          label={t`NDI source name`}
          value={form.ndiName}
          required
          placeholder={t`STUDIO (CAM 1)`}
          error={errors.ndiName}
          onChange={(next): void => {
            setForm({ ...form, ndiName: next });
          }}
        />
      );
    case 'file':
      return (
        <FormField
          id="source-path"
          label={t`File path`}
          value={form.path}
          required
          placeholder="/media/clip.mp4"
          error={errors.path}
          onChange={(next): void => {
            setForm({ ...form, path: next });
          }}
        />
      );
    case 'solid':
      return (
        <FormField
          id="source-color"
          label={t`Fill colour`}
          value={form.color}
          required
          placeholder="#101014"
          error={errors.color}
          onChange={(next): void => {
            setForm({ ...form, color: next });
          }}
        />
      );
    case 'clock':
      return <ClockKindFields form={form} setForm={setForm} errors={errors} />;
    case 'timer':
      return <TimerKindFields form={form} setForm={setForm} errors={errors} />;
    case 'webrtc':
      return <WebrtcSourceFields form={form} setForm={setForm} />;
    default:
      // `bars` carries only its kind tag.
      return null;
  }
}

/**
 * The WHIP-ingest (`webrtc`) source fields (ADR-T014 / ADR-W023). There is no
 * URL to author — Multiview is the WHIP server, so the form shows the DERIVED
 * publish endpoint (read-only, copyable) to hand a publisher, plus the optional
 * publisher token (masked) and the accept-audio toggle.
 */
function WebrtcSourceFields({
  form,
  setForm,
}: {
  readonly form: SourceFormState;
  readonly setForm: (next: SourceFormState) => void;
}): JSX.Element {
  const { t } = useLingui();
  const id = form.id.trim();
  return (
    <>
      {id === '' ? (
        <p className="text-xs text-muted-foreground">
          <Trans>
            Set an identifier above; the WHIP publish endpoint is derived from
            it and shown here once it is filled in.
          </Trans>
        </p>
      ) : (
        <DerivedUrlField
          id="source-whip-url"
          label={t`WHIP publish endpoint`}
          value={whipPublishUrl(id)}
          hint={
            <Trans>
              Give this to a publisher (OBS ≥ 30, GStreamer whipclientsink, or a
              browser) to push media into this source. Relative to the control
              plane origin.
            </Trans>
          }
          trailing={
            <HelpLink
              to="/help/concepts/glossary#whip"
              label={t`What is WHIP?`}
              compact
            />
          }
        />
      )}
      <SecretField
        id="source-webrtc-token"
        label={t`Publisher token (optional)`}
        value={form.webrtcToken}
        placeholder={t`leave blank to require a Write API key`}
        hint={
          <Trans>
            A bearer token the publisher presents on the WHIP POST. Leave blank
            to require a control-plane API key with Write scope instead —
            publishing is never anonymous.
          </Trans>
        }
        onChange={(next): void => {
          setForm({ ...form, webrtcToken: next });
        }}
      />
      <CheckboxField
        id="source-webrtc-audio"
        label={t`Accept the publisher's audio (Opus)`}
        checked={form.webrtcAudio}
        onChange={(next): void => {
          setForm({ ...form, webrtcAudio: next });
        }}
      />
    </>
  );
}

/** Translate a clock face value to its localized option label. */
function clockFaceLabel(face: ClockFace): JSX.Element {
  switch (face) {
    case 'analog':
      return <Trans>Analog (hands on a dial)</Trans>;
    case 'digital':
      return <Trans>Digital (HH:MM:SS readout)</Trans>;
    case 'dual':
      return <Trans>Dual (analogue face + digital readout)</Trans>;
  }
}

/** The clock-source fields (ADR-0047: face/timezone/label + metadata toggles). */
function ClockKindFields({
  form,
  setForm,
  errors,
}: {
  readonly form: SourceFormState;
  readonly setForm: (next: SourceFormState) => void;
  readonly errors: FieldErrors<SourceField>;
}): JSX.Element {
  const { t } = useLingui();
  const usingZone = form.clockTimezone.trim() !== '';
  return (
    <>
      <SelectField<ClockFace>
        label={t`Face`}
        value={form.clockFace}
        options={CLOCK_FACES}
        optionLabel={clockFaceLabel}
        onChange={(next): void => {
          setForm({ ...form, clockFace: next });
        }}
      />
      <CheckboxField
        id="source-clock-12h"
        label={t`12-hour clock`}
        checked={form.clockTwelveHour}
        onChange={(next): void => {
          setForm({ ...form, clockTwelveHour: next });
        }}
      />
      <FormField
        id="source-clock-tz-name"
        label={t`Timezone (IANA id)`}
        value={form.clockTimezone}
        placeholder="Australia/Sydney"
        datalist={IANA_TIMEZONES}
        error={errors.clockTimezone}
        hint={
          <Trans>
            DST-correct, preferred over a fixed offset (e.g. Australia/Sydney,
            UTC). Leave blank to use a fixed offset instead.
          </Trans>
        }
        onChange={(next): void => {
          setForm({ ...form, clockTimezone: next });
        }}
      />
      {usingZone ? null : (
        <FormField
          id="source-clock-tz"
          label={t`Timezone offset (minutes from UTC)`}
          type="number"
          value={form.clockTzMinutes}
          placeholder="0"
          error={errors.clockTzMinutes}
          hint={<Trans>Whole minutes between −720 and 840 (e.g. 600 = UTC+10).</Trans>}
          onChange={(next): void => {
            setForm({ ...form, clockTzMinutes: next });
          }}
        />
      )}
      <FormField
        id="source-clock-label"
        label={t`Label (optional)`}
        value={form.clockLabel}
        placeholder={t`e.g. Sydney`}
        hint={<Trans>A location/title drawn on the face. Leave blank for none.</Trans>}
        onChange={(next): void => {
          setForm({ ...form, clockLabel: next });
        }}
      />
      <CheckboxField
        id="source-clock-show-offset"
        label={t`Show the UTC offset badge`}
        checked={form.clockShowOffset}
        onChange={(next): void => {
          setForm({ ...form, clockShowOffset: next });
        }}
      />
      <CheckboxField
        id="source-clock-show-reference"
        label={t`Show the reference-lock badge (PTP/NTP/SYS)`}
        checked={form.clockShowReference}
        onChange={(next): void => {
          setForm({ ...form, clockShowReference: next });
        }}
      />
      <CheckboxField
        id="source-clock-numerals"
        label={t`Draw hour numerals (analogue / dual face)`}
        checked={form.clockNumerals}
        onChange={(next): void => {
          setForm({ ...form, clockNumerals: next });
        }}
      />
    </>
  );
}

/** Translate a timer target type to its localized option label. */
function timerTargetTypeLabel(type: TimerTargetType): JSX.Element {
  return type === 'time_of_day' ? (
    <Trans>Time of day (recurring)</Trans>
  ) : (
    <Trans>Specific date &amp; time</Trans>
  );
}

/** Translate a timer direction to its localized option label. */
function timerDirectionLabel(direction: TimerDirection): JSX.Element {
  return direction === 'down' ? (
    <Trans>Count down to the target</Trans>
  ) : (
    <Trans>Count up from the target</Trans>
  );
}

/** Translate a timer at-target behaviour to its localized option label. */
function timerOnTargetLabel(onTarget: TimerOnTarget): JSX.Element {
  switch (onTarget) {
    case 'hold':
      return <Trans>Hold at 00:00:00</Trans>;
    case 'continue':
      return <Trans>Continue past the target</Trans>;
    case 'zero_then_up':
      return <Trans>Reach zero, then count the overrun up</Trans>;
    case 'recur':
      return <Trans>Re-arm to the next occurrence (recurring daily)</Trans>;
  }
}

/** Translate a timer format to its localized option label. */
function timerFormatLabel(format: TimerFormat): JSX.Element {
  switch (format) {
    case 'd_hh_mm_ss':
      return <Trans>D:HH:MM:SS (drop day when zero)</Trans>;
    case 'hh_mm_ss':
      return <Trans>HH:MM:SS</Trans>;
    case 'mm_ss':
      return <Trans>MM:SS</Trans>;
    case 'hh_mm_ss_ff':
      return <Trans>HH:MM:SS:FF (with frames)</Trans>;
    case 'auto':
      return <Trans>Auto (drop leading zero units)</Trans>;
  }
}

/** The timer-source fields (ADR-0047: target/direction/on_target/format + overrun). */
function TimerKindFields({
  form,
  setForm,
  errors,
}: {
  readonly form: SourceFormState;
  readonly setForm: (next: SourceFormState) => void;
  readonly errors: FieldErrors<SourceField>;
}): JSX.Element {
  const { t } = useLingui();
  const isTimeOfDay = form.timerTargetType === 'time_of_day';
  const usingZone = form.timerTimezone.trim() !== '';
  // `recur` is only meaningful for a recurring time-of-day target (ADR-0047);
  // hide it for a date+time target so the body never carries an inert policy.
  const onTargetOptions = isTimeOfDay
    ? TIMER_ON_TARGETS
    : TIMER_ON_TARGETS.filter((o) => o !== 'recur');
  return (
    <>
      <SelectField<TimerTargetType>
        label={t`Target`}
        value={form.timerTargetType}
        options={TIMER_TARGET_TYPES}
        optionLabel={timerTargetTypeLabel}
        onChange={(next): void => {
          setForm(withTimerTargetType(form, next));
        }}
      />
      {isTimeOfDay ? (
        <FormField
          id="source-timer-at-tod"
          label={t`Target time of day`}
          value={form.timerAt}
          required
          placeholder="14:30:00"
          error={errors.timerAt}
          hint={<Trans>24-hour HH:MM:SS in the timezone below.</Trans>}
          onChange={(next): void => {
            setForm({ ...form, timerAt: next });
          }}
        />
      ) : (
        <FormField
          id="source-timer-at-dt"
          label={t`Target date &amp; time`}
          value={form.timerAt}
          required
          placeholder="2026-07-01T09:00:00"
          error={errors.timerAt}
          hint={<Trans>Local YYYY-MM-DDTHH:MM:SS, resolved in the timezone below.</Trans>}
          onChange={(next): void => {
            setForm({ ...form, timerAt: next });
          }}
        />
      )}
      <FormField
        id="source-timer-tz-name"
        label={t`Timezone (IANA id)`}
        value={form.timerTimezone}
        placeholder="Australia/Sydney"
        datalist={IANA_TIMEZONES}
        error={errors.timerTimezone}
        hint={
          <Trans>
            DST-correct, preferred over a fixed offset. Leave blank to use a
            fixed offset instead.
          </Trans>
        }
        onChange={(next): void => {
          setForm({ ...form, timerTimezone: next });
        }}
      />
      {usingZone ? null : (
        <FormField
          id="source-timer-tz"
          label={t`Timezone offset (minutes from UTC)`}
          type="number"
          value={form.timerTzMinutes}
          placeholder="0"
          error={errors.timerTzMinutes}
          hint={<Trans>Whole minutes between −720 and 840 (e.g. 600 = UTC+10).</Trans>}
          onChange={(next): void => {
            setForm({ ...form, timerTzMinutes: next });
          }}
        />
      )}
      {isTimeOfDay ? (
        <CheckboxField
          id="source-timer-recur-daily"
          label={t`Recur daily (re-arm each day)`}
          checked={form.timerRecurDaily}
          onChange={(next): void => {
            setForm({ ...form, timerRecurDaily: next });
          }}
        />
      ) : null}
      <SelectField<TimerDirection>
        label={t`Direction`}
        value={form.timerDirection}
        options={TIMER_DIRECTIONS}
        optionLabel={timerDirectionLabel}
        onChange={(next): void => {
          setForm({ ...form, timerDirection: next });
        }}
      />
      <SelectField<TimerOnTarget>
        label={t`At the target`}
        value={form.timerOnTarget}
        options={onTargetOptions}
        optionLabel={timerOnTargetLabel}
        onChange={(next): void => {
          setForm({ ...form, timerOnTarget: next });
        }}
      />
      <SelectField<TimerFormat>
        label={t`Display format`}
        value={form.timerFormat}
        options={TIMER_FORMATS}
        optionLabel={timerFormatLabel}
        onChange={(next): void => {
          setForm({ ...form, timerFormat: next });
        }}
      />
      <FormField
        id="source-timer-label"
        label={t`Label (optional)`}
        value={form.timerLabel}
        placeholder={t`e.g. ON AIR IN`}
        hint={<Trans>Drawn with the count. Leave blank for none.</Trans>}
        onChange={(next): void => {
          setForm({ ...form, timerLabel: next });
        }}
      />
      <FormField
        id="source-timer-overrun-prefix"
        label={t`Overrun prefix (optional)`}
        value={form.timerOverrunPrefix}
        placeholder="+"
        hint={<Trans>Shown past the target. Blank uses the default “+”.</Trans>}
        onChange={(next): void => {
          setForm({ ...form, timerOverrunPrefix: next });
        }}
      />
      <CheckboxField
        id="source-timer-overrun-badge"
        label={t`Show the overrun badge (OVER / ELAPSED)`}
        checked={form.timerOverrunBadge}
        onChange={(next): void => {
          setForm({ ...form, timerOverrunBadge: next });
        }}
      />
    </>
  );
}

/** The collapsible Advanced block (captions / colour / pin / wallclock / auth). */
function SourceAdvancedFields({
  form,
  setForm,
  errors,
}: {
  readonly form: SourceFormState;
  readonly setForm: (next: SourceFormState) => void;
  readonly errors: FieldErrors<SourceField>;
}): JSX.Element {
  const { t } = useLingui();
  const captionLabel = (mode: CaptionsMode): JSX.Element => {
    switch (mode) {
      case 'none':
        return <Trans>None (no captions decoded)</Trans>;
      case 'auto':
        return <Trans>Auto (first usable track)</Trans>;
      case 'off':
        return <Trans>Off (pinned off)</Trans>;
      case 'teletext_page':
        return <Trans>DVB teletext page</Trans>;
      case 'track':
        return <Trans>Subtitle track (id / language)</Trans>;
      case 'embedded_cc':
        return <Trans>Embedded CEA-608 (CC1–CC4 only)</Trans>;
      case 'sidecar':
        return <Trans>Stored sidecar (not decoded per source)</Trans>;
    }
  };
  const activationLabel = (choice: SourceActivationChoice): JSX.Element => {
    switch (choice) {
      case 'always_on':
        return <Trans>Always on (started at boot)</Trans>;
      case 'on_demand':
        return <Trans>On demand (idle until something uses it)</Trans>;
    }
  };
  const wallclockLabel = (choice: WallClockChoice): JSX.Element => {
    switch (choice) {
      case 'default':
        return <Trans>Engine default (reclock to house)</Trans>;
      case 'use':
        return <Trans>Use the source wall-clock when trusted</Trans>;
      case 'discard':
        return <Trans>Discard the source wall-clock</Trans>;
    }
  };
  return (
    <AdvancedSection summary={t`Advanced`}>
      {form.kind === 'hls' ? (
        <>
          <FormField
            id="source-user-agent"
            label={t`User-Agent header (optional)`}
            value={form.userAgent}
            error={errors.userAgent}
            hint={<Trans>Sent on every HLS request. Not a secret; exported with the config.</Trans>}
            onChange={(next): void => {
              setForm({ ...form, userAgent: next });
            }}
          />
          <FormField
            id="source-referrer"
            label={t`Referer header (optional)`}
            value={form.referrer}
            error={errors.referrer}
            onChange={(next): void => {
              setForm({ ...form, referrer: next });
            }}
          />
        </>
      ) : null}
      <SelectField<CaptionsMode>
        label={t`Captions`}
        value={form.captionsMode}
        options={captionModesForValue(form.captionsMode)}
        optionLabel={captionLabel}
        onChange={(next): void => {
          setForm({ ...form, captionsMode: next });
        }}
      />
      {form.captionsMode === 'teletext_page' ? (
        <FormField
          id="source-captions-page"
          label={t`Teletext page`}
          type="number"
          value={form.captionsPage}
          placeholder="801"
          error={errors.captionsPage}
          hint={<Trans>Available for local TS/file sources with a teletext stream; page 100–899.</Trans>}
          onChange={(next): void => {
            setForm({ ...form, captionsPage: next });
          }}
        />
      ) : null}
      {form.captionsMode === 'track' ? (
        <FormField
          id="source-captions-track"
          label={t`Track id or language tag`}
          value={form.captionsTrack}
          placeholder="eng"
          error={errors.captionsTrack}
          onChange={(next): void => {
            setForm({ ...form, captionsTrack: next });
          }}
        />
      ) : null}
      {form.captionsMode === 'embedded_cc' ? (
        <FormField
          id="source-captions-field"
          label={t`Caption field / service`}
          value={form.captionsField}
          placeholder="cc1"
          error={errors.captionsField}
          hint={<Trans>Only CEA-608 fields cc1, cc2, cc3 and cc4 decode. CEA-708 services are not supported.</Trans>}
          onChange={(next): void => {
            setForm({ ...form, captionsField: next });
          }}
        />
      ) : null}
      {form.captionsMode === 'sidecar' ? (
        <FormField
          id="source-captions-path"
          label={t`Sidecar file path`}
          value={form.captionsPath}
          placeholder="/subs/program.vtt"
          error={errors.captionsPath}
          hint={<Trans>This stored sidecar is not decoded per source. Use the legacy command-line subtitle option instead.</Trans>}
          onChange={(next): void => {
            setForm({ ...form, captionsPath: next });
          }}
        />
      ) : null}

      <CheckboxField
        id="source-color-override"
        label={t`Override the detected colour metadata`}
        checked={form.colorOverrideEnabled}
        onChange={(next): void => {
          setForm({ ...form, colorOverrideEnabled: next });
        }}
      />
      {form.colorOverrideEnabled ? (
        <div className="grid grid-cols-2 gap-3">
          <FormField
            id="source-color-primaries"
            label={t`Primaries`}
            value={form.colorPrimaries}
            placeholder="auto"
            onChange={(next): void => {
              setForm({ ...form, colorPrimaries: next });
            }}
          />
          <FormField
            id="source-color-transfer"
            label={t`Transfer`}
            value={form.colorTransfer}
            placeholder="auto"
            onChange={(next): void => {
              setForm({ ...form, colorTransfer: next });
            }}
          />
          <FormField
            id="source-color-matrix"
            label={t`Matrix`}
            value={form.colorMatrix}
            placeholder="auto"
            onChange={(next): void => {
              setForm({ ...form, colorMatrix: next });
            }}
          />
          <FormField
            id="source-color-range"
            label={t`Range`}
            value={form.colorRange}
            placeholder="auto"
            onChange={(next): void => {
              setForm({ ...form, colorRange: next });
            }}
          />
        </div>
      ) : null}

      <CheckboxField
        id="source-gpu-pin"
        label={t`Pin decode to a specific GPU`}
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
            id="source-gpu-stable-id"
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

      <SelectField<SourceActivationChoice>
        label={t`Start`}
        value={form.activation}
        options={ACTIVATION_CHOICES}
        optionLabel={activationLabel}
        onChange={(next): void => {
          setForm({ ...form, activation: next });
        }}
      />

      <SelectField<WallClockChoice>
        label={t`Wall-clock handling`}
        value={form.wallclock}
        options={WALLCLOCK_CHOICES}
        optionLabel={wallclockLabel}
        onChange={(next): void => {
          setForm({ ...form, wallclock: next });
        }}
      />

      <FormField
        id="source-auth"
        label={t`Credentials secret reference`}
        value={form.authSecretRef}
        placeholder="op://Servers/cam/credentials"
        hint={<Trans>A reference only — never a plaintext secret. Leave blank for none.</Trans>}
        onChange={(next): void => {
          setForm({ ...form, authSecretRef: next });
        }}
      />

      <div className="mt-2 flex flex-col gap-3 border-t pt-3">
        <h4 className="text-sm font-medium">
          <Trans>Upstream EPG (electronic programme guide)</Trans>
        </h4>
        <SelectField<EpgModeChoice>
          label={t`EPG mode`}
          value={form.epgMode}
          options={EPG_MODES}
          optionLabel={epgModeLabel}
          onChange={(next): void => {
            setForm({ ...form, epgMode: next });
          }}
        />
        {form.epgMode !== 'disabled' ? (
          <>
            <FormField
              id="source-epg-url"
              label={t`XMLTV guide URL`}
              value={form.epgUrl}
              placeholder="https://guide.example.net/xmltv.xml"
              error={errors.epgUrl}
              required={form.epgMode === 'manual'}
              hint={<Trans>Required in manual mode; credential-free http(s) only.</Trans>}
              onChange={(next): void => {
                setForm({ ...form, epgUrl: next });
              }}
            />
            <FormField
              id="source-epg-channel-id"
              label={t`Provider channel id`}
              value={form.epgChannelId}
              placeholder="bbc1.uk"
              error={errors.epgChannelId}
              required={form.epgMode === 'manual'}
              hint={<Trans>The exact channel id as it appears in the XMLTV feed.</Trans>}
              onChange={(next): void => {
                setForm({ ...form, epgChannelId: next });
              }}
            />
          </>
        ) : (
          <p className="text-xs text-muted-foreground">
            <Trans>Disabled — any previously configured URL/channel id is retained for later re-enable.</Trans>
          </p>
        )}
      </div>

      <div className="flex flex-col gap-3 border-t pt-3">
        <h4 className="text-sm font-medium">
          <Trans>IPTV playlist presentation</Trans>
        </h4>
        <div className="grid grid-cols-2 gap-3">
          <FormField
            id="source-meta-tvg-id"
            label={t`tvg-id`}
            value={form.metaTvgId}
            placeholder="bbc1.uk"
            onChange={(next): void => {
              setForm({ ...form, metaTvgId: next });
            }}
          />
          <FormField
            id="source-meta-name"
            label={t`Display name`}
            value={form.metaName}
            placeholder="BBC One"
            error={errors.metaName}
            onChange={(next): void => {
              setForm({ ...form, metaName: next });
            }}
          />
          <FormField
            id="source-meta-logo"
            label={t`Logo URL`}
            value={form.metaLogo}
            placeholder="https://logos.example.net/bbc1.png"
            error={errors.metaLogo}
            onChange={(next): void => {
              setForm({ ...form, metaLogo: next });
            }}
          />
          <FormField
            id="source-meta-group"
            label={t`Group / category`}
            value={form.metaGroup}
            placeholder="UK Terrestrial"
            onChange={(next): void => {
              setForm({ ...form, metaGroup: next });
            }}
          />
        </div>
      </div>
    </AdvancedSection>
  );
}

function epgModeLabel(mode: EpgModeChoice): JSX.Element {
  switch (mode) {
    case 'automatic':
      return <Trans>Automatic (resolve the guide automatically)</Trans>;
    case 'manual':
      return <Trans>Manual (explicit XMLTV URL + channel id)</Trans>;
    case 'disabled':
      return <Trans>Disabled (no guide fetched for this source)</Trans>;
  }
}

function isSourceFormState(value: unknown): value is SeedForm {
  return typeof value === 'object' && value !== null && 'kind' in value && 'extra' in value;
}

/** Read the “Add as source” seed the IPTV-ORG browser leaves in router state. */
function readIptvSeed(state: unknown): { ts: number; form: SeedForm } | undefined {
  if (typeof state !== 'object' || state === null || !('iptvSeed' in state)) {
    return undefined;
  }
  const seed = state.iptvSeed;
  if (typeof seed !== 'object' || seed === null || !('ts' in seed) || !('form' in seed)) {
    return undefined;
  }
  // The seed is produced by our own browser page (toSourceForm) in router state.
  return typeof seed.ts === 'number' && isSourceFormState(seed.form)
    ? { ts: seed.ts, form: seed.form }
    : undefined;
}

/** Sources management (ingest). */
export function SourcesPage(): JSX.Element {
  const { t } = useLingui();
  const sources = useSources();
  const tiles = useLiveTiles();
  const demand = useSourceStatus();
  const links = useCatalogueLinks();
  const linkBySource = new Map((links.data?.links ?? []).map((l) => [l.source_id, l]));
  // "Add as source" from /sources/iptv-org arrives as router state; each distinct
  // timestamp opens the create dialog once, prefilled (nothing is saved yet).
  const location = useLocation();
  const [seed, setSeed] = useState<CrudSeed<SeedForm> | undefined>(undefined);
  const [seenTs, setSeenTs] = useState<number | undefined>(undefined);
  const incoming = readIptvSeed(location.state);
  if (incoming !== undefined && incoming.ts !== seenTs) {
    setSeenTs(incoming.ts);
    setSeed({ key: incoming.ts, creating: true, form: incoming.form });
  }

  // The approved surface owns presentation only; the original action
  // components, permissions, live status queries and mutations stay the
  // source of truth.
  const renderItems = (
    rows: readonly SourceView[],
    onEdit: (row: SourceView) => void,
    onDelete: (row: SourceView) => void,
  ): JSX.Element => (
    <SourcesSurface
      rows={rows}
      demand={demand.data}
      tileFor={(id): LiveTile | undefined => tileForSource(tiles, id)}
      linkFor={(id): ReturnType<typeof linkBySource.get> => linkBySource.get(id)}
      onEdit={onEdit}
      onDelete={onDelete}
    />
  );

  return (
    <div data-sources-touch="" data-approved-page="sources" className="min-w-0 [&_button]:min-h-12 [&_button]:min-w-12 [&_button]:h-auto [&_button]:whitespace-normal [&_button]:break-words [&_button]:max-w-full [&_button>span]:min-w-0 [&_button>span]:whitespace-normal [&_button>span]:[overflow-wrap:anywhere] [&_a]:min-w-12 [&_a]:min-h-12 [&_a]:whitespace-normal [&_a]:break-words [&>div:first-child>div:last-child]:flex-wrap [&>div:first-child>div:last-child]:min-w-0 [&>div:first-child>div:last-child]:max-w-full">
    <CrudPage<SourceView, SourceFormState, SourceField>
      kind="sources"
      dialogClassName="[&_button]:min-h-12 [&_button]:min-w-12 [&_button]:h-auto [&_button]:whitespace-normal [&_button]:[overflow-wrap:anywhere] [&_summary]:min-h-12 [&_summary]:content-center [&_input]:min-h-12 [&_input]:min-w-12 [&_select:not([aria-hidden=true])]:min-h-12 [&_select:not([aria-hidden=true])]:min-w-12 [&_[role=combobox]]:min-h-12 [&_[role=combobox]]:min-w-12 [&_a]:min-h-12 [&_a]:min-w-12 [&_a]:inline-flex [&_a]:items-center [&_a]:whitespace-normal [&_a]:[overflow-wrap:anywhere] [&_*]:min-w-0"
      title={<Trans>Sources</Trans>}
      description={
        <span className="inline-flex flex-wrap items-center gap-x-3 gap-y-1">
          <Trans>Add and manage live ingest sources.</Trans>
          <HelpLink to="/help/concepts/transports#choosing" label={t`About source transports`} />
        </span>
      }
      newLabel={t`New source`}
      dialogCreateTitle={t`New source`}
      dialogEditTitle={t`Edit source`}
      dialogDescription={t`A source is an ingest input bound into the multiview by id.`}
      caption={t`Configured ingest sources.`}
      emptyMessage={<Trans>No sources configured.</Trans>}
      loadingMessage={<Trans>Loading sources…</Trans>}
      errorPrefix={<Trans>Could not load sources:</Trans>}
      seed={seed}
      headerExtras={
        <>
          <M3uImportButton />
          <Button asChild variant="outline">
            <Link to="/sources/iptv-org">
              <Trans>Browse IPTV-ORG</Trans>
            </Link>
          </Button>
          <ExportConfigButton compact />
        </>
      }
      callout={
        <ApplySemanticsCallout
          helpTo="/help/config#sources"
          helpLabel={t`How configuration applies`}
          message={
            <Trans>
              Synthetic sources (bars, solid colour, clock, timer) apply to the
              running engine immediately when saved, and deleting any source
              removes it from the running engine immediately. Network and file
              sources (RTSP, HLS, MPEG-TS, SRT, RTMP, RIST, file) also apply live
              on a full-engine run; on the software engine, and for NDI / YouTube
              / AES67, they are stored and go live via config export + restart.
              Each save response declares which applied (X-Multiview-Apply: live
              or restart).
            </Trans>
          }
        />
      }
      savedDescription={t`Stored. Synthetic sources (bars, solid colour, clock, timer) and — on a full-engine run — network/file sources apply to the running engine immediately; NDI / YouTube / AES67 (and any kind on the software engine) go live via config export + restart. The response header declares which applied.`}
      deletedDescription={t`Removed. The running engine drops the source immediately (the response header confirms) — tiles bound to it show the no-signal slate until re-routed.`}
      list={sources.data ?? []}
      isPending={sources.isPending}
      isError={sources.isError}
      errorMessage={sources.error?.message}
      columns={() => []}
      renderItems={renderItems}
      rowId={(row): string => row.id}
      rowName={(row): string => row.name}
      emptyForm={emptySourceForm}
      formFromRecord={sourceFormFromRecord}
      validate={validateSourceForm}
      toSaveVars={(form, creating): SaveResourceVars => ({
        id: creating ? form.id.trim() : form.id,
        create: creating,
        input: { name: form.name.trim(), body: sourceFormToBody(form) },
      })}
      renderFields={(form, setForm, creating, errors): JSX.Element => (
        <>
          {creating ? (
            // Device-projected streams (ADR-M009 facet (a)): picking one
            // prefills the transport form and stamps device_ref.
            <SourceFromDeviceSection form={form} setForm={setForm} />
          ) : null}
          <FormField
            id="source-id"
            label={t`Identifier`}
            value={form.id}
            disabled={!creating}
            required={creating}
            placeholder={t`e.g. cam-north`}
            error={errors.id}
            onChange={(next): void => {
              setForm({ ...form, id: next });
            }}
          />
          <FormField
            id="source-name"
            label={t`Name`}
            value={form.name}
            required
            error={errors.name}
            onChange={(next): void => {
              setForm({ ...form, name: next });
            }}
          />
          <SelectField<SourceFormKind>
            label={t`Kind`}
            value={form.kind}
            options={FORM_KINDS}
            trailing={
              <HelpLink
                to="/help/concepts/transports#choosing"
                label={t`About source transports`}
                compact
              />
            }
            onChange={(next): void => {
              setForm(withSourceKind(form, next));
            }}
          />
          <SourceKindFields form={form} setForm={setForm} errors={errors} />
          <OriginPanel form={form} setForm={setForm} />
          <SourceAdvancedFields form={form} setForm={setForm} errors={errors} />
        </>
      )}
    />
    </div>
  );
}
