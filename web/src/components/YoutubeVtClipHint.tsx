/* eslint-disable react-refresh/only-export-components -- the pure reason-matcher lives beside the component that renders its result */
// Shared "this YouTube source is not live" hint (#419 / #423).
//
// The backend can only ingest LIVE YouTube; a recorded video is reported by
// the engine as a source/tile failure reason starting with "not a live
// stream" (status was_live|not_live|post_live). There is NO server endpoint
// to import a URL into the media library as a VT clip (tracked as #423), so
// this never calls an invented endpoint — it names the gap and disables the
// action with an honest accessible description.
import type { JSX } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';

import { Button } from './ui/button';

/** Whether a demand/tile `detail` string names the recorded-video failure (#423). */
export function isNotLiveStreamReason(detail: string | null | undefined): boolean {
  return typeof detail === 'string' && detail.startsWith('not a live stream');
}

/**
 * The short redirect note + disabled "Import as VT clip" action, shown both in
 * the `youtube` source form (always) and on the Sources list when a youtube
 * source's reported reason starts with "not a live stream" (#419 item 4).
 */
export function YoutubeVtClipHint(): JSX.Element {
  const { t } = useLingui();
  return (
    <p className="flex flex-wrap items-center gap-2 text-xs text-muted-foreground">
      <span>
        <Trans>
          YouTube sources must be live. For a recorded video, import it as a
          VT clip instead.
        </Trans>
      </span>
      <Button
        type="button"
        variant="outline"
        size="sm"
        disabled
        aria-describedby="youtube-vt-clip-unavailable"
      >
        <Trans>Import as VT clip</Trans>
      </Button>
      <span id="youtube-vt-clip-unavailable" className="sr-only">
        {t`Not available on this server yet — tracked as issue #423. There is no server endpoint to import a URL into the media library.`}
      </span>
    </p>
  );
}
