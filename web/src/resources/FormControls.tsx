// Shared, accessible form controls for the resource management dialogs.
//
// Every field renders a real <label>, and a field error renders inline in a
// dedicated element wired to the control via `aria-describedby` (+
// `aria-invalid`), so screen readers announce the problem at the field — never
// a single far-away alert. Error codes come from the pure validators in
// `./forms`; the localized message lives here (Lingui stays in components).
import { useId, useState } from 'react';
import type { JSX, ReactNode } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import { Check, ChevronRight, Copy, Download, Eye, EyeOff, Info } from 'lucide-react';

import type { FormErrorCode } from './forms';
import {
  ConfigExportUnsupportedError,
  downloadConfigExport,
  fetchConfigExport,
} from './exportConfig';
import { HelpLink } from '../components/HelpLink';
import { Button } from '../components/ui/button';
import { Input } from '../components/ui/input';
import { Label } from '../components/ui/label';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '../components/ui/select';
import { toast } from '../components/ui/use-toast';

/** The localized message for a validation code. */
export function FieldErrorMessage({ code }: { readonly code: FormErrorCode }): JSX.Element {
  switch (code) {
    case 'required':
      return <Trans>This field is required.</Trans>;
    case 'url-invalid':
      return (
        <Trans>
          Enter a full URL with a host. Wrap an IPv6 literal in brackets, e.g.
          rtsp://[2001:db8::1]:8554/cam.
        </Trans>
      );
    case 'scheme-rtsp':
      return <Trans>An RTSP source URL must start with rtsp:// (or rtsps://).</Trans>;
    case 'scheme-srt':
      return <Trans>An SRT URL must start with srt://.</Trans>;
    case 'scheme-rist':
      return <Trans>A RIST URL must start with rist://.</Trans>;
    case 'scheme-rtmp':
      return <Trans>An RTMP URL must start with rtmp:// (or rtmps://).</Trans>;
    case 'scheme-http':
      return <Trans>This URL must start with http:// or https://.</Trans>;
    case 'hex-color':
      return <Trans>Enter a hex colour like #101014 or #abc.</Trans>;
    case 'int':
      return <Trans>Enter a whole number.</Trans>;
    case 'int-range':
      return <Trans>Enter a whole number within the allowed range.</Trans>;
    case 'positive-int':
      return <Trans>Enter a whole number greater than zero.</Trans>;
    case 'number':
      return <Trans>Enter a number (decimals allowed, e.g. -23.5).</Trans>;
    case 'zone-extent':
      return (
        <Trans>
          The zone must fit inside the frame: x, y at least 0, width and
          height above 0, and x+width / y+height at most 1.
        </Trans>
      );
    case 'mount-slash':
      return <Trans>A mount point must start with /, e.g. /multiview.</Trans>;
    case 'tracks-required':
      return <Trans>List at least one track name (comma-separated).</Trans>;
    case 'finite-number':
      return <Trans>Enter a number (decibels may be negative, e.g. -3).</Trans>;
    case 'duplicate-track':
      return <Trans>Another route already claims this track name.</Trans>;
    case 'duplicate-input':
      return <Trans>Another route already uses this input.</Trans>;
    case 'reserved-track':
      return (
        <Trans>
          “prog” is reserved for the program mix — choose another track name.
        </Trans>
      );
    case 'program-bus-muted':
      return (
        <Trans>
          Every input on the program mix is muted. Unmute at least one, or take
          them all off the mix.
        </Trans>
      );
    case 'rational-fps':
      return (
        <Trans>
          Enter an exact rational rate like 60000/1001, or a whole number like
          50 — never a decimal.
        </Trans>
      );
    case 'timezone':
      return (
        <Trans>
          Enter a valid IANA timezone id, e.g. Australia/Sydney or UTC. Leave
          blank to use a fixed UTC offset instead.
        </Trans>
      );
    case 'time-of-day':
      return <Trans>Enter a 24-hour time of day as HH:MM:SS, e.g. 14:30:00.</Trans>;
    case 'date-time':
      return (
        <Trans>
          Enter a local date and time as YYYY-MM-DDTHH:MM:SS, e.g.
          2026-07-01T09:00:00.
        </Trans>
      );
    case 'members-required':
      return <Trans>Add at least one member device to the group.</Trans>;
    case 'duplicate-member':
      return <Trans>This device is already a member of the group.</Trans>;
  }
}

/** A labelled text/number input with an inline, SR-wired error. */
export function FormField({
  id,
  label,
  value,
  onChange,
  error,
  disabled,
  required,
  placeholder,
  type,
  hint,
  trailing,
  labelHidden,
  datalist,
}: {
  readonly id: string;
  readonly label: string;
  readonly value: string;
  readonly onChange: (next: string) => void;
  /** The active validation code for this field, if any. */
  readonly error?: FormErrorCode | undefined;
  readonly disabled?: boolean;
  readonly required?: boolean;
  readonly placeholder?: string;
  readonly type?: string;
  /** Optional supporting hint rendered under the control. */
  readonly hint?: ReactNode;
  /** Optional trailing affordance next to the label (e.g. a HelpLink). */
  readonly trailing?: ReactNode;
  /**
   * Visually hide the label (it stays the accessible name) — for dense table
   * cells where the column header already shows the text.
   */
  readonly labelHidden?: boolean;
  /**
   * Optional suggestion list rendered as a native `<datalist>` — keeps the
   * field free-text (the value is server-validated) while offering a native,
   * keyboard-accessible autocomplete (e.g. IANA timezone ids). Omitted ⇒ no
   * datalist, identical to before.
   */
  readonly datalist?: readonly string[];
}): JSX.Element {
  const errorId = `${id}-error`;
  const hintId = `${id}-hint`;
  const listId = `${id}-list`;
  const hasList = datalist !== undefined && datalist.length > 0;
  const describedBy =
    [error !== undefined ? errorId : undefined, hint !== undefined ? hintId : undefined]
      .filter((part) => part !== undefined)
      .join(' ') || undefined;
  return (
    <div className="flex flex-col gap-1">
      <div
        className={
          labelHidden === true ? 'sr-only' : 'flex items-center gap-1.5'
        }
      >
        <Label htmlFor={id}>{label}</Label>
        {trailing}
      </div>
      <Input
        id={id}
        type={type ?? 'text'}
        value={value}
        required={required ?? false}
        disabled={disabled ?? false}
        aria-invalid={error !== undefined}
        {...(describedBy !== undefined ? { 'aria-describedby': describedBy } : {})}
        {...(placeholder !== undefined ? { placeholder } : {})}
        {...(hasList ? { list: listId } : {})}
        onChange={(event): void => {
          onChange(event.target.value);
        }}
      />
      {hasList ? (
        <datalist id={listId}>
          {datalist.map((option) => (
            <option key={option} value={option} />
          ))}
        </datalist>
      ) : null}
      {hint !== undefined ? (
        <p id={hintId} className="text-xs text-muted-foreground">
          {hint}
        </p>
      ) : null}
      {error !== undefined ? (
        <p id={errorId} className="text-sm text-destructive">
          <FieldErrorMessage code={error} />
        </p>
      ) : null}
    </div>
  );
}

/**
 * A labelled secret input: masked by default (`type="password"`), with a
 * keyboard-accessible reveal toggle. The value is never auto-shown — the
 * operator opts in per field. Secrets follow the config-secret posture: editable
 * here, but rendered masked so an over-the-shoulder read does not leak them.
 */
export function SecretField({
  id,
  label,
  value,
  onChange,
  error,
  required,
  placeholder,
  hint,
  trailing,
}: {
  readonly id: string;
  readonly label: string;
  readonly value: string;
  readonly onChange: (next: string) => void;
  readonly error?: FormErrorCode | undefined;
  readonly required?: boolean;
  readonly placeholder?: string;
  readonly hint?: ReactNode;
  readonly trailing?: ReactNode;
}): JSX.Element {
  const { t } = useLingui();
  const [revealed, setRevealed] = useState(false);
  const errorId = `${id}-error`;
  const hintId = `${id}-hint`;
  const describedBy =
    [error !== undefined ? errorId : undefined, hint !== undefined ? hintId : undefined]
      .filter((part) => part !== undefined)
      .join(' ') || undefined;
  return (
    <div className="flex flex-col gap-1">
      <div className="flex items-center gap-1.5">
        <Label htmlFor={id}>{label}</Label>
        {trailing}
      </div>
      <div className="flex items-center gap-2">
        <Input
          id={id}
          type={revealed ? 'text' : 'password'}
          value={value}
          required={required ?? false}
          autoComplete="off"
          aria-invalid={error !== undefined}
          {...(describedBy !== undefined ? { 'aria-describedby': describedBy } : {})}
          {...(placeholder !== undefined ? { placeholder } : {})}
          onChange={(event): void => {
            onChange(event.target.value);
          }}
        />
        <Button
          type="button"
          variant="outline"
          size="icon"
          className="shrink-0"
          aria-pressed={revealed}
          aria-label={revealed ? t`Hide ${label}` : t`Reveal ${label}`}
          onClick={(): void => {
            setRevealed((prev) => !prev);
          }}
        >
          {revealed ? (
            <EyeOff className="size-4" aria-hidden="true" />
          ) : (
            <Eye className="size-4" aria-hidden="true" />
          )}
        </Button>
      </div>
      {hint !== undefined ? (
        <p id={hintId} className="text-xs text-muted-foreground">
          {hint}
        </p>
      ) : null}
      {error !== undefined ? (
        <p id={errorId} className="text-sm text-destructive">
          <FieldErrorMessage code={error} />
        </p>
      ) : null}
    </div>
  );
}

/**
 * A labelled, read-only value with a copy-to-clipboard button — for a DERIVED
 * locator the operator hands to a publisher/viewer (a WHIP/WHEP endpoint URL,
 * ADR-W023). Not an editable field: the value is computed, not authored.
 */
export function DerivedUrlField({
  id,
  label,
  value,
  hint,
  trailing,
}: {
  readonly id: string;
  readonly label: string;
  readonly value: string;
  readonly hint?: ReactNode;
  readonly trailing?: ReactNode;
}): JSX.Element {
  const { t } = useLingui();
  const [copied, setCopied] = useState(false);
  const hintId = `${id}-hint`;
  const copy = (): void => {
    // The Clipboard API is unavailable in an insecure context (the lib.dom type
    // claims it is always present, but at runtime `navigator.clipboard` is
    // undefined off HTTPS/localhost); probe the property before using it.
    if (!('clipboard' in navigator)) {
      toast({ title: t`Copy unavailable`, description: t`Select and copy the value manually.` });
      return;
    }
    navigator.clipboard
      .writeText(value)
      .then((): void => {
        setCopied(true);
        window.setTimeout((): void => {
          setCopied(false);
        }, 2000);
        toast({ title: t`Copied`, description: t`${label} copied to the clipboard.` });
      })
      .catch((): void => {
        toast({
          title: t`Copy failed`,
          description: t`Select and copy the value manually.`,
          variant: 'destructive',
        });
      });
  };
  return (
    <div className="flex flex-col gap-1">
      <div className="flex items-center gap-1.5">
        <Label htmlFor={id}>{label}</Label>
        {trailing}
      </div>
      <div className="flex items-center gap-2">
        <Input
          id={id}
          type="text"
          value={value}
          readOnly
          className="font-mono text-xs"
          {...(hint !== undefined ? { 'aria-describedby': hintId } : {})}
          onFocus={(event): void => {
            event.target.select();
          }}
        />
        <Button
          type="button"
          variant="outline"
          size="icon"
          className="shrink-0"
          aria-label={t`Copy ${label}`}
          onClick={copy}
        >
          {copied ? (
            <Check className="size-4" aria-hidden="true" />
          ) : (
            <Copy className="size-4" aria-hidden="true" />
          )}
        </Button>
      </div>
      {hint !== undefined ? (
        <p id={hintId} className="text-xs text-muted-foreground">
          {hint}
        </p>
      ) : null}
    </div>
  );
}

/** A labelled <Select> over fixed string options, with optional display text. */
export function SelectField<Option extends string>({
  label,
  value,
  options,
  onChange,
  optionLabel,
  trailing,
  testId,
  labelHidden,
  placeholder,
  error,
}: {
  readonly label: string;
  readonly value: Option;
  readonly options: readonly Option[];
  readonly onChange: (next: Option) => void;
  /** Optional display text per option (defaults to the option itself). */
  readonly optionLabel?: (option: Option) => ReactNode;
  /** Optional trailing affordance next to the label (e.g. a HelpLink). */
  readonly trailing?: ReactNode;
  /**
   * Optional stable test id on the trigger. e2e tests against the production
   * bundle cannot match freshly-added localized labels (the compiled catalog
   * lags until the i18n lane runs `lingui extract`), so they hook this instead.
   */
  readonly testId?: string;
  /** Visually hide the label (it stays the accessible name). */
  readonly labelHidden?: boolean;
  /** Placeholder shown while no option is selected (empty `value`). */
  readonly placeholder?: string;
  /** The active validation code for this field, if any. */
  readonly error?: FormErrorCode | undefined;
}): JSX.Element {
  const labelId = useId();
  const errorId = useId();
  return (
    <div className="flex flex-col gap-1">
      <div
        className={
          labelHidden === true ? 'sr-only' : 'flex items-center gap-1.5'
        }
      >
        <Label id={labelId}>{label}</Label>
        {trailing}
      </div>
      <Select
        value={value}
        onValueChange={(next): void => {
          const picked = options.find((option) => option === next);
          if (picked !== undefined) {
            onChange(picked);
          }
        }}
      >
        <SelectTrigger
          aria-labelledby={labelId}
          aria-invalid={error !== undefined}
          {...(testId !== undefined ? { 'data-testid': testId } : {})}
          {...(error !== undefined ? { 'aria-describedby': errorId } : {})}
        >
          <SelectValue {...(placeholder !== undefined ? { placeholder } : {})} />
        </SelectTrigger>
        <SelectContent>
          {options.map((option) => (
            <SelectItem key={option} value={option}>
              {optionLabel !== undefined ? optionLabel(option) : option}
            </SelectItem>
          ))}
        </SelectContent>
      </Select>
      {error !== undefined ? (
        <p id={errorId} className="text-sm text-destructive">
          <FieldErrorMessage code={error} />
        </p>
      ) : null}
    </div>
  );
}

/** A labelled checkbox row. */
export function CheckboxField({
  id,
  label,
  checked,
  onChange,
  labelHidden,
}: {
  readonly id: string;
  readonly label: string;
  readonly checked: boolean;
  readonly onChange: (next: boolean) => void;
  /** Visually hide the label (it stays the accessible name). */
  readonly labelHidden?: boolean;
}): JSX.Element {
  return (
    <div className="flex items-center gap-2">
      <Input
        id={id}
        type="checkbox"
        className="h-4 w-4"
        checked={checked}
        onChange={(event): void => {
          onChange(event.target.checked);
        }}
      />
      <Label htmlFor={id} className={labelHidden === true ? 'sr-only' : undefined}>
        {label}
      </Label>
    </div>
  );
}

/**
 * A keyboard-native collapsible "Advanced" disclosure (a real
 * <details>/<summary>, so it needs no extra ARIA wiring).
 */
export function AdvancedSection({
  summary,
  children,
}: {
  readonly summary: string;
  readonly children: ReactNode;
}): JSX.Element {
  return (
    <details className="group rounded-md border p-3" data-testid="advanced-section">
      <summary
        className="flex cursor-pointer list-none items-center gap-1 text-sm font-medium [&::-webkit-details-marker]:hidden"
        data-testid="advanced-toggle"
      >
        <ChevronRight
          className="size-4 transition-transform group-open:rotate-90"
          aria-hidden="true"
        />
        {summary}
      </summary>
      <div className="mt-3 flex flex-col gap-4">{children}</div>
    </details>
  );
}

/**
 * The honest apply-semantics callout (ADR-W015 §4, ADR-W018): each page states
 * exactly how its saved edits reach the running engine. The default copy is
 * the stored-resource text (true for outputs); the sources page overrides it
 * with the per-kind live/restart truth. Text + icon, never colour alone.
 */
export function ApplySemanticsCallout({
  helpTo,
  helpLabel,
  message,
}: {
  readonly helpTo: string;
  readonly helpLabel: string;
  /** Page-specific apply-semantics copy (defaults to the stored-resource text). */
  readonly message?: ReactNode;
}): JSX.Element {
  return (
    <div
      role="note"
      className="mb-4 flex items-start gap-2 rounded-md border bg-muted/40 p-3 text-sm"
    >
      <Info className="mt-0.5 size-4 shrink-0 text-muted-foreground" aria-hidden="true" />
      <p>
        {message ?? (
          <Trans>
            Changes saved here are stored, not yet live: the engine does not
            hot-add outputs. Apply them by exporting the configuration and
            restarting Multiview. Layout apply, source swap, routing, and
            salvos act on the running engine immediately.
          </Trans>
        )}{' '}
        <HelpLink to={helpTo} label={helpLabel} />
      </p>
    </div>
  );
}

/**
 * "Export configuration" — downloads `GET /api/v1/config/export` as
 * multiview.toml. Degrades to an explanatory toast when the backend does not
 * serve the route yet (404/501).
 */
export function ExportConfigButton({
  compact = false,
}: {
  /** Outline/secondary styling for page headers. */
  readonly compact?: boolean;
}): JSX.Element {
  const { t } = useLingui();
  const exportNow = (): void => {
    fetchConfigExport()
      .then((result): void => {
        downloadConfigExport(result);
        toast({
          title: t`Configuration exported`,
          description: t`Saved as ${result.filename}. Restart Multiview with this file to apply stored changes.`,
        });
      })
      .catch((error: unknown): void => {
        if (error instanceof ConfigExportUnsupportedError) {
          toast({
            title: t`Export not available`,
            description: t`This control plane does not serve config export yet. Update the server to use it.`,
            variant: 'destructive',
          });
          return;
        }
        toast({
          title: t`Could not export configuration`,
          description: error instanceof Error ? error.message : String(error),
          variant: 'destructive',
        });
      });
  };
  return (
    <Button variant={compact ? 'outline' : 'default'} onClick={exportNow}>
      <Download aria-hidden="true" />
      <Trans>Export configuration</Trans>
    </Button>
  );
}
