import { useState } from 'react';
import type { JSX } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import { Button } from '../components/ui/button';
import { Input } from '../components/ui/input';
import { Label } from '../components/ui/label';
import { OperationApiError } from '../api/operations';
import { useAutomation, useAutomationCommand } from './sourceEvents';
import type { AutomationRule, AutomationView } from './sourceEvents';

function hhmmss(unixMs: number): string {
  const d = new Date(unixMs);
  const two = (n: number): string => String(n).padStart(2, '0');
  return `${two(d.getHours())}:${two(d.getMinutes())}:${two(d.getSeconds())}`;
}

function StatusLine({ view }: { readonly view: AutomationView }): JSX.Element {
  const s = view.status;
  if (!view.executor_running) {
    return <p role="status"><Trans>Automation executor is not running on this server (source events are unavailable).</Trans></p>;
  }
  if (s.phase === 'in_break' || s.phase === 'restoring') {
    const swaps = s.replaced.map((c) => `${c.cell}: ${c.original} → ${c.replacement}`).join(', ');
    const since = s.since_unix_ms == null ? '' : hhmmss(s.since_unix_ms);
    const audio = s.audio_replaced ?? '';
    return <p role="status" className="font-medium">
      {s.phase === 'restoring' ? <Trans>Restoring…</Trans> : <Trans>In break: replaced {swaps} since {since}</Trans>}
      {audio !== '' ? <> <Trans>(program audio → {audio})</Trans></> : null}
      {s.test ? <span className="ml-2 rounded bg-amber-200 px-1.5 py-0.5 text-xs font-semibold uppercase text-amber-900"><Trans>Test</Trans></span> : null}
    </p>;
  }
  if (s.phase === 'manual_override') {
    return <p role="status" className="font-medium"><Trans>Manual override: an operator changed a replaced cell; automation will not restore this break.</Trans></p>;
  }
  if (s.phase === 'idle') return <p role="status"><Trans>Armed — idle, waiting for a break.</Trans></p>;
  return <p role="status"><Trans>Disarmed — no automatic action will be taken.</Trans></p>;
}

const errorText = (error: OperationApiError | null): string | undefined => {
  if (error === null) return undefined;
  return error.detail ?? error.message;
};

function RuleEditor({ id, view, onSave, pending }: {
  readonly id: string;
  readonly view: AutomationView;
  readonly onSave: (rule: AutomationRule | null) => void;
  readonly pending: boolean;
}): JSX.Element {
  const { t } = useLingui();
  const rule = view.rule ?? undefined;
  const [action, setAction] = useState<'swap_cell' | 'route_program'>(rule?.on_break_start.action ?? 'swap_cell');
  const [target, setTarget] = useState(rule?.on_break_start.target ?? 'bars');
  const [cells, setCells] = useState((rule?.on_break_start.cells ?? []).join(', '));
  const [restore, setRestore] = useState(rule?.restore_on_end ?? true);
  const [maxDuration, setMaxDuration] = useState(String(rule?.max_duration_s ?? 600));
  const [webhook, setWebhook] = useState(rule?.webhook_url ?? '');
  const noBars = view.bars_sources.length === 0 && (target === 'bars' || target === 'bars_tone');
  const save = (): void => {
    const list = cells.split(',').map((c) => c.trim()).filter((c) => c !== '');
    onSave({
      armed: rule?.armed ?? false,
      on_break_start: { action, target: target.trim(), cells: list.length > 0 ? list : null },
      restore_on_end: restore,
      max_duration_s: Number.parseInt(maxDuration, 10) || 600,
      webhook_url: webhook.trim() === '' ? null : webhook.trim(),
    });
  };
  return <div className="space-y-2">
    <div className="grid grid-cols-2 gap-2">
      <Label htmlFor={`${id}-auto-action`}><Trans>On break start</Trans></Label>
      <select id={`${id}-auto-action`} className="rounded border bg-transparent px-2 py-1 text-sm" value={action}
        onChange={(e) => { setAction(e.target.value === 'route_program' ? 'route_program' : 'swap_cell'); }}>
        <option value="swap_cell">{t`Swap the cell(s) showing this source`}</option>
        <option value="route_program">{t`Swap cell(s) and program audio`}</option>
      </select>
      <Label htmlFor={`${id}-auto-target`}><Trans>Replacement</Trans></Label>
      <Input id={`${id}-auto-target`} list={`${id}-auto-targets`} value={target} onChange={(e) => { setTarget(e.target.value); }} />
      <datalist id={`${id}-auto-targets`}>
        <option value="bars">{t`Bars`}</option>
        <option value="bars_tone">{t`Bars + tone`}</option>
      </datalist>
      <Label htmlFor={`${id}-auto-cells`}><Trans>Cells (optional, comma-separated)</Trans></Label>
      <Input id={`${id}-auto-cells`} value={cells} placeholder={t`All cells showing this source`} onChange={(e) => { setCells(e.target.value); }} />
      <Label htmlFor={`${id}-auto-max`}><Trans>Watchdog: restore after (seconds)</Trans></Label>
      <Input id={`${id}-auto-max`} type="number" min={1} max={86400} value={maxDuration} onChange={(e) => { setMaxDuration(e.target.value); }} />
      <Label htmlFor={`${id}-auto-webhook`}><Trans>Webhook URL (optional, http://)</Trans></Label>
      <Input id={`${id}-auto-webhook`} value={webhook} onChange={(e) => { setWebhook(e.target.value); }} />
    </div>
    <label className="flex items-center gap-2 text-sm">
      <input type="checkbox" checked={restore} onChange={(e) => { setRestore(e.target.checked); }} />
      <Trans>Restore at break end</Trans>
    </label>
    <p className="text-xs text-muted-foreground">
      <Trans>Replacement is another source id, or "bars" / "bars_tone". Bars uses your first configured Bars source; "bars_tone" also routes its 1 kHz line-up tone onto this source's program audio. Add a Bars source on the Sources page first if you have none.</Trans>
    </p>
    {noBars ? <p className="text-xs text-destructive"><Trans>No Bars source is configured yet — this rule will refuse to act until you add one.</Trans></p> : null}
    <div className="flex gap-2">
      <Button type="button" size="sm" disabled={pending || target.trim() === ''} onClick={save}><Trans>Save rule</Trans></Button>
      {view.rule ? <Button type="button" size="sm" variant="outline" disabled={pending} onClick={() => { onSave(null); }}><Trans>Remove rule</Trans></Button> : null}
    </div>
  </div>;
}

/** ADR-0183 automation: rule editor, arm/disarm with confirmation, live status,
 * "Restore now" and the bounded action log. */
export function AutomationPanel({ id }: { readonly id: string }): JSX.Element {
  const query = useAutomation(id);
  const command = useAutomationCommand(id);
  const [confirming, setConfirming] = useState<'arm' | 'disarm' | undefined>(undefined);
  if (query.isError) {
    const status = query.error instanceof OperationApiError ? query.error.status : undefined;
    return <p role="alert">{status === 403 ? <Trans>You do not have permission to view this source's automation.</Trans>
      : <Trans>Automation status could not be read.</Trans>}</p>;
  }
  if (query.data === undefined) return <p role="status"><Trans>Loading automation…</Trans></p>;
  const view = query.data;
  const armed = view.rule?.armed ?? false;
  const owned = view.status.phase === 'in_break' || view.status.phase === 'restoring';
  return <section aria-label="Automation" className="space-y-3 rounded border p-3">
    <h3 className="font-semibold"><Trans>Ad-break automation</Trans></h3>
    <StatusLine view={view} />
    {view.status.last_error ? <p className="text-xs text-destructive">{view.status.last_error}</p> : null}
    <div className="flex flex-wrap gap-2">
      {view.rule ? (confirming === undefined
        ? <Button type="button" size="sm" variant={armed ? 'destructive' : 'default'} disabled={command.isPending}
          onClick={() => { setConfirming(armed ? 'disarm' : 'arm'); }}>
          {armed ? <Trans>Disarm</Trans> : <Trans>Arm</Trans>}
        </Button>
        : <span className="flex items-center gap-2 rounded border border-amber-400 p-2 text-sm">
          {confirming === 'arm'
            ? <Trans>Arm automation? Confirmed ad breaks on this source will switch program output automatically.</Trans>
            : <Trans>Disarm automation? A replacement on air now will be restored.</Trans>}
          <Button type="button" size="sm" onClick={() => { command.mutate({ kind: confirming }); setConfirming(undefined); }}><Trans>Confirm</Trans></Button>
          <Button type="button" size="sm" variant="outline" onClick={() => { setConfirming(undefined); }}><Trans>Cancel</Trans></Button>
        </span>) : null}
      <Button type="button" size="sm" variant="outline" disabled={command.isPending || !owned}
        onClick={() => { command.mutate({ kind: 'restore' }); }}><Trans>Restore now</Trans></Button>
    </div>
    {command.isError ? <p role="alert" className="text-xs text-destructive">{errorText(command.error)}</p> : null}
    <details>
      <summary className="cursor-pointer text-sm"><Trans>Rule</Trans>{view.rule ? null : <> — <Trans>none configured</Trans></>}</summary>
      <RuleEditor key={JSON.stringify(view.rule ?? null)} id={id} view={view} pending={command.isPending}
        onSave={(rule) => { command.mutate({ kind: 'save', rule }); }} />
    </details>
    <div>
      <h4 className="text-sm font-semibold"><Trans>Action log</Trans></h4>
      {view.log.length === 0
        ? <p className="text-xs text-muted-foreground"><Trans>No automation actions yet.</Trans></p>
        : <ul className="max-h-40 space-y-1 overflow-auto text-xs">
          {view.log.map((entry, index) => <li key={`${String(entry.at_unix_ms)}:${String(index)}`}>
            <span className="font-mono">{hhmmss(entry.at_unix_ms)}</span> {entry.detail}
          </li>)}
        </ul>}
    </div>
  </section>;
}
