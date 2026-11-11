// Manual Start / Stop for one source. Start holds the source ready until Stop.
// Stop is refused by the server while anything else uses the source; the
// refusal names every user (an "Always on" setting counts), shown inline.
import { useState } from 'react';
import type { JSX } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import { Play, Square } from 'lucide-react';

import { useSourceAction } from '../api/sourceStatus';
import type { SourceDemand } from '../api/sourceStatus';
import { Button } from './ui/button';

/** Start / Stop buttons for source `id` given its lifecycle `demand`. */
export function SourceLifecycleActions({
  id,
  name,
  demand,
}: {
  readonly id: string;
  readonly name: string;
  readonly demand: SourceDemand | undefined;
}): JSX.Element {
  const { t } = useLingui();
  const action = useSourceAction();
  const [refusal, setRefusal] = useState<string | null>(null);
  const phase = demand?.phase ?? 'idle';
  const manual = demand?.consumers.some((c) => c.kind === 'manual') ?? false;
  const run = (which: 'start' | 'stop'): void => {
    setRefusal(null);
    action.mutate(
      { id, action: which },
      {
        onError: (error) => {
          setRefusal(error.detail ?? error.message);
        },
      },
    );
  };
  return (
    <span className="inline-flex flex-col gap-1">
      <span className="inline-flex gap-1">
        {phase === 'idle' || (phase === 'failed' && !manual) ? (
          <Button
            size="sm"
            variant="outline"
            disabled={action.isPending}
            aria-label={t`Start ${name}`}
            onClick={(): void => {
              run('start');
            }}
          >
            <Play className="size-3.5" aria-hidden="true" />
            <Trans>Start</Trans>
          </Button>
        ) : null}
        {phase !== 'idle' ? (
          <Button
            size="sm"
            variant="outline"
            disabled={action.isPending}
            aria-label={t`Stop ${name}`}
            onClick={(): void => {
              run('stop');
            }}
          >
            <Square className="size-3.5" aria-hidden="true" />
            <Trans>Stop</Trans>
          </Button>
        ) : null}
      </span>
      {refusal !== null ? (
        <span role="alert" className="text-xs text-destructive" data-testid="source-stop-refused">
          {refusal}
        </span>
      ) : null}
    </span>
  );
}
