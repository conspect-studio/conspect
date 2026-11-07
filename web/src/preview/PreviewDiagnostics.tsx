import { useState } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import { Button } from '../components/ui/button';
import type { PreviewDiagnostics as Diagnostics } from './diagnostics';

export function PreviewDiagnostics({ diagnostics }: { readonly diagnostics: Diagnostics }) {
  const { t } = useLingui();
  const [copy, setCopy] = useState<'idle' | 'ok' | 'failed'>('idle');
  const explanations = {
    'http-capacity': t`WHEP capacity response. The server could not accept this preview.`,
    'http-error': t`WHEP HTTP request failed.`,
    signaling: t`Preview negotiation failed.`,
    'ice-timeout': t`Connection timed out. The network cause is unknown.`,
    'connection-timeout': t`Connection timed out after ICE connected. The cause is unknown.`,
    'ice-failed': t`ICE connection failed. The network cause is unknown.`,
    'peer-failed': t`Peer connection failed. The cause is unknown.`,
    'dtls-failed': t`DTLS transport failed.`,
    'video-no-rtp': t`No inbound video RTP bytes were reported.`,
    'video-no-decode': t`Video RTP arrived, but no decoded frames were reported.`,
    'video-stalled': t`Inbound video stopped progressing.`,
    'media-unknown': t`Video progress could not be determined from browser statistics.`,
    'media-play-error': t`The browser could not play the preview media.`,
  };
  const text = JSON.stringify(diagnostics, null, 2);
  return <section className="space-y-2 px-3 py-2 text-xs" aria-label={t`Preview diagnostics`}>
    <p>{explanations[diagnostics.stage]}</p>
    <p><Trans>Unknown values mean the browser did not provide evidence. Network blocking is not established.</Trans></p>
    <details><summary><Trans>Safe technical details</Trans></summary><pre className="max-h-64 overflow-auto whitespace-pre-wrap">{text}</pre></details>
    <Button type="button" size="sm" variant="secondary" onClick={(): void => {
      void (async () => {
        try { await navigator.clipboard.writeText(text); setCopy('ok'); }
        catch { setCopy('failed'); }
      })();
    }}><Trans>Copy diagnostics</Trans></Button>
    <p role="status">{copy === 'ok' ? <Trans>Diagnostics copied.</Trans> : copy === 'failed' ? <Trans>Could not copy diagnostics. Select the safe technical details and copy them manually.</Trans> : null}</p>
  </section>;
}
