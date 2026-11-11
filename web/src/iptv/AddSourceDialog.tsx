// The single "Add as source" step from the IPTV-ORG browser: say how the source
// starts, then continue to the Sources page's create dialog, which opens
// prefilled with that choice (nothing is saved until the operator confirms
// there). The choice is explicit — nothing is preselected — and Continue stays
// disabled until one is picked (`docs/io/iptv-org.md`, Integration rules,
// item 3). Same control as the bulk dialog and the M3U import dialog.
import { useState } from 'react';
import type { JSX } from 'react';
import { Trans } from '@lingui/react/macro';

import type { IndexStream } from '../api/catalogue';
import { Button } from '../components/ui/button';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '../components/ui/dialog';
import type { SourceActivationChoice } from '../resources/forms';
import { ActivationChoice } from './ActivationChoice';

/** The single-add activation step. */
export function AddSourceDialog({
  open,
  stream,
  onClose,
  onConfirm,
}: {
  readonly open: boolean;
  readonly stream: IndexStream | null;
  readonly onClose: () => void;
  readonly onConfirm: (activation: SourceActivationChoice) => void;
}): JSX.Element {
  const [activation, setActivation] = useState<SourceActivationChoice | null>(null);

  const close = (): void => {
    setActivation(null);
    onClose();
  };

  return (
    <Dialog
      open={open}
      onOpenChange={(next): void => {
        if (!next) {
          close();
        }
      }}
    >
      <DialogContent>
        <DialogHeader>
          <DialogTitle>
            <Trans>Add as source</Trans>
          </DialogTitle>
          <DialogDescription>
            <Trans>
              The stream becomes an ordinary source linked to its IPTV-ORG entry. The Sources page
              opens the create dialog next with this choice already set; nothing is saved until you
              confirm there.
            </Trans>
          </DialogDescription>
        </DialogHeader>
        {stream !== null ? (
          <p className="text-sm" lang="" dir="auto">
            <strong>{stream.title}</strong>
            {stream.q !== null ? ` · ${stream.q}` : ''}
          </p>
        ) : null}
        <ActivationChoice
          value={activation}
          onChange={setActivation}
          count={1}
          name="catalogue-single-activation"
        />
        <DialogFooter>
          <Button type="button" variant="outline" onClick={close}>
            <Trans>Cancel</Trans>
          </Button>
          <Button
            type="button"
            disabled={activation === null || stream === null}
            onClick={(): void => {
              if (activation === null) {
                return;
              }
              const chosen = activation;
              // Reset before navigating: coming back must ask again.
              setActivation(null);
              onConfirm(chosen);
            }}
          >
            <Trans>Continue</Trans>
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
