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
  resetsLive,
  withBitrate,
  withFormat,
} from '../resources/outputFormat';
import type { Anchor, FitMode } from '../resources/outputFormat';

/** Props for {@link OutputFormatFields}. */
export interface OutputFormatFieldsProps {
  readonly form: OutputFormState;
  readonly setForm: (next: OutputFormState) => void;
  /** Whether the dialog edits an existing output (the warning applies). */
  readonly editing: boolean;
}

/** The format editor, slot list and apply warning of the output dialog. */
export function OutputFormatFields({
  form,
  setForm,
  editing,
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
          <Trans>Width and height must both be even numbers from 16 to 8192.</Trans>
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
