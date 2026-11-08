/* eslint-disable react-refresh/only-export-components -- the warning threshold lives beside the control that uses it */
// The explicit "Always on / On demand" choice for the IPTV-ORG add flows.
//
// Nothing is preselected: the operator says how the sources start, and the add
// control stays disabled until they do (`docs/io/iptv-org.md`, Integration
// rules, item 3). The control, wording and large-selection warning mirror the
// M3U import dialog's, so both import paths ask the same question the same way.
import type { JSX } from 'react';
import { Trans } from '@lingui/react/macro';

import type { SourceActivationChoice } from '../resources/forms';

/**
 * Above this many always-on sources the choice warns about the cost. The same
 * threshold as the M3U import dialog (`LARGE_ALWAYS_ON` in
 * `M3uImportDialog.tsx`); each always-on source holds a connection and a
 * decoder from boot.
 */
export const LARGE_ALWAYS_ON = 20;

/** The radio pair both catalogue add flows use. */
export function ActivationChoice({
  value,
  onChange,
  count,
  name,
}: {
  /** The chosen activation, or null while nothing is chosen (no default). */
  readonly value: SourceActivationChoice | null;
  readonly onChange: (next: SourceActivationChoice) => void;
  /** How many sources the choice covers (drives the large-selection warning). */
  readonly count: number;
  /** The radio group name; unique per mounted dialog. */
  readonly name: string;
}): JSX.Element {
  return (
    <fieldset className="space-y-1 text-sm" data-testid="catalogue-activation">
      <legend className="font-medium">
        <Trans>Start the {String(count)} selected source(s)</Trans>
      </legend>
      <label className="flex items-center gap-2">
        <input
          type="radio"
          name={name}
          checked={value === 'on_demand'}
          onChange={(): void => {
            onChange('on_demand');
          }}
        />
        <Trans>On demand: idle and free until something uses them</Trans>
      </label>
      <label className="flex items-center gap-2">
        <input
          type="radio"
          name={name}
          checked={value === 'always_on'}
          onChange={(): void => {
            onChange('always_on');
          }}
        />
        <Trans>Always on: each opens a connection and decodes from boot</Trans>
      </label>
      {value === 'always_on' && count > LARGE_ALWAYS_ON ? (
        <p role="alert" className="text-xs text-destructive">
          <Trans>
            {String(count)} always-on sources open {String(count)} connections and decoders at once.
            Choose On demand unless every one must run all the time.
          </Trans>
        </p>
      ) : null}
    </fieldset>
  );
}
