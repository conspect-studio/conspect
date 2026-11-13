// Numbered mixer inputs (ADR-0110 §12.2, #341): Input N · label · source with
// PGM/PVW tally from the engine's live readback, select-by-input buttons and
// a per-input re-patch control (plan → take). Re-patching the input on PGM
// changes Program, so it asks for an explicit confirm and only then sends
// `on_air: "allow"`; otherwise the server's default `protect` applies.
import { useState } from 'react';
import type { JSX } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import { Button } from '../ui/button';
import { inputSource, mixerCrosspoint } from '../../api/patch';
import type { MixerInputView, MixerPatchView, OnAir, PatchRequest } from '../../api/patch';

export interface MixerInputBankProps {
  readonly mixer: MixerPatchView;
  readonly sources: readonly { readonly id: string; readonly name: string }[];
  readonly disabled: boolean;
  readonly onSelect: (action: 'select_preview' | 'select_program', input: number) => void;
  /** Plan then take; resolves when the take was accepted (202). */
  readonly onRepatch: (request: PatchRequest) => Promise<void>;
}

function InputRow({ me, input, sources, disabled, onSelect, onRepatch }: {
  readonly me: string;
  readonly input: MixerInputView;
  readonly sources: MixerInputBankProps['sources'];
  readonly disabled: boolean;
  readonly onSelect: MixerInputBankProps['onSelect'];
  readonly onRepatch: MixerInputBankProps['onRepatch'];
}): JSX.Element {
  const { t } = useLingui();
  const [choice, setChoice] = useState('');
  const [confirming, setConfirming] = useState(false);
  const [busy, setBusy] = useState(false);
  const patched = input.video !== null;
  const sourceLabel = input.video === null ? t`Unpatched` : inputSource(input) ?? t`Restricted source`;
  const tally = input.on_program ? 'PGM' : input.on_preview ? 'PVW' : undefined;
  const send = async (onAir: OnAir): Promise<void> => {
    setBusy(true);
    try {
      await onRepatch({ crosspoints: [mixerCrosspoint(me, input.number, choice)], on_air: onAir });
      setChoice('');
    } finally {
      setBusy(false);
      setConfirming(false);
    }
  };
  const repatch = (): void => {
    if (input.on_program) {
      setConfirming(true);
      return;
    }
    void send('protect');
  };
  return (
    <li
      className={`flex flex-wrap items-center gap-2 rounded border p-2 ${input.on_program ? 'border-red-600' : input.on_preview ? 'border-green-600' : ''}`}
      aria-label={t`Input ${input.number}`}
    >
      <span className="font-semibold"><Trans>Input {input.number}</Trans></span>
      {input.label !== null && <span>· {input.label}</span>}
      <span className="text-muted-foreground">· {sourceLabel}</span>
      {tally !== undefined && <span role="status" aria-label={tally === 'PGM' ? t`On Program` : t`On Preview`} className={`rounded px-1 text-xs text-white ${tally === 'PGM' ? 'bg-red-600' : 'bg-green-600'}`}>{tally}</span>}
      <Button size="sm" disabled={disabled || !patched || input.on_preview || input.on_program} onClick={() => { onSelect('select_preview', input.number); }}><Trans>PVW</Trans></Button>
      <Button size="sm" disabled={disabled || !patched || input.on_program} onClick={() => { onSelect('select_program', input.number); }}><Trans>PGM</Trans></Button>
      <label className="sr-only" htmlFor={`repatch-${String(input.number)}`}><Trans>Patch source for input {input.number}</Trans></label>
      <select id={`repatch-${String(input.number)}`} className="rounded border bg-background p-1" value={choice} disabled={disabled || busy} onChange={(event) => { setChoice(event.target.value); }}>
        <option value="">{t`Re-patch…`}</option>
        {sources.map((source) => <option key={source.id} value={source.id}>{source.name}</option>)}
      </select>
      <Button size="sm" disabled={disabled || busy || choice === '' || choice === inputSource(input)} onClick={repatch}><Trans>Patch</Trans></Button>
      {confirming && (
        <div role="alertdialog" aria-label={t`Confirm on-air re-patch`} className="flex items-center gap-2 rounded border border-red-600 p-2">
          <span><Trans>Input {input.number} is on Program: re-patching cuts Program to the new source.</Trans></span>
          <Button size="sm" variant="destructive" disabled={busy} onClick={() => { void send('allow'); }}><Trans>Cut Program</Trans></Button>
          <Button size="sm" variant="outline" disabled={busy} onClick={() => { setConfirming(false); }}><Trans>Cancel</Trans></Button>
        </div>
      )}
    </li>
  );
}

/** The live numbered input bank of one M/E. */
export function MixerInputBank({ mixer, sources, disabled, onSelect, onRepatch }: MixerInputBankProps): JSX.Element {
  const { t } = useLingui();
  return (
    <section className="space-y-2" aria-label={t`Mixer inputs`}>
      <h2 className="font-semibold"><Trans>Mixer inputs</Trans></h2>
      <ul className="space-y-1">
        {mixer.inputs.map((input) => (
          <InputRow key={input.number} me={mixer.me} input={input} sources={sources} disabled={disabled} onSelect={onSelect} onRepatch={onRepatch} />
        ))}
      </ul>
    </section>
  );
}
