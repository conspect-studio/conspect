// Operator-visible operation status. A command's `202` is only an ACCEPTANCE; the
// authoritative outcome requires a command-specific terminal expectation.
// `followOperation` toasts acceptance only: a bare corr cannot establish success.
import { t } from '@lingui/core/macro';

import { toast } from '../components/ui/use-toast';

/** The slice of an accepted command this module needs. */
export interface FollowableOperation {
  readonly operation_id: string;
  /** Accepted command kind, including replay (whose original verb is unknown). */
  readonly kind?: string;
}

/** Toast acceptance only; the tracker owns outcome feedback. */
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

}
