// Dashboard — the "is everything alive?" view. Live tile states come from the
// realtime cache (snapshot ⊕ deltas); layouts come from the typed REST client.
// All reads are best-effort and never block (invariant #10).
import { useMemo } from "react";
import type { JSX } from "react";
import { Trans, useLingui } from "@lingui/react/macro";
import { useQuery } from "@tanstack/react-query";

import { createApiClient } from "../api/client";
import { useLayouts } from "../api/queries";
import { PageHeader } from "../components/PageHeader";
import { TileStateBadge } from "../components/TileStateBadge";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "../components/ui/card";
import { useActiveLocale } from "../i18n/I18nProvider";
import { formatFps } from "../i18n/format";
import { TILES_QUERY_KEY } from "../realtime/useEngineEvents";
import type { LiveTile } from "../realtime/useEngineEvents";

function useLiveTiles(): readonly LiveTile[] {
  const query = useQuery<Record<string, LiveTile>>({
    queryKey: TILES_QUERY_KEY,
    // The realtime hook owns this cache entry; never fetch over HTTP for it.
    queryFn: (): Record<string, LiveTile> => ({}),
    enabled: false,
    initialData: {},
  });
  return useMemo(
    () => Object.values(query.data).sort((a, b) => a.id.localeCompare(b.id)),
    [query.data],
  );
}

/** The monitoring dashboard. */
export function DashboardPage(): JSX.Element {
  const { t } = useLingui();
  const locale = useActiveLocale();
  const client = useMemo(() => createApiClient(), []);
  const layouts = useLayouts(client);
  const tiles = useLiveTiles();

  return (
    <>
      <PageHeader
        title={<Trans>Dashboard</Trans>}
        description={<Trans>Live engine status at a glance.</Trans>}
      />

      <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
        <Card>
          <CardHeader>
            <CardTitle>
              <Trans>Layouts</Trans>
            </CardTitle>
            <CardDescription>
              <Trans>Configured multiview layouts.</Trans>
            </CardDescription>
          </CardHeader>
          <CardContent>
            {layouts.isPending ? (
              <p role="status" aria-live="polite" className="text-sm text-muted-foreground">
                <Trans>Loading…</Trans>
              </p>
            ) : layouts.isError ? (
              <p role="alert" className="text-sm text-destructive">
                {layouts.error.message}
              </p>
            ) : (
              <p className="text-3xl font-semibold tabular-nums">
                {layouts.data.length}
              </p>
            )}
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle>
              <Trans>Tiles</Trans>
            </CardTitle>
            <CardDescription>
              <Trans>Live tiles from the engine stream.</Trans>
            </CardDescription>
          </CardHeader>
          <CardContent>
            <p className="text-3xl font-semibold tabular-nums">{tiles.length}</p>
          </CardContent>
        </Card>
      </div>

      <section aria-labelledby="tiles-heading" className="mt-8">
        <h2 id="tiles-heading" className="mb-3 text-lg font-semibold">
          <Trans>Tile status</Trans>
        </h2>
        {tiles.length === 0 ? (
          <p className="text-sm text-muted-foreground">
            <Trans>
              No live tiles yet. They appear once the engine streams a snapshot.
            </Trans>
          </p>
        ) : (
          <ul className="grid gap-3 sm:grid-cols-2 lg:grid-cols-3">
            {tiles.map((tile) => (
              <li key={tile.id}>
                <Card>
                  <CardHeader className="flex-row items-center justify-between gap-2 pb-3">
                    <CardTitle className="truncate text-base" lang="" dir="auto">
                      {tile.id}
                    </CardTitle>
                    <TileStateBadge state={tile.state} />
                  </CardHeader>
                  <CardContent className="text-sm text-muted-foreground">
                    {tile.fps !== undefined ? (
                      <span aria-label={t`Frame rate`}>
                        {formatFps(locale, tile.fps)}
                      </span>
                    ) : (
                      <Trans>No frame-rate sample.</Trans>
                    )}
                  </CardContent>
                </Card>
              </li>
            ))}
          </ul>
        )}
      </section>
    </>
  );
}
