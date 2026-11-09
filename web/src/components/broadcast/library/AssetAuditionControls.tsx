// AssetAuditionControls — the presentational audition surface for one
// library asset. The evidence audit is unambiguous here (library-evidence
// §6): NO per-asset audition, scrub-audio endpoint, waveform-peaks service
// or audio spectrogram exists in the repository today. Therefore:
//   * the whole panel is capability-gated: `auditionGate.supported:
//     false` renders the honest unavailable state with the caller's
//     reason — never a fake "working" transport (audit §9.10–11);
//   * playback is LOCAL ONLY and labeled as such — it is never a program
//     path and carries no on-air signal;
//   * analysis data is NEVER generated here: the caller supplies the
//     ACTUAL analysis (waveform peaks 0..1) for the active mode, or
//     `null` — the component renders an honest "no analysis" panel, and
//     in production nothing synthetic is invented in its place;
//   * the analysis MODE selector (waveform / spectrogram) is controlled:
//     each mode carries its own capability gate, so a mode without a
//     service renders disabled with its reason;
//   * every control fires an INTENT — accepted/applied truth arrives
//     back through props only.
import type { JSX } from 'react';
import { useLingui } from '@lingui/react/macro';

import { cn } from '../../../lib/utils';
import { Badge } from '../../ui/badge';
import { TouchAction } from '../TouchAction';
import type { ActionGate } from './types';

/** One selectable analysis mode (waveform or spectrogram). */
export interface AnalysisModeOption {
  readonly id: 'waveform' | 'spectrogram';
  /** Visible label (already localized by the caller). */
  readonly label: string;
  /** Capability gate: a mode without a service renders disabled + reason. */
  readonly gate: ActionGate;
}

/** Exact asset sample window. */
export interface AnalysisWindow { readonly startSample: number; readonly endSample: number; readonly sampleRateHz: number }
interface AnalysisBinding { readonly assetId: string; readonly assetRevision: string; readonly channelId: string; readonly window: AnalysisWindow; readonly measuredNote?: string | null }
export type AnalysisData =
  | (AnalysisBinding & { readonly kind: 'waveform'; readonly amplitudeUnit: 'normalized_peak'; readonly peaks: readonly number[] })
  | (AnalysisBinding & { readonly kind: 'spectrogram'; readonly frequencyUnit: 'hz'; readonly magnitudeUnit: 'dbfs'; readonly frequencyBinsHz: readonly number[]; readonly timeBins: readonly { readonly startSample: number; readonly endSample: number; readonly magnitudes: readonly number[] }[] });
function validWindow(window: AnalysisWindow): boolean { return Number.isSafeInteger(window.startSample) && window.startSample >= 0 && Number.isSafeInteger(window.endSample) && window.endSample > window.startSample && Number.isSafeInteger(window.sampleRateHz) && window.sampleRateHz > 0; }
function validAnalysis(analysis: AnalysisData | null, assetId: string, revision: string, channel: string | null, mode: string | null, window: AnalysisWindow): analysis is AnalysisData {
  if (!analysis || !validWindow(window) || !validWindow(analysis.window) || analysis.assetId !== assetId || analysis.assetRevision !== revision || analysis.channelId !== channel || analysis.kind !== mode || analysis.window.startSample !== window.startSample || analysis.window.endSample !== window.endSample || analysis.window.sampleRateHz !== window.sampleRateHz) return false;
  if (analysis.kind === 'waveform') return analysis.peaks.length > 0 && analysis.peaks.every((peak) => Number.isFinite(peak) && peak >= 0 && peak <= 1);
  return analysis.frequencyBinsHz.length > 0 && analysis.frequencyBinsHz.every((hz, index) => Number.isFinite(hz) && hz > 0 && (index === 0 || hz > (analysis.frequencyBinsHz[index - 1] ?? 0))) && analysis.timeBins.length > 0 && analysis.timeBins.every((bin, index) => Number.isSafeInteger(bin.startSample) && Number.isSafeInteger(bin.endSample) && bin.startSample >= window.startSample && bin.endSample <= window.endSample && bin.endSample > bin.startSample && (index === 0 || bin.startSample >= (analysis.timeBins[index - 1]?.endSample ?? 0)) && bin.magnitudes.length === analysis.frequencyBinsHz.length && bin.magnitudes.every((db) => Number.isFinite(db) && db <= 0));
}

/** One selectable audio channel. */
export interface ChannelOption {
  readonly id: string;
  /** Visible label (already localized by the caller). */
  readonly label: string;
}

/** Props for {@link AssetAuditionControls}. */
export interface AssetAuditionControlsProps {
  /** The asset being auditioned (for the header). */
  readonly assetName: string;
  readonly assetId: string;
  readonly assetRevision: string;
  readonly analysisWindow: AnalysisWindow;
  /**
   * Capability gate for per-asset audition itself. `supported: false`
   * renders the honest unavailable panel with the reason — the default
   * truth today (no audition endpoint exists).
   */
  readonly auditionGate: ActionGate;
  /** Transport state, caller-owned: playing + the exact position label. */
  readonly transport: {
    readonly playing: boolean;
    /** Exact-formatted position (caller's grid math); null = not reported. */
    readonly positionLabel: string | null;
  };
  /** Intent: the operator pressed play/pause (local audition only). */
  readonly onPlayPauseIntent?: (() => void) | undefined;
  /** Intent: the operator pressed stop. */
  readonly onStopIntent?: (() => void) | undefined;
  /** The caller's channel options (empty = no channel choice offered). */
  readonly channels: readonly ChannelOption[];
  /** The active channel id, controlled. */
  readonly activeChannelId: string | null;
  /** Intent: the operator switched the audition channel. */
  readonly onChannelChange?: ((channelId: string) => void) | undefined;
  /** The analysis modes offered, each with its own capability gate. */
  readonly analysisModes: readonly AnalysisModeOption[];
  /** The active analysis mode id, controlled. */
  readonly activeAnalysisModeId: string | null;
  /** Intent: the operator switched the analysis mode. */
  readonly onAnalysisModeChange?: ((modeId: string) => void) | undefined;
  /**
   * The ACTUAL analysis for the active mode; `null` renders the honest
   * no-analysis panel — never a synthetic stand-in.
   */
  readonly analysis: AnalysisData | null;
}

/**
 * Presentational audition controls: local playback intents, channel and
 * analysis-mode selection, and an analysis surface for real supplied data.
 */
export function AssetAuditionControls({
  assetName,
  assetId,
  assetRevision,
  analysisWindow,
  auditionGate,
  transport,
  onPlayPauseIntent,
  onStopIntent,
  channels,
  activeChannelId,
  onChannelChange,
  analysisModes,
  activeAnalysisModeId,
  onAnalysisModeChange,
  analysis,
}: AssetAuditionControlsProps): JSX.Element {
  const { t } = useLingui();
  const usableAnalysis = validAnalysis(analysis, assetId, assetRevision, activeChannelId, activeAnalysisModeId, analysisWindow) && analysisModes.some((mode) => mode.id === activeAnalysisModeId && mode.gate.supported);

  if (!auditionGate.supported) {
    return (
      <section
        data-testid="asset-audition-controls"
        data-state="unavailable"
        aria-label={t`Audition`}
        className="rounded-lg border border-dashed p-4"
      >
        <p className="min-w-0 break-words text-sm font-medium">
          {t`Audition is not available`}
        </p>
        {auditionGate.unavailableReason !== null ? (
          <p className="min-w-0 break-words text-xs text-muted-foreground">
            {auditionGate.unavailableReason}
          </p>
        ) : null}
      </section>
    );
  }

  return (
    <section
      data-testid="asset-audition-controls"
      data-state="available"
      data-playing={transport.playing ? 'true' : 'false'}
      aria-label={t`Audition`}
      className="flex min-w-0 flex-col gap-3"
    >
      <div className="flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1">
        <h3 className="min-w-0 break-words text-sm font-semibold">
          {assetName}
        </h3>
        <Badge variant="secondary">{t`Local audition — not a program path`}</Badge>
      </div>

      <div className="flex min-w-0 flex-wrap items-center gap-2">
        <TouchAction
          variant="default"
          disabled={onPlayPauseIntent === undefined}
          onPress={(): void => {
            onPlayPauseIntent?.();
          }}
        >
          {transport.playing ? t`Pause` : t`Play`}
        </TouchAction>
        <TouchAction
          variant="outline"
          disabled={onStopIntent === undefined}
          onPress={(): void => {
            onStopIntent?.();
          }}
        >
          {t`Stop`}
        </TouchAction>
        <p className="min-w-0 break-words font-mono text-xs text-muted-foreground">
          {transport.positionLabel ?? t`Position not reported`}
        </p>
      </div>

      {channels.length > 0 && onChannelChange !== undefined ? (
        <div
          data-testid="audition-channels"
          className="flex min-w-0 flex-wrap items-center gap-2"
        >
          {channels.map((channel) => {
            const active = channel.id === activeChannelId;
            return (
              <button
                key={channel.id}
                type="button"
                aria-pressed={active}
                onClick={(): void => {
                  onChannelChange(channel.id);
                }}
                className={cn(
                  'inline-flex min-h-12 min-w-12 items-center rounded-md border px-3 text-xs font-medium transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring',
                  active
                    ? 'border-primary bg-primary text-primary-foreground'
                    : 'border-input bg-transparent hover:bg-accent',
                )}
              >
                {channel.label}
              </button>
            );
          })}
        </div>
      ) : null}

      {analysisModes.length > 0 ? (
        <div
          data-testid="audition-analysis-modes"
          className="flex min-w-0 flex-wrap items-start gap-2"
        >
          {analysisModes.map((mode) => {
            const blocked = !mode.gate.supported;
            return (
              <div key={mode.id} className="flex min-w-0 flex-col gap-1">
                <TouchAction
                  variant={mode.id === activeAnalysisModeId ? 'default' : 'outline'}
                  disabled={blocked}
                  disabledReason={
                    blocked ? mode.gate.unavailableReason ?? undefined : undefined
                  }
                  onPress={(): void => {
                    onAnalysisModeChange?.(mode.id);
                  }}
                >
                  {mode.label}
                </TouchAction>
              </div>
            );
          })}
        </div>
      ) : null}

      <div data-testid="audition-analysis-surface" data-analysis-kind={usableAnalysis ? analysis.kind : 'none'} className="flex min-h-24 min-w-0 flex-wrap items-end gap-0.5 rounded-lg border p-2">
        {!usableAnalysis ? <p className="min-w-0 flex-1 basis-full break-words text-xs text-muted-foreground">{analysis === null ? t`No analysis for this mode — no analysis service supplied it.` : t`Analysis unavailable — stale, unbound or invalid measured data.`}</p> : analysis.kind === 'waveform' ? <>
          {analysis.peaks.map((peak, index) => <span key={index} data-testid="audition-analysis-bar" aria-hidden="true" className="w-1 shrink-0 rounded-sm bg-foreground" style={{ height: `${String(Math.max(2, Math.round(peak * 80)))}px` }} />)}
          <p className="min-w-0 flex-1 basis-full break-words text-xs text-muted-foreground">{analysis.measuredNote ?? t`Measured waveform amplitude (normalized peak).`}</p>
        </> : <div data-testid="audition-spectrogram" className="flex min-w-0 flex-col gap-1 text-xs">
          <p>{t`Spectrogram frequency bins (Hz): ${analysis.frequencyBinsHz.join(', ')} · magnitude (dBFS)`}</p>
          <div className="flex flex-wrap gap-1" aria-label={t`Measured time-frequency bins`}>{analysis.timeBins.map((bin, index) => <span key={index} className="rounded border p-1" data-testid="spectrogram-time-bin">{`${String(bin.startSample)}–${String(bin.endSample)}: ${bin.magnitudes.join(', ')} dBFS`}</span>)}</div>
          {analysis.measuredNote !== undefined && analysis.measuredNote !== null ? <p>{analysis.measuredNote}</p> : null}
        </div>}
      </div>
    </section>
  );
}
