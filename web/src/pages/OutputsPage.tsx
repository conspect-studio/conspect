// Outputs — kind-specific typed management of the program sinks/servers.
//
// Form fields map 1:1 onto the config `Output` schema
// (crates/multiview-config/src/schema.rs) via ../resources/forms. HONESTY:
// `rtsp_server` and `ndi` outputs are accepted by the config but NOT yet
// runnable — `build_outputs` in crates/multiview-cli/src/pipeline.rs warns and
// skips them — so the card and the form flag those kinds with a clear,
// non-alarming "Not yet runnable in this build" note (text + icon, never
// colour alone). hls / ll_hls / rtmp / srt run today; `display` (the local
// DRM/KMS head, DEV-B1/ADR-0044) runs only in a `display-kms` build and is
// badged "Requires a display-kms build" (a default build fails the run with a
// clear error rather than skipping the output).
//
// LIST (broadcast console UI, design round 3/4 adoption): the resource TABLE
// is gone — one responsive TaskWorkspace card per output, with the concise
// destination + link state + the actual configured facts (transport kind,
// destination, codec) always visible in the fold, the per-card Edit/Delete
// keys at the broadcast 48px touch floor (TouchAction), and the secondary
// honesty notes behind the caller-controlled Details disclosure. A table
// would have hidden the actions behind horizontal scroll on a phone; the
// cards never do. ProducerStatus is deliberately NOT re-rendered per card:
// the outputs source API exposes no producer/freshness/health fact per sink,
// so there is nothing honest to badge — the shell already renders the one
// honest app-wide "Unknown" producer line, and a per-card badge here would
// either duplicate it or invent facts (a `written` counter is cumulative
// output, never media health; a connected push link is a peer attached,
// never "publishing decoded media"). The CRUD dialogs (create / edit with
// GET prefill + `If-Match` ETag, delete confirmation) keep the exact
// CrudPage flow and copy; they were lifted, not redesigned.
import { useMemo, useState } from 'react';
import type { JSX } from 'react';
import { Link } from 'react-router-dom';
import { hlsPlaybackCollisionIds, hlsPlaybackPaths } from '../preview/outputPlayback';
import { Trans, useLingui } from '@lingui/react/macro';
import { CircleCheck, Hourglass, MonitorCog, Pencil, Plus, Trash2 } from 'lucide-react';

import { OutputFromDeviceSection } from '../devices/FromDevice';
import { useAudioRouting } from '../resources/audioRouting';
import { getResource, whepPlayUrl } from '../resources/api';
import {
  useDeleteResource,
  useOutputLink,
  useOutputs,
  useSaveResource,
} from '../resources/queries';
import type { ResourceContext } from '../resources/queries';
import { OUTPUT_KINDS } from '../resources/types';
import type { OutputKind, OutputView } from '../resources/types';
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
import { useOutputPlayback } from '../resources/outputPlayback';
import { HelpLink } from '../components/HelpLink';
import { TaskWorkspace, TouchAction } from '../components/broadcast';
import { useQuery } from '@tanstack/react-query';
import { getRoutingState } from '../api/outputRouting';
import type { OutputRouteState } from '../api/outputRouting';
import { useMediaPlayers } from '../api/media-playersQueries';
import { createApiClient } from '../api/client';
import { useLayouts } from '../api/queries';
import { useSources } from '../resources/queries';
import { OutputFeedRow } from './OutputFeedRow';
import { OutputAudioRow } from './OutputAudioRow';
import { OutputFormatFields } from './OutputFormatFields';
import { formatError, resetsLive } from '../resources/outputFormat';
import type { FeedChoice } from './OutputFeedRow';
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
  const linkQuery = useOutputLink(id, true);
  const link = linkQuery.data;
  // A failing poll keeps the last good snapshot in cache; never present it as
  // the current link state.
  const pollFailing = linkQuery.isError;
  if (link === undefined || link === null) {
    if (pollFailing) {
      return (
        <div className="flex flex-col gap-0.5 text-xs" data-testid="output-link">
          <span className="text-destructive" data-testid="output-link-unavailable">
            <Trans>Link state unavailable — the status poll is failing.</Trans>
          </span>
        </div>
      );
    }
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
      {pollFailing ? (
        <span className="text-destructive" data-testid="output-link-stale">
          <Trans>Stale — last successful poll; the status poll is failing.</Trans>
        </span>
      ) : null}
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
          <div className="grid grid-cols-1 gap-3 sm:grid-cols-3">
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
          placeholder={t`Conspect Program`}
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
  if (outputId !== '' && playback.data?.unavailable === true) {
    return (
      <p className="text-sm text-muted-foreground" role="status">
        {t`Playback URLs are unavailable: this server does not provide them yet (backend not deployed).`}
      </p>
    );
  }
  if (outputId === '' || playback.data === undefined || playback.data.urls.length === 0) {
    return null;
  }
  return (
    <div className="flex flex-col gap-2">
      <p className="text-sm font-medium">{t`Playback URL`}</p>
      {playback.data.urls.map((entry, index) => (
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
        <div className="grid grid-cols-1 gap-3 sm:grid-cols-3">
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
        <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
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

/**
 * One output's compact broadcast card (TaskWorkspace): the transport kind,
 * destination, codec and runnability facts stay in the phone fold with the
 * 48px Edit/Delete keys; the secondary honesty notes sit behind the
 * caller-controlled Details disclosure. Nothing here invents a live fact:
 * every value shown is the CONFIGURED document (requested), never a claim
 * about what the running engine currently serves.
 */
function OutputCard({
  output,
  playbackPath,
  playbackCollision,
  detailsOpen,
  onDetailsToggle,
  onEdit,
  onDelete,
  embedded = false,
  feedRow,
}: {
  readonly embedded?: boolean;
  /** The live Video/Audio feed row (standalone page only). */
  readonly feedRow?: JSX.Element | undefined;
  readonly output: OutputView;
  readonly playbackPath: string | undefined;
  readonly playbackCollision: boolean;
  readonly detailsOpen: boolean;
  readonly onDetailsToggle: (open: boolean) => void;
  readonly onEdit: (row: OutputView) => void;
  readonly onDelete: (row: OutputView) => void;
}): JSX.Element {
  const { t } = useLingui();
  // An unknown-kind document renders + deletes normally but Edit is refused —
  // editing through a fold would silently rewrite the authored document.
  // `aria-disabled` (not `disabled`) keeps the control focusable so the
  // reason is discoverable by keyboard/SR users; the press is a guarded
  // no-op and the stored document stays as authored.
  const editRefused = !output.editable;
  const editDisabledReason = editRefused
    ? t`kind "${output.rawKind}" isn't editable in this UI; the document is preserved as authored`
    : undefined;
  if (embedded) {
    // Approved Routing composition: Requested (config) beside Effective
    // (measured). Only configured facts are shown as requested; effective and
    // restart state have no readback in the API, so they are Unknown.
    const unbuilt = output.editable && OUTPUT_RUNNABLE[output.kind] === 'unbuilt';
    return (
      <div className="out-card" data-testid={`output-card-${output.id}`}>
        <div className="out-head">
          <b lang="" dir="auto">{output.name}</b>
          <span className="pill pill-idle" data-testid="output-kind-tag">{output.rawKind}</span>
        </div>
        <p className="out-sub">
          <code lang="" dir="auto">{output.id}</code>
          {output.target !== undefined ? (
            <>
              {' · '}
              <span lang="" dir="auto" className="break-all">{output.target}</span>
            </>
          ) : null}
        </p>
        <div className="out-tx" data-testid="output-transmission">
          {unbuilt ? (
            <span className="pill pill-nosig"><Trans>⊘ Not available</Trans></span>
          ) : (
            <span className="pill pill-nosig"><Trans>Transmission: Unknown</Trans></span>
          )}
          {output.editable ? (
            <span className="flex min-w-0 flex-col gap-1">
              <RunnabilityNote kind={output.kind} />
              {hasLiveLink(output.kind) ? <OutputLinkStatus id={output.id} /> : null}
            </span>
          ) : (
            <span aria-hidden="true">—</span>
          )}
        </div>
        <div className="req-eff">
          <div data-testid="output-requested">
            <span className="lbl"><Trans>Requested (config)</Trans></span>
            <span className="mono">
              {output.codec ?? <Trans>codec not set</Trans>}
            </span>
          </div>
          <div data-testid="output-effective">
            <span className="lbl"><Trans>Effective (measured)</Trans></span>
            <span className="mono">
              <Trans>Unknown — no measured readback</Trans>
            </span>
          </div>
        </div>
        <p className="out-flag" data-testid="output-restart">
          <span className="pill pill-idle"><Trans>Restart: Unknown</Trans></span>
          <span><Trans>The API exposes no per-output restart readback.</Trans></span>
        </p>
        <div className="out-flag">
          <Link className="underline" to="/monitoring">
            <Trans>Watch program</Trans>
          </Link>
          {playbackPath !== undefined ? (
            <a className="underline" href={playbackPath} target="_blank" rel="noreferrer">
              <Trans>Open HLS playlist</Trans>
            </a>
          ) : null}
        </div>
        {playbackCollision ? (
          <p className="out-flag">
            <Trans>
              HLS link unavailable: output IDs resolve to colliding mounts; the
              runtime mount depends on configuration order.
            </Trans>
          </p>
        ) : null}
        <p className="out-flag">
          <Trans>Requires an active output; saved settings do not confirm playback.</Trans>
        </p>
        <button
          type="button"
          className="plan-tile"
          aria-disabled="true"
          data-testid="output-plan-tile"
          title={t`Output sinks are not routing targets: the routing API accepts video cells, audio tracks and subtitle layers only.`}
        >
          <Trans>Plan a source → {output.name}</Trans>
          <span className="plan-tile-why">
            <Trans>Unavailable — no output routing target in the API</Trans>
          </span>
        </button>
        <div className="out-actions">
          <TouchAction
            variant="outline"
            aria-label={
              editRefused
                ? `${t`Edit output`}: ${output.name} — ${editDisabledReason ?? ''}`
                : `${t`Edit output`}: ${output.name}`
            }
            aria-disabled={editRefused}
            {...(editRefused ? { title: editDisabledReason } : {})}
            {...(editRefused ? { className: 'cursor-not-allowed opacity-50' } : {})}
            onPress={(): void => {
              if (editRefused) {
                return;
              }
              onEdit(output);
            }}
          >
            <Pencil aria-hidden="true" />
            <Trans>Edit</Trans>
          </TouchAction>
          <TouchAction
            variant="ghost"
            aria-label={`${t`Delete output`}: ${output.name}`}
            onPress={(): void => {
              onDelete(output);
            }}
          >
            <Trash2 aria-hidden="true" />
            <Trans>Delete</Trans>
          </TouchAction>
        </div>
      </div>
    );
  }
  return (
    <div data-testid={`output-card-${output.id}`}>
      <TaskWorkspace
      title={
        <span className="flex min-w-0 flex-wrap items-baseline gap-x-2 break-words">
          <span lang="" dir="auto" className="font-medium">
            {output.name}
          </span>
          <code className="min-w-0 break-all text-xs text-muted-foreground" lang="" dir="auto">{output.id}</code>
        </span>
      }
      status={
        <span className="flex min-w-0 flex-wrap items-center gap-x-3 gap-y-1 break-words">
          <Badge variant="outline">{output.rawKind}</Badge>
          <span className="min-w-0">
            <Trans>Destination</Trans>:{' '}
            {output.target !== undefined ? (
              <code className="break-all rounded bg-muted px-1 py-0.5 font-mono" lang="" dir="auto">
                {output.target}
              </code>
            ) : (
              <span>—</span>
            )}
          </span>
          {output.codec !== undefined ? (
            <span>
              <Trans>Codec</Trans>: <span className="font-mono">{output.codec}</span>
            </span>
          ) : null}
          {output.editable ? (
            <span className="flex min-w-0 flex-col gap-1">
              <RunnabilityNote kind={output.kind} />
              {hasLiveLink(output.kind) ? <OutputLinkStatus id={output.id} /> : null}
            </span>
          ) : (
            // Runnability is defined for the kinds this build knows; an unknown
            // kind gets an honest dash, never a claim about the folded kind.
            <span aria-hidden="true">—</span>
          )}
        </span>
      }
      details={
        <div className="grid gap-1 text-xs text-muted-foreground">
          {playbackCollision ? (
            <p className="break-words">
              <Trans>
                HLS link unavailable: output IDs resolve to colliding mounts; the
                runtime mount depends on configuration order.
              </Trans>
            </p>
          ) : null}
          {playbackPath === undefined &&
          !playbackCollision &&
          (output.kind === 'hls' || output.kind === 'll-hls') ? (
            <p className="break-words">
              <Trans>
                Output playback unavailable: the configured playlist path is
                missing.
              </Trans>
            </p>
          ) : null}
          <p className="break-words">
            <Trans>
              Requires an active output; saved settings do not confirm playback.
            </Trans>
          </p>
        </div>
      }
      detailsOpen={detailsOpen}
      onDetailsToggle={onDetailsToggle}
    >
      <div className="grid gap-2">
        {feedRow}
        <div className="flex flex-wrap items-center gap-3 text-sm">
          <Link className="underline" to="/monitoring">
            <Trans>Watch program</Trans>
          </Link>
          {playbackPath !== undefined ? (
            <a className="underline" href={playbackPath} target="_blank" rel="noreferrer">
              <Trans>Open HLS playlist</Trans>
            </a>
          ) : null}
        </div>
        <div className="flex flex-wrap items-start gap-2">
          <TouchAction
            variant="outline"
            aria-label={
              editRefused
                ? `${t`Edit output`}: ${output.name} — ${editDisabledReason ?? ''}`
                : `${t`Edit output`}: ${output.name}`
            }
            aria-disabled={editRefused}
            {...(editRefused ? { title: editDisabledReason } : {})}
            {...(editRefused ? { className: 'cursor-not-allowed opacity-50' } : {})}
            onPress={(): void => {
              if (editRefused) {
                return;
              }
              onEdit(output);
            }}
          >
            <Pencil aria-hidden="true" />
            <Trans>Edit</Trans>
          </TouchAction>
          <TouchAction
            variant="ghost"
            aria-label={`${t`Delete output`}: ${output.name}`}
            onPress={(): void => {
              onDelete(output);
            }}
          >
            <Trash2 aria-hidden="true" />
            <Trans>Delete</Trans>
          </TouchAction>
        </div>
      </div>
    </TaskWorkspace>
    </div>
  );
}

const GENERATOR_KINDS = new Set(['bars', 'test', 'solid', 'clock', 'timer']);

/** The routable feeds grouped Players · Generators · Sources (§8). */
function useFeedChoices(
  sources: readonly { readonly id: string; readonly name: string; readonly rawKind: string }[] | undefined,
  players: readonly { readonly id: string }[] | undefined,
  layouts: readonly { readonly id: string; readonly name: string }[] | undefined,
): readonly FeedChoice[] {
  const playerIds = new Set((players ?? []).map((p) => p.id));
  // Stored layouts route as their own aux canvas (ADR-0108 slice 3).
  const layoutChoices = (layouts ?? []).map(
    (layout): FeedChoice => ({ key: `aux:${layout.id}`, label: layout.name, group: 'layouts' }),
  );
  return [...layoutChoices, ...(sources ?? []).map((source): FeedChoice => {
    const group: FeedChoice['group'] =
      playerIds.has(source.id) || source.rawKind === 'media_player'
        ? 'players'
        : GENERATOR_KINDS.has(source.rawKind)
          ? 'generators'
          : 'sources';
    return { key: `src:${source.id}`, label: source.name, group };
  })];
}

const RESOURCE_CONTEXT: ResourceContext = {};

const NO_ERRORS = {};

/** Outputs management (standalone `/outputs` route). */
export function OutputsPage(): JSX.Element {
  return <OutputsSurface />;
}

/**
 * The Outputs controller + cards. `embedded` renders it as the Routing page's
 * "Output sinks" block (block heading + New action instead of the page
 * header); the CRUD dialogs, ETag flows and cards are identical in both.
 */
export function OutputsSurface({
  embedded = false,
}: {
  readonly embedded?: boolean;
}): JSX.Element {
  const { t } = useLingui();
  const outputs = useOutputs();
  const routing = useAudioRouting();
  // Live feed rows on the standalone page only (the embedded Routing column
  // keeps its own plan tile).
  const routeState = useQuery({
    queryKey: ['routing', 'state'],
    queryFn: () => getRoutingState(),
    enabled: !embedded,
    refetchInterval: 2000,
  });
  const sources = useSources();
  const players = useMediaPlayers();
  const apiClient = useMemo(() => createApiClient(), []);
  const layouts = useLayouts(apiClient);
  const feedChoices = useFeedChoices(
    embedded ? undefined : sources.data,
    embedded ? undefined : players.data,
    embedded ? undefined : layouts.data,
  );
  const routeOf = (id: string): OutputRouteState | undefined =>
    routeState.data?.outputs.find((row) => row.output === id);
  const selectableTracks = routing.data?.state.selectable_tracks;
  // Discrete tracks a routed audio feed may carry (the program bus `prog` is
  // the "Program mix" item); unknown until audio-routing has loaded.
  const audioTracks = useMemo(
    () => selectableTracks?.filter((track) => track !== 'prog'),
    [selectableTracks],
  );
  const audioSources = useMemo(
    () => (embedded ? [] : (sources.data ?? []).map((s) => ({ id: s.id, label: s.name }))),
    [embedded, sources.data],
  );
  const playbackPaths = hlsPlaybackPaths(outputs.data ?? []);
  const playbackCollisions = hlsPlaybackCollisionIds(outputs.data ?? []);
  const save = useSaveResource('outputs', RESOURCE_CONTEXT);
  const remove = useDeleteResource('outputs', RESOURCE_CONTEXT);

  const [form, setForm] = useState<OutputFormState | null>(null);
  const [creating, setCreating] = useState(true);
  const [pendingDelete, setPendingDelete] = useState<OutputView | null>(null);
  const [errors, setErrors] = useState<FieldErrors<OutputField>>(NO_ERRORS);
  // Each card's Details disclosure is caller-owned state (the TaskWorkspace
  // controlled-disclosure contract).
  const [detailsOpen, setDetailsOpen] = useState<Record<string, boolean>>({});

  const openCreate = (): void => {
    setCreating(true);
    setErrors(NO_ERRORS);
    setForm(emptyOutputForm());
  };

  const openEdit = (row: OutputView): void => {
    setCreating(false);
    setErrors(NO_ERRORS);
    void getResource('outputs', row.id, RESOURCE_CONTEXT)
      .then((result): void => {
        const parsed = outputFormFromRecord(result.record);
        if (parsed === undefined) {
          // Defensive backstop: the card's Edit is already refused for an
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

  const closeForm = (): void => {
    setForm(null);
    setErrors(NO_ERRORS);
  };

  const submitForm = (): void => {
    if (form === null) {
      return;
    }
    const found = validateOutputForm(form, creating);
    setErrors(found);
    if (Object.keys(found).length > 0 || formatError(form) !== undefined) {
      return;
    }
    const liveReset = !creating && resetsLive(form.kind);
    save.mutate(
      {
        id: creating ? form.id.trim() : form.id,
        create: creating,
        input: { name: form.name.trim(), body: outputFormToBody(form) },
      },
      {
        onSuccess: (): void => {
          toast({
            title: creating ? t`Created` : t`Saved`,
            description: liveReset
              ? t`Stored. A format or bitrate change resets this output (about 1 s); Program and other outputs are not affected. Any other change applies on the next restart.`
              : t`Stored. It goes live via config export + restart; the running engine is unchanged until then.`,
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
        toast({
          title: t`Deleted`,
          description: t`Removed from the store. The running engine is unchanged until a config export + restart.`,
        });
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

  const hasErrors = Object.keys(errors).length > 0;

  return (
    <>
      {embedded ? (
        <div className="block-head" data-testid="output-sinks-head">
          <h2>
            <Trans>Output sinks</Trans>
          </h2>
          <div data-testid="output-header-actions" className="flex min-w-0 max-w-full flex-wrap items-center gap-2">
            <ExportConfigButton compact />
            <Button data-testid="crud-new" className="h-auto min-h-12 min-w-12 max-w-full whitespace-normal break-words py-2" onClick={openCreate}>
              <Plus aria-hidden="true" />
              <Trans>New output</Trans>
            </Button>
          </div>
        </div>
      ) : (
      <div
        data-testid="output-page-header"
        className="min-w-0 max-w-full [&>div>div]:min-w-0 [&>div>div]:max-w-full"
      >
      <PageHeader
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
        actions={
          <div data-testid="output-header-actions" className="flex min-w-0 max-w-full flex-wrap items-center gap-2">
            <ExportConfigButton compact />
            <Button data-testid="crud-new" className="h-auto min-h-12 min-w-12 max-w-full whitespace-normal break-words py-2" onClick={openCreate}>
              <Plus aria-hidden="true" />
              <Trans>New output</Trans>
            </Button>
          </div>
        }
      />
      </div>
      )}

      <ApplySemanticsCallout
        helpTo="/help/config#outputs"
        helpLabel={t`How configuration applies`}
      />

      {outputs.isPending ? (
        <p role="status" aria-live="polite" className="text-sm text-muted-foreground">
          <Trans>Loading outputs…</Trans>
        </p>
      ) : outputs.isError ? (
        <p role="alert" className="text-sm text-destructive">
          <Trans>Could not load outputs:</Trans> {outputs.error.message}
        </p>
      ) : outputs.data.length === 0 ? (
        <p className="text-sm text-muted-foreground">
          <Trans>No outputs configured.</Trans>
        </p>
      ) : (
        <div>
          <p className="mb-3 text-xs text-muted-foreground">{t`Configured output sinks.`}</p>
          <div className={embedded ? 'out-cards' : 'grid gap-4'}>
            {outputs.data.map((output) => (
              <OutputCard
                key={output.id}
                embedded={embedded}
                output={output}
                playbackPath={playbackPaths.get(output.id)}
                playbackCollision={playbackCollisions.has(output.id)}
                detailsOpen={detailsOpen[output.id] === true}
                onDetailsToggle={(open: boolean): void => {
                  setDetailsOpen((prev) => ({ ...prev, [output.id]: open }));
                }}
                feedRow={
                  embedded ? undefined : (
                    <OutputFeedRow
                      output={output.id}
                      outputName={output.name}
                      route={routeOf(output.id)}
                      choices={feedChoices}
                      audioRow={
                        <OutputAudioRow
                          output={output.id}
                          outputName={output.name}
                          route={routeOf(output.id)}
                          sources={audioSources}
                          tracks={audioTracks}
                        />
                      }
                    />
                  )
                }
                onEdit={openEdit}
                onDelete={(row: OutputView): void => {
                  setPendingDelete(row);
                }}
              />
            ))}
          </div>
        </div>
      )}

      <Dialog
        open={form !== null}
        onOpenChange={(open): void => {
          if (!open) {
            closeForm();
          }
        }}
      >
        <DialogContent touchScroll>
          <DialogHeader>
            <DialogTitle>
              {creating ? <Trans>New output</Trans> : <Trans>Edit output</Trans>}
            </DialogTitle>
            <DialogDescription>
              <Trans>An output is a sink/server that publishes the program.</Trans>
            </DialogDescription>
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
              <div className="flex min-w-0 flex-col gap-4">
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
                <OutputFormatFields form={form} setForm={setForm} editing={!creating} />
                <OutputAdvancedFields
                  form={form}
                  setForm={setForm}
                  errors={errors}
                  selectableTracks={selectableTracks}
                />
                {!creating ? <PlaybackUrlsSection outputId={form.id} /> : null}
              </div>
              {hasErrors ? (
                <p role="alert" className="text-sm text-destructive">
                  <Trans>Fix the highlighted fields, then save again.</Trans>
                </p>
              ) : null}
              <DialogFooter>
                <TouchAction variant="outline" onPress={closeForm}>
                  <Trans>Cancel</Trans>
                </TouchAction>
                <Button
                  type="submit"
                  className="h-12 min-w-12"
                  disabled={save.isPending}
                >
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
        <DialogContent touchScroll>
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
                {pendingDelete.name}
              </span>
            </p>
          ) : null}
          <DialogFooter>
            <TouchAction
              variant="outline"
              onPress={(): void => {
                setPendingDelete(null);
              }}
            >
              <Trans>Cancel</Trans>
            </TouchAction>
            <Button
              variant="destructive"
              className="h-12 min-w-12"
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