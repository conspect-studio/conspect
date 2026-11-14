// The output's picture format and slots (ADR-0110, output-routing §12.3, #342):
// width × height, fit mode + anchor, three presets, and the slot list (video
// codec + bitrate editable; audio slots read-only with language/track).
//
// The values live in the stored body as `format` and `slots` (round-tripped
// through the form's `extra`, so an unknown key is never dropped). Applying a
// changed format or video bitrate resets THIS output only on a kind that
// carries its parameter sets in band (HLS, SRT, RIST); every other kind applies
// on the next restart — the warning below says which, before Save.
import type { JSX } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import { Plus, Trash2 } from 'lucide-react';
import { Button } from '../components/ui/button';
import { FormField, SelectField } from '../resources/FormControls';
import type { OutputFormState } from '../resources/forms';
import {
  ANCHORS,
  audioSlotsOf,
  bitrateOf,
  FIT_MODES,
  FORMAT_PRESETS,
  formatError,
  formatOf,
  hasEncodedVideo,
  MAX_SUBTITLE_SLOTS,
  resetsLive,
  SUBTITLE_FORMATS,
  subtitleCapabilityOf,
  subtitleEffectiveName,
  subtitleFollowUpIssue,
  subtitleSlotError,
  subtitleSlotsOf,
  subtitleUnsupportedMedium,
  withBitrate,
  withFormat,
  withSubtitleSlots,
} from '../resources/outputFormat';
import type { Anchor, FitMode, SubtitleFormat, SubtitleSlotRow } from '../resources/outputFormat';

/** Props for {@link OutputFormatFields}. */
export interface OutputFormatFieldsProps {
  readonly form: OutputFormState;
  readonly setForm: (next: OutputFormState) => void;
  /** Whether the dialog edits an existing output (the warning applies). */
  readonly editing: boolean;
  /** Declared sources, for the subtitle slot source select. */
  readonly sources?: readonly { readonly id: string; readonly label: string }[] | undefined;
}

/** The format editor, slot list and apply warning of the output dialog. */
export function OutputFormatFields({
  form,
  setForm,
  editing,
  sources,
}: OutputFormatFieldsProps): JSX.Element | null {
  const { t } = useLingui();
  if (!hasEncodedVideo(form.kind)) return null;
  const format = formatOf(form) ?? { width: '', height: '', mode: 'contain', anchor: 'center' };
  const bitrate = bitrateOf(form);
  const audioSlots = audioSlotsOf(form);
  const error = formatError(form);
  const set = (patch: Partial<typeof format>): void => {
    setForm(withFormat(form, { ...format, ...patch }));
  };
  return (
    <fieldset className="grid gap-3 rounded-md border p-3" data-testid="output-format-fields">
      <legend className="px-1 text-sm font-medium">
        <Trans>Picture format and slots</Trans>
      </legend>
      <div className="flex flex-wrap gap-2" role="group" aria-label={t`Format presets`}>
        {FORMAT_PRESETS.map((preset) => (
          <button
            key={preset.id}
            type="button"
            className="h-10 rounded-md border px-3 text-sm"
            onClick={(): void => {
              setForm(
                withFormat(form, {
                  width: String(preset.width),
                  height: String(preset.height),
                  mode: preset.mode,
                  anchor: preset.anchor,
                }),
              );
            }}
          >
            {preset.mode === 'cover' ? (
              <Trans>{preset.width}×{preset.height} cover centre</Trans>
            ) : (
              <>
                {preset.width}×{preset.height}
              </>
            )}
          </button>
        ))}
        <button
          type="button"
          className="h-10 rounded-md border px-3 text-sm"
          onClick={(): void => {
            setForm(withFormat(form, { width: '', height: '', mode: 'contain', anchor: 'center' }));
          }}
        >
          <Trans>Program canvas</Trans>
        </button>
      </div>
      <div className="grid grid-cols-2 gap-3">
        <FormField
          id="output-format-width"
          label={t`Width (px)`}
          value={format.width}
          placeholder={t`Program canvas`}
          onChange={(next): void => {
            set({ width: next });
          }}
        />
        <FormField
          id="output-format-height"
          label={t`Height (px)`}
          value={format.height}
          placeholder={t`Program canvas`}
          onChange={(next): void => {
            set({ height: next });
          }}
        />
      </div>
      <div className="grid grid-cols-2 gap-3">
        <SelectField<FitMode>
          label={t`Fit`}
          value={format.mode}
          options={FIT_MODES}
          testId="output-format-fit"
          optionLabel={(mode): JSX.Element =>
            mode === 'contain' ? (
              <Trans>Contain (letterbox)</Trans>
            ) : mode === 'cover' ? (
              <Trans>Cover (zoom and crop)</Trans>
            ) : (
              <Trans>Fill (stretch)</Trans>
            )
          }
          onChange={(mode): void => {
            set({ mode });
          }}
        />
        <SelectField<Anchor>
          label={t`Anchor`}
          value={format.anchor}
          options={ANCHORS}
          testId="output-format-anchor"
          onChange={(anchor): void => {
            set({ anchor });
          }}
        />
      </div>
      {error === 'size' ? (
        <p role="alert" className="text-sm text-destructive">
          <Trans>
            Width and height must both be even numbers from 16 to 8192, at most 3840 × 2160 pixels in
            area.
          </Trans>
        </p>
      ) : null}
      <div className="grid gap-2">
        <p className="text-sm font-medium">
          <Trans>Slots</Trans>
        </p>
        <div className="grid grid-cols-2 gap-3">
          <p className="text-sm">
            <Trans>Video slot 0: codec {form.codec || '—'}</Trans>
          </p>
          <FormField
            id="output-slot-bitrate"
            label={t`Video bitrate (kbit/s)`}
            value={bitrate}
            placeholder="4000"
            onChange={(next): void => {
              setForm(withBitrate(form, next));
            }}
          />
        </div>
        {error === 'bitrate' ? (
          <p role="alert" className="text-sm text-destructive">
            <Trans>The bitrate must be a whole number from 1 to 1000000 kbit/s.</Trans>
          </p>
        ) : null}
        <ul className="grid gap-1 text-sm text-muted-foreground" aria-label={t`Audio slots`}>
          {audioSlots.length === 0 ? (
            <li>
              <Trans>Audio slot 0: program mix</Trans>
            </li>
          ) : (
            audioSlots.map((slot, index) => (
              <li key={index}>
                {index === 0 ? (
                  <Trans>Audio slot 0: program mix</Trans>
                ) : (
                  <Trans>
                    Audio slot {index}: track {slot.track ?? '—'}
                  </Trans>
                )}
                {slot.language !== undefined ? <> ({slot.language})</> : null}
              </li>
            ))
          )}
        </ul>
        <SubtitleSlots form={form} setForm={setForm} sources={sources} />
      </div>
      {editing ? (
        <p role="note" className="text-sm" data-testid="output-format-warning">
          {resetsLive(form.kind) ? (
            <Trans>
              Applying resets this output (about 1 s); Program and other outputs are not affected.
            </Trans>
          ) : (
            <Trans>Applies on the next restart.</Trans>
          )}
        </p>
      ) : null}
    </fieldset>
  );
}

/** Sentinel source-select value for an unpatched slot (Radix forbids ''). */
const UNPATCHED = '__unpatched__';

/** Props for {@link SubtitleSlots}. */
interface SubtitleSlotsProps {
  readonly form: OutputFormState;
  readonly setForm: (next: OutputFormState) => void;
  readonly sources?: readonly { readonly id: string; readonly label: string }[] | undefined;
}

/**
 * The subtitle slot editor (`extra.slots.subtitle`, ADR-0110 §12.3/§12.8,
 * #345 part F3): add/remove rows (at most {@link MAX_SUBTITLE_SLOTS}), each
 * with a format, language, optional name and a source select, capability-
 * aware per the output kind (mirrors `SlotCapability` in
 * `crates/multiview-config/src/output_format.rs`).
 */
function SubtitleSlots({ form, setForm, sources }: SubtitleSlotsProps): JSX.Element | null {
  const { t } = useLingui();
  const cap = subtitleCapabilityOf(form.kind);
  const rows = subtitleSlotsOf(form);
  if (cap.max === 0) {
    if (!subtitleUnsupportedMedium(form.kind) || rows.length === 0) return null;
    return (
      <p role="alert" className="text-sm text-destructive" data-testid="subtitle-slots-medium-mismatch">
        <Trans>This output's medium carries no subtitles.</Trans>
      </p>
    );
  }
  const set = (next: readonly SubtitleSlotRow[]): void => {
    setForm(withSubtitleSlots(form, next));
  };
  const addRow = (): void => {
    set([...rows, { format: cap.builtFormats[0] ?? 'webvtt', language: '', name: '', source: undefined }]);
  };
  const removeRow = (index: number): void => {
    set(rows.filter((_, i) => i !== index));
  };
  const updateRow = (index: number, patch: Partial<SubtitleSlotRow>): void => {
    set(rows.map((row, i) => (i === index ? { ...row, ...patch } : row)));
  };
  return (
    <div className="grid gap-2" data-testid="subtitle-slots">
      <div className="flex items-center justify-between">
        <p className="text-sm font-medium">
          <Trans>Subtitle slots</Trans>
        </p>
        <Button
          type="button"
          variant="outline"
          size="sm"
          disabled={rows.length >= Math.min(cap.max, MAX_SUBTITLE_SLOTS)}
          onClick={addRow}
        >
          <Plus aria-hidden="true" />
          <Trans>Add subtitle slot</Trans>
        </Button>
      </div>
      {rows.length === 0 ? (
        <p className="text-sm text-muted-foreground">
          <Trans>No subtitle slots.</Trans>
        </p>
      ) : (
        <ul className="grid gap-3">
          {rows.map((row, index) => {
            const errorCode = subtitleSlotError(form.kind, rows, index);
            return (
              <li key={index} className="grid gap-2 rounded-md border p-2" data-testid={`subtitle-slot-${String(index)}`}>
                <div className="grid grid-cols-2 gap-2 sm:grid-cols-4">
                  <SelectField<SubtitleFormat>
                    label={t`Format`}
                    value={row.format}
                    options={SUBTITLE_FORMATS}
                    testId={`subtitle-slot-${String(index)}-format`}
                    onChange={(format): void => {
                      updateRow(index, { format });
                    }}
                  />
                  <FormField
                    id={`subtitle-slot-${String(index)}-language`}
                    label={t`Language`}
                    value={row.language}
                    placeholder="en"
                    onChange={(language): void => {
                      updateRow(index, { language });
                    }}
                  />
                  <FormField
                    id={`subtitle-slot-${String(index)}-name`}
                    label={t`Name`}
                    value={row.name}
                    placeholder={subtitleEffectiveName(row, index)}
                    onChange={(name): void => {
                      updateRow(index, { name });
                    }}
                  />
                  <SelectField<string>
                    label={t`Source`}
                    value={row.source ?? UNPATCHED}
                    options={[UNPATCHED, ...(sources ?? []).map((s) => s.id)]}
                    testId={`subtitle-slot-${String(index)}-source`}
                    optionLabel={(id): string =>
                      id === UNPATCHED ? t`Unpatched` : sources?.find((s) => s.id === id)?.label ?? id
                    }
                    onChange={(source): void => {
                      updateRow(index, { source: source === UNPATCHED ? undefined : source });
                    }}
                  />
                </div>
                {errorCode === 'format' ? (
                  (() => {
                    const issue = subtitleFollowUpIssue(form.kind, row.format);
                    return (
                      <p role="alert" className="text-sm text-destructive">
                        {cap.mediumFormats.includes(row.format) ? (
                          issue === undefined ? (
                            <Trans>This format is not supported on this output kind.</Trans>
                          ) : (
                            <Trans>
                              Not supported yet on this output kind (follow-up #{issue}).
                            </Trans>
                          )
                        ) : (
                          <Trans>This format does not match this output's medium.</Trans>
                        )}
                      </p>
                    );
                  })()
                ) : errorCode === 'language' ? (
                  <p role="alert" className="text-sm text-destructive">
                    <Trans>
                      Language must be 1–35 characters of letters, digits and hyphens.
                    </Trans>
                  </p>
                ) : errorCode === 'name' ? (
                  <p role="alert" className="text-sm text-destructive">
                    <Trans>Name must be at most 64 printable characters, no quotes.</Trans>
                  </p>
                ) : errorCode === 'duplicate-name' ? (
                  <p role="alert" className="text-sm text-destructive">
                    <Trans>
                      Another slot already uses this menu label (name, else language); give this
                      slot a distinct name or language.
                    </Trans>
                  </p>
                ) : null}
                <div>
                  <Button
                    type="button"
                    variant="ghost"
                    size="sm"
                    aria-label={t`Remove subtitle slot ${index + 1}`}
                    onClick={(): void => {
                      removeRow(index);
                    }}
                  >
                    <Trash2 aria-hidden="true" />
                    <Trans>Remove</Trans>
                  </Button>
                </div>
              </li>
            );
          })}
        </ul>
      )}
    </div>
  );
}
