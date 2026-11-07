// Operator-visible operation status. A command's `202` is only an ACCEPTANCE; the
// authoritative outcome arrives on the realtime stream (`corr` == operation id).
// `followOperation` toasts acceptance immediately, then the outcome: success, a
// failure with its reason, or -- reported separately -- that no outcome arrived
// before the timeout (the command may still have run).
import { t } from '@lingui/core/macro';

import { toast } from '../components/ui/use-toast';
import { REPLAY_KIND } from '../realtime/correlatedCommands';
import type { OperationOutcome } from '../realtime/operations';

/** The slice of an accepted command this module needs. */
export interface FollowableOperation {
  readonly operation_id: string;
  /** Absent for commands the server does not correlate (acceptance-only). */
  readonly completion?: Promise<OperationOutcome>;
  /** The 202 body `kind`; `replay` means the original command's kind is unknown. */
  readonly kind?: string;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null;
}

/** True when the completing envelope type denotes failure. */
function isFailure(type: string): boolean {
  return /(^|\.)(failed|rejected|error)$/.test(type);
}

/** Best-effort human reason from a failure envelope's payload. */
function reasonOf(data: unknown): string | undefined {
  if (!isRecord(data)) {
    return undefined;
  }
  for (const key of ['message', 'reason', 'detail', 'error']) {
    const value = data[key];
    if (typeof value === 'string' && value !== '') {
      return value;
    }
  }
  return undefined;
}

/**
 * Toast the acceptance now and the authoritative outcome when it lands.
 * `label` names the command for the operator (already translated).
 */
export function followOperation(
  accepted: FollowableOperation,
  label: string,
  acceptance?: { readonly title: string; readonly description?: string },
): void {
  toast(
    acceptance ?? {
      title: t`Command accepted`,
      description: `${label} — ${t`Operation id`}: ${accepted.operation_id}`,
    },
  );
  if (accepted.completion === undefined) {
    // Uncorrelated command: the server never echoes a `corr` for it, so there is
    // no outcome to wait for and no timeout to report.
    return;
  }
  // A replayed acceptance may be of an uncorrelated command: report an outcome
  // if one arrives, but never claim a timeout.
  const silentTimeout = accepted.kind === REPLAY_KIND;
  accepted.completion.then(
    (outcome): void => {
      const type = outcome.envelope.t;
      if (isFailure(type)) {
        const reason = reasonOf(outcome.envelope.data) ?? type;
        toast({
          title: t`${label} failed`,
          description: reason,
          variant: 'destructive',
        });
        return;
      }
      toast({ title: t`${label} completed` });
    },
    (): void => {
      if (silentTimeout) {
        return;
      }
      toast({
        title: t`Outcome not received`,
        description: t`${label}: no result arrived for operation ${accepted.operation_id}. It may still be running.`,
        variant: 'destructive',
      });
    },
  );
}
