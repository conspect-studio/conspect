// Canvas frame-rate picker: a dropdown of the common broadcast rates, each
// mapped to its EXACT rational (never a float — 29.97 is 30000/1001), plus a
// "Custom" escape hatch for any other `num/den`. The stored value is always the
// rational string the config/API expect.
import type { JSX } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import { Input } from '../../components/ui/input';
import { Label } from '../../components/ui/label';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '../../components/ui/select';
import { FRAME_RATE_PRESETS, presetFor } from '../frameRates';

const CUSTOM = '__custom__';

export interface FrameRateFieldProps {
  readonly id: string;
  readonly value: string;
  readonly invalid: boolean;
  readonly onChange: (rational: string) => void;
}

/** Dropdown of common rates + a custom rational input. */
export function FrameRateField({ id, value, invalid, onChange }: FrameRateFieldProps): JSX.Element {
  const { t } = useLingui();
  const preset = presetFor(value);
  const selected = preset?.rational ?? CUSTOM;
  return (
    <div className="flex flex-col gap-1">
      <Label htmlFor={id} className="text-xs">
        <Trans>Frame rate</Trans>
      </Label>
      <Select
        value={selected}
        onValueChange={(next): void => {
          // Choosing "Custom" keeps the current value so it can be edited.
          if (next !== CUSTOM) {
            onChange(next);
          } else if (preset !== undefined) {
            onChange(preset.rational);
          }
        }}
      >
        <SelectTrigger id={id} className="w-56" aria-invalid={invalid}>
          <SelectValue />
        </SelectTrigger>
        <SelectContent>
          {FRAME_RATE_PRESETS.map((p) => (
            <SelectItem key={p.rational} value={p.rational}>
              {p.label}
            </SelectItem>
          ))}
          <SelectItem value={CUSTOM}>{t`Custom…`}</SelectItem>
        </SelectContent>
      </Select>
      {preset === undefined ? (
        <Input
          id={`${id}-custom`}
          aria-label={t`Custom frame rate (num/den)`}
          className="w-56"
          value={value}
          placeholder="30000/1001"
          aria-invalid={invalid}
          onChange={(event): void => {
            onChange(event.target.value);
          }}
        />
      ) : null}
      <p className="text-xs text-muted-foreground">
        <Trans>Must match the running output to apply live (e.g. 29.97 = 30000/1001).</Trans>
      </p>
    </div>
  );
}
