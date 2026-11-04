import { useState } from 'react';
import type { JSX } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import { Button } from '../components/ui/button';
import { Dialog, DialogContent, DialogDescription, DialogHeader, DialogTitle } from '../components/ui/dialog';
import { PreviewSurface } from './PreviewSurface';
import { usePreviewCapabilities } from './capabilities';

/** No video session or JPEG request exists until the operator opens the dialog. */
export function SourcePreviewButton({ id, name }: { readonly id: string; readonly name: string }): JSX.Element {
  const { t } = useLingui();
  const [open, setOpen] = useState(false);
  const capabilities = usePreviewCapabilities();
  return <>
    <Button variant="outline" size="sm" onClick={() => { setOpen(true); }} aria-label={t`Preview: ${name}`}><Trans>Preview</Trans></Button>
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogContent className="max-w-3xl">
        <DialogHeader>
          <DialogTitle><Trans>Source preview</Trans> — {name}</DialogTitle>
          <DialogDescription><Trans>The engine must ingest this source before a preview is available. WebRTC is used only when the server supports it.</Trans></DialogDescription>
        </DialogHeader>
        {open ? <PreviewSurface key={id} scope="input" whepEndpoint={`/api/v1/preview/inputs/${encodeURIComponent(id)}/whep`} jpegPath={`/api/v1/preview/inputs/${encodeURIComponent(id)}.jpg`} label={name} capabilities={capabilities.data} className="aspect-video w-full bg-black object-contain" /> : null}
      </DialogContent>
    </Dialog>
  </>;
}
