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
import { Link } from 'react-router-dom';
import { hlsPlaybackCollisionIds, hlsPlaybackPaths } from '../preview/outputPlayback';
import { Trans, useLingui } from '@lingui/react/macro';
import { CircleCheck, Hourglass, MonitorCog } from 'lucide-react';
import type { ColumnDef } from '@tanstack/react-table';

import { OutputFromDeviceSection } from '../devices/FromDevice';
import { useOutputLink, useOutputs } from '../resources/queries';
import { useAudioRouting } from '../resources/audioRouting';
import type { SaveResourceVars } from '../resources/queries';
import type { OutputKind, OutputView } from '../resources/types';
import { OUTPUT_KINDS } from '../resources/types';
import {
  emptyOutputForm,
  OUTPUT_RUNNABLE,
  outputFormFromRecord,
  outputFormToBody,
  parseTrackList,
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
  DerivedUrlField,
  ExportConfigButton,
  FieldErrorMessage,
  FormField,
  PlaybackUrlRow,
  SecretField,
  SelectField,
} from '../resources/FormControls';
import { whepPlayUrl } from '../resources/api';
import { useOutputPlayback } from '../resources/outputPlayback';
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

/** Output kinds that report a live network link. */
function hasLiveLink(kind: OutputKind): boolean {
  return kind === 'srt' || kind === 'rtmp' || kind === 'rist';
}

/**
 * A push output's live link: whether a peer is attached, what is wrong *now*,
 * and why packets were not delivered.
 *
 * HONESTY (issue #202): a listener waiting for a caller discards every encoded
 * packet, which is expected — reporting that as "packets dropped" alongside an
 * error from a previous session reads as a fault when the link is healthy. So
 * (1) only `last_error` — current link health — is shown as an alarm;
 * `last_session_error` is muted history from the previous session; and (2) the
 * additive reason counters separate benign discards from real delivery
 * failures. Benign is exactly two things: no caller attached, and a *fresh*
 * session waiting for its first keyframe (nothing is wrong with the link, the
 * decoder just has not been handed an IDR). A slow peer's queue overflow, and
 * every packet held until that peer is re-admitted on the next keyframe — audio
 * and non-IDR video alike — is delivery loss that peer caused, so it is counted
 * as slow-peer loss and shown as an alarm (t10 / SRT-STATUS-01: the two causes
 * share one keyframe gate and were once conflated here). Bodies from a server
 * without the reason counters fall back to the one conflated total.
 */
export function OutputLinkStatus({ id }: { readonly id: string }): JSX.Element | null {
  const { t } = useLingui();
  const link = useOutputLink(id, true).data;
  if (link === undefined || link === null) {
    return null;
  }
  const state = ((): string => {
    switch (link.state) {
      case 'listening':
        return t`Listening, no caller attached`;
      case 'connected':
        return t`Connected (${link.clients})`;
      case 'connecting':
        return t`Connecting`;
      case 'reconnecting':
        return t`Reconnecting`;
      default:
        return t`Idle`;
    }
  })();
  // The reason counters are additive; an older server sends none of them and
  // only its conflated `dropped` total, which we cannot honestly split.
  const hasReasons =
    link.discarded_no_client !== undefined &&
    link.discarded_awaiting_keyframe !== undefined &&
    link.dropped_slow_peer !== undefined &&
    link.dropped_upstream !== undefined;
  const discardedNoClient = link.discarded_no_client ?? 0;
  const discardedKeyframe = link.discarded_awaiting_keyframe ?? 0;
  const slowPeerDrops = link.dropped_slow_peer ?? 0;
  const upstreamDrops = link.dropped_upstream ?? 0;
  return (
    <div className="flex flex-col gap-0.5 text-xs" data-testid="output-link">
      <span>{state}</span>
      {link.peers.map((peer) => (
        <code key={peer} className="text-muted-foreground" lang="" dir="auto">
          {peer}
        </code>
      ))}
      {link.last_error !== undefined && link.last_error !== null ? (
        <span className="text-destructive">{link.last_error}</span>
      ) : null}
      {link.last_session_error !== undefined &&
      link.last_session_error !== null ? (
        <span className="text-muted-foreground">
          <Trans>Last session ended: {link.last_session_error}</Trans>
        </span>
      ) : null}
      {hasReasons ? (
        <>
          {slowPeerDrops > 0 ? (
            <span className="text-destructive">
              <Trans>
                {slowPeerDrops} packets dropped (slow peer, incl. keyframe resync)
              </Trans>
            </span>
          ) : null}
          {upstreamDrops > 0 ? (
            <span className="text-destructive">
              <Trans>{upstreamDrops} packets dropped upstream (queue full)</Trans>
            </span>
          ) : null}
          {discardedNoClient > 0 ? (
            <span className="text-muted-foreground">
              <Trans>
                {discardedNoClient} packets discarded while no caller was attached
              </Trans>
            </span>
          ) : null}
          {discardedKeyframe > 0 ? (
            <span className="text-muted-foreground">
              <Trans>
                {discardedKeyframe} packets discarded waiting for the first keyframe
              </Trans>
            </span>
          ) : null}
        </>
      ) : link.dropped > 0 ? (
        <span className="text-destructive">
          <Trans>{link.dropped} packets dropped</Trans>
        </span>
      ) : null}
    </div>
  );
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
          hint={t`To let callers pull the program, use srt://[::]:7001?mode=listener: one socket serves up to 16 IPv6 and IPv4 callers. 0.0.0.0 is IPv4 only.`}
          error={errors.url}
          onChange={(next): void => {
            setForm({ ...form, url: next });
          }}
        />
      );
    case 'rist':
      return (
        <FormField
          id="output-url"
          label={t`Destination URL`}
          value={form.url}
          required
          placeholder="rist://[2001:db8::1]:5000"
          error={errors.url}
          onChange={(next): void => {
            setForm({ ...form, url: next });
          }}
        />
      );
    case 'display':
      return <DisplayKindFields form={form} setForm={setForm} errors={errors} />;
    case 'webrtc':
      return <WebrtcOutputFields form={form} setForm={setForm} errors={errors} />;
    case 'whip-push':
      return <WhipPushOutputFields form={form} setForm={setForm} errors={errors} />;
  }
}

/**
 * The viewer **playback URL(s)** section (ADR-0101 — "a link to copy the
 * actual URL to paste into a tool like VLC"): one {@link PlaybackUrlRow} per
 * URL the server derives for this output (`GET
 * /api/v1/outputs/{id}/playback`). Only rendered for a SAVED output (the
 * route needs a real id); an unsaved (still-creating) output shows nothing
 * here — there is nothing to derive yet.
 */
function PlaybackUrlsSection({ outputId }: { readonly outputId: string }): JSX.Element | null {
  const { t } = useLingui();
  const playback = useOutputPlayback(outputId === '' ? undefined : outputId);
  if (outputId === '' || playback.data === undefined || playback.data.length === 0) {
    return null;
  }
  return (
    <div className="flex flex-col gap-2">
      <p className="text-sm font-medium">{t`Playback URL`}</p>
      {playback.data.map((entry, index) => (
        <PlaybackUrlRow
          key={`${entry.label}-${String(index)}`}
          idPrefix={`output-playback-${outputId}-${String(index)}`}
          label={entry.label}
          url={entry.url ?? undefined}
          vlcCommand={entry.vlc_command ?? undefined}
          notes={entry.notes ?? []}
        />
      ))}
    </div>
  );
}

/**
 * The WHEP-serve (`webrtc`) output fields (ADR-0049 / ADR-W023). It never
 * encodes — it fans the H.264 program rendition to browser viewers — so the
 * form shows the DERIVED WHEP play URL (read-only, copyable), the viewer cap,
 * and the optional viewer token (masked). The B-frames-off H.264 requirement is
 * stated, since this schema has no rendition-settings surface to enforce it.
 */
function WebrtcOutputFields({
  form,
  setForm,
  errors,
}: {
  readonly form: OutputFormState;
  readonly setForm: (next: OutputFormState) => void;
  readonly errors: FieldErrors<OutputField>;
}): JSX.Element {
  const { t } = useLingui();
  const id = form.id.trim();
  return (
    <>
      {id === '' ? (
        <p className="text-xs text-muted-foreground">
          <Trans>
            Set an identifier above; the WHEP play endpoint is derived from it
            and shown here once it is filled in.
          </Trans>
        </p>
      ) : (
        <DerivedUrlField
          id="output-whep-url"
          label={t`WHEP play endpoint`}
          value={whepPlayUrl(id)}
          hint={
            <Trans>
              Browser viewers POST an SDP offer here. With no token, viewing
              needs a control-plane API key with View scope — never anonymous.
              Relative to the control plane origin.
            </Trans>
          }
          trailing={<HelpLink to="/help/concepts/glossary#whep" label={t`What is WHEP?`} compact />}
        />
      )}
      <FormField
        id="output-webrtc-max-viewers"
        label={t`Maximum concurrent viewers (optional)`}
        type="number"
        value={form.webrtcMaxViewers}
        placeholder="8"
        error={errors.webrtcMaxViewers}
        hint={<Trans>Viewers beyond this cap (or the endpoint pool) receive a 503. Default 8.</Trans>}
        onChange={(next): void => {
          setForm({ ...form, webrtcMaxViewers: next });
        }}
      />
      <SecretField
        id="output-webrtc-token"
        label={t`Viewer token (optional)`}
        value={form.webrtcToken}
        placeholder={t`leave blank to require a View API key`}
        hint={
          <Trans>
            A bearer token a viewer presents on the WHEP POST. Leave blank to
            require a control-plane API key with View scope instead.
          </Trans>
        }
        onChange={(next): void => {
          setForm({ ...form, webrtcToken: next });
        }}
      />
      <p className="text-xs text-muted-foreground">
        <Trans>
          A WebRTC output consumes the H.264 program rendition with B-frames
          off — it does not spawn a separate encode.
        </Trans>
      </p>
    </>
  );
}

/**
 * The WHIP-push (`whip-push`) output fields (ADR-0049 / ADR-W023): publishes
 * the program to a remote WHIP origin (the WebRTC sibling of rtmp/srt push).
 * https is recommended; the client aborts on a plaintext downgrade.
 */
function WhipPushOutputFields({
  form,
  setForm,
  errors,
}: {
  readonly form: OutputFormState;
  readonly setForm: (next: OutputFormState) => void;
  readonly errors: FieldErrors<OutputField>;
}): JSX.Element {
  const { t } = useLingui();
  return (
    <>
      <FormField
        id="output-url"
        label={t`Remote WHIP endpoint URL`}
        value={form.url}
        required
        placeholder="https://[2001:db8::15]:8443/whip/pgm1"
        error={errors.url}
        hint={
          <Trans>
            The remote origin's WHIP ingest URL. Use https — the client follows
            only https redirects and aborts on a plaintext downgrade.
          </Trans>
        }
        onChange={(next): void => {
          setForm({ ...form, url: next });
        }}
      />
      <SecretField
        id="output-whip-push-token"
        label={t`Origin token (optional)`}
        value={form.whipPushToken}
        placeholder={t`bearer token for the remote origin`}
        hint={<Trans>A bearer token sent on the WHIP POST to the remote origin.</Trans>}
        onChange={(next): void => {
          setForm({ ...form, whipPushToken: next });
        }}
      />
    </>
  );
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

/**
 * The discrete-track picker for `audioMode = 'tracks'`: when the current
 * audio-routing document's `selectable_tracks` are known, offer ONLY those as
 * checkboxes (the Outputs page must never let an operator select a track
 * that does not currently exist — AUD-9). A track this output already
 * carries but the routing document no longer declares (stale after an
 * external edit) is still shown, checked, so editing never silently drops it;
 * it is clearly distinguished as unknown. Falls back to the legacy free-text
 * comma list only while the routing document has not loaded yet.
 */
function AudioTracksField({
  form,
  setForm,
  error,
  selectableTracks,
}: {
  readonly form: OutputFormState;
  readonly setForm: (next: OutputFormState) => void;
  readonly error: FieldErrors<OutputField>['audioTracks'];
  readonly selectableTracks: readonly string[] | undefined;
}): JSX.Element {
  const { t } = useLingui();
  if (selectableTracks === undefined) {
    return (
      <FormField
        id="output-audio-tracks"
        label={t`Tracks (comma-separated)`}
        value={form.audioTracks}
        placeholder="prog, commentary"
        error={error}
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
    );
  }
  const selected = parseTrackList(form.audioTracks);
  const unknownSelected = selected.filter((track) => !selectableTracks.includes(track));
  const options = [...selectableTracks, ...unknownSelected];
  const toggle = (track: string, checked: boolean): void => {
    const next = checked
      ? [...selected, track]
      : selected.filter((existing) => existing !== track);
    setForm({ ...form, audioTracks: next.join(', ') });
  };
  return (
    <fieldset className="flex flex-col gap-1">
      <legend className="text-sm font-medium">
        <Trans>Tracks</Trans>
      </legend>
      {options.length === 0 ? (
        <p className="text-xs text-muted-foreground">
          <Trans>No tracks declared yet on the Audio page.</Trans>
        </p>
      ) : (
        options.map((track) => (
          <div key={track} className="flex items-center gap-2">
            <CheckboxField
              id={`output-audio-track-${track}`}
              label={track}
              checked={selected.includes(track)}
              onChange={(checked): void => {
                toggle(track, checked);
              }}
            />
            {unknownSelected.includes(track) ? (
              <span className="text-xs text-destructive">
                <Trans>not currently declared</Trans>
              </span>
            ) : null}
          </div>
        ))
      )}
      {error !== undefined ? (
        <p role="alert" className="text-sm text-destructive">
          <FieldErrorMessage code={error} />
        </p>
      ) : null}
      <p className="text-xs text-muted-foreground">
        <Trans>
          Names declared by the audio routing block; the program bus is
          always available as "prog".
        </Trans>
      </p>
    </fieldset>
  );
}

/** The collapsible Advanced block (audio selection + GPU pin). */
function OutputAdvancedFields({
  form,
  setForm,
  errors,
  selectableTracks,
}: {
  readonly form: OutputFormState;
  readonly setForm: (next: OutputFormState) => void;
  readonly errors: FieldErrors<OutputField>;
  readonly selectableTracks: readonly string[] | undefined;
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
        <AudioTracksField
          form={form}
          setForm={setForm}
          error={errors.audioTracks}
          selectableTracks={selectableTracks}
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
  const routing = useAudioRouting();
  const selectableTracks = routing.data?.state.selectable_tracks;
  const playbackPaths = hlsPlaybackPaths(outputs.data ?? []);
  const playbackCollisions = hlsPlaybackCollisionIds(outputs.data ?? []);

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
          <div className="flex flex-col gap-1">
            <RunnabilityNote kind={ctx.row.original.kind} />
            {hasLiveLink(ctx.row.original.kind) ? (
              <OutputLinkStatus id={ctx.row.original.id} />
            ) : null}
          </div>
        ) : (
          <span className="text-sm text-muted-foreground" aria-hidden="true">
            —
          </span>
        ),
    },
    {
      id: 'playback',
      header: t`Playback`,
      cell: (ctx): JSX.Element => {
        const path = playbackPaths.get(ctx.row.original.id);
        return <div className="flex flex-col gap-1 text-sm">
          <Link className="underline" to="/monitoring"><Trans>Watch program</Trans></Link>
          {path !== undefined ? <a className="underline" href={path} target="_blank" rel="noreferrer"><Trans>Open HLS playlist</Trans></a> : null}
          {playbackCollisions.has(ctx.row.original.id) ? <span className="text-xs text-muted-foreground"><Trans>HLS link unavailable: output IDs resolve to colliding mounts; the runtime mount depends on configuration order.</Trans></span> : null}
          {path === undefined && !playbackCollisions.has(ctx.row.original.id) && (ctx.row.original.kind === 'hls' || ctx.row.original.kind === 'll-hls') ? <span className="text-xs text-muted-foreground"><Trans>Output playback unavailable: the configured playlist path is missing.</Trans></span> : null}
          <span className="text-xs text-muted-foreground"><Trans>Requires an active output; saved settings do not confirm playback.</Trans></span>
        </div>;
      },
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
          {creating ? (
            // Device decode targets (ADR-M009 facet (b)): picking one binds
            // the new output to the device via device_ref.
            <OutputFromDeviceSection form={form} setForm={setForm} />
          ) : null}
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
          <OutputAdvancedFields
            form={form}
            setForm={setForm}
            errors={errors}
            selectableTracks={selectableTracks}
          />
          {!creating ? <PlaybackUrlsSection outputId={form.id} /> : null}
        </>
      )}
    />
  );
}
