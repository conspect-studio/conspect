// Monitoring — the live preview wall.
//
// Shows the composited PROGRAM and each INPUT as low-rate JPEG stills the control
// plane serves (GET /api/v1/preview/program.jpg, /preview/inputs,
// /preview/inputs/{id}.jpg), refreshed ~1/s. Because an <img> tag cannot send an
// Authorization header, each still is FETCHED with the stored bearer token and
// shown via an object URL (revoked on refresh). Alongside the pixels, the real
// per-tile lifecycle state streams over the WebSocket (`useEngineEvents`). All
// reads are best-effort and never block the engine (invariant #10).
import { useEffect, useRef, useState } from 'react';
import type { JSX } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import { useQuery } from '@tanstack/react-query';

import { getStoredToken } from '../api/token';
import { ConnectionStatus } from '../components/ConnectionStatus';
import { HelpLink } from '../components/HelpLink';
import { PageHeader } from '../components/PageHeader';
import { TileStateBadge } from '../components/TileStateBadge';
import { Card, CardContent } from '../components/ui/card';
import { useLiveTiles } from '../resources/useLiveTiles';
import { useEngineEvents } from '../realtime/useEngineEvents';
import type { LiveTile } from '../realtime/useEngineEvents';

/** Fetch a preview JPEG with the bearer token and expose it as an object URL,
 *  refreshed every `refreshMs`. Returns `undefined` until a frame arrives (the
 *  endpoint answers 503 when the engine has produced none yet). */
function usePreviewUrl(path: string, refreshMs: number): string | undefined {
  const [url, setUrl] = useState<string | undefined>(undefined);
  const current = useRef<string | undefined>(undefined);

  useEffect(() => {
    let cancelled = false;

    const revoke = (): void => {
      if (current.current !== undefined) {
        URL.revokeObjectURL(current.current);
        current.current = undefined;
      }
    };

    const tick = async (): Promise<void> => {
      const headers: Record<string, string> = {};
      const token = getStoredToken();
      if (token !== undefined) {
        headers.Authorization = `Bearer ${token}`;
      }
      try {
        const resp = await fetch(path, { headers, cache: 'no-store' });
        if (!resp.ok) {
          return;
        }
        const blob = await resp.blob();
        if (cancelled) {
          return;
        }
        const next = URL.createObjectURL(blob);
        revoke();
        current.current = next;
        setUrl(next);
      } catch {
        // Best-effort preview; a failed fetch just keeps the last frame.
      }
    };

    void tick();
    const handle = window.setInterval(() => void tick(), refreshMs);
    return (): void => {
      cancelled = true;
      window.clearInterval(handle);
      revoke();
    };
  }, [path, refreshMs]);

  return url;
}

/** The ids of inputs that can be previewed (GET /api/v1/preview/inputs). */
function usePreviewInputIds(): readonly string[] {
  const query = useQuery<readonly string[]>({
    queryKey: ['preview', 'inputs'],
    queryFn: async (): Promise<readonly string[]> => {
      const headers: Record<string, string> = {};
      const token = getStoredToken();
      if (token !== undefined) {
        headers.Authorization = `Bearer ${token}`;
      }
      const resp = await fetch('/api/v1/preview/inputs', { headers });
      if (!resp.ok) {
        return [];
      }
      const data: unknown = await resp.json();
      return Array.isArray(data) ? data.filter((v): v is string => typeof v === 'string') : [];
    },
    refetchInterval: 5000,
  });
  return query.data ?? [];
}

/** A single live preview image with a placeholder until the first frame. */
function PreviewImage(props: {
  readonly src: string | undefined;
  readonly alt: string;
  readonly className?: string;
}): JSX.Element {
  if (props.src === undefined) {
    return (
      <div
        className={`flex items-center justify-center bg-muted text-xs text-muted-foreground ${props.className ?? ''}`}
        role="img"
        aria-label={props.alt}
      >
        <Trans>Waiting for a frame…</Trans>
      </div>
    );
  }
  return <img src={props.src} alt={props.alt} className={props.className} />;
}

/** The monitoring page. */
export function MonitoringPage(): JSX.Element {
  const { t } = useLingui();
  const { status } = useEngineEvents();
  const program = usePreviewUrl('/api/v1/preview/program.jpg', 1000);
  const inputIds = usePreviewInputIds();
  const tiles = useLiveTiles();

  return (
    <>
      <PageHeader
        title={<Trans>Monitoring</Trans>}
        description={
          <Trans>
            Live program output and per-input previews, with each tile&apos;s
            lifecycle state from the engine stream.
          </Trans>
        }
        actions={<ConnectionStatus status={status} />}
      />

      <section aria-labelledby="program-heading">
        <h2 id="program-heading" className="mb-3 text-lg font-semibold">
          <Trans>Program</Trans>
        </h2>
        <Card className="overflow-hidden">
          <PreviewImage
            src={program}
            alt={t`Live program output`}
            className="aspect-video w-full bg-black object-contain"
          />
        </Card>
      </section>

      <section aria-labelledby="inputs-heading" className="mt-8">
        <h2 id="inputs-heading" className="mb-3 flex items-center gap-2 text-lg font-semibold">
          <Trans>Inputs</Trans>
          <HelpLink
            to="/help/concepts/resilience#tile-lifecycle"
            label={t`About tile lifecycle states`}
            compact
          />
        </h2>
        {inputIds.length === 0 ? (
          <p className="text-sm text-muted-foreground">
            <Trans>No inputs are available to preview.</Trans>
          </p>
        ) : (
          <ul className="grid gap-3 sm:grid-cols-2 lg:grid-cols-3 xl:grid-cols-4">
            {inputIds.map((id) => (
              <InputThumbnail key={id} id={id} tile={tiles.get(id)} />
            ))}
          </ul>
        )}
      </section>
    </>
  );
}

/** One input's live thumbnail + lifecycle badge. */
function InputThumbnail(props: {
  readonly id: string;
  readonly tile: LiveTile | undefined;
}): JSX.Element {
  const { t } = useLingui();
  const src = usePreviewUrl(`/api/v1/preview/inputs/${encodeURIComponent(props.id)}.jpg`, 1000);
  return (
    <li>
      <Card className="overflow-hidden">
        <PreviewImage
          src={src}
          alt={t`Preview of input ${props.id}`}
          className="aspect-video w-full bg-black object-contain"
        />
        <CardContent className="flex items-center justify-between gap-2 py-2">
          <code className="truncate text-xs" lang="" dir="auto">
            {props.id}
          </code>
          {props.tile !== undefined ? <TileStateBadge state={props.tile.state} /> : null}
        </CardContent>
      </Card>
    </li>
  );
}
