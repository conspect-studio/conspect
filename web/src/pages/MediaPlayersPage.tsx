// Media players — the VT (video-tape) transport panel.
//
// Each configured media player (ADR-0057 + ADR-0097) is a pre-declared,
// bus-selectable channel that rolls an asset with VT-style transport. This page
// lists the players (`GET /api/v1/media/players`) and drives each through its
// transport verbs (`POST .../load|cue|play|pause|stop|seek` and the vamp exit
// `.../exit/arm|take|cancel`), each a `202 Accepted` whose operation id is shown
// in a toast. The player's live transport state, playhead, and loaded asset
// arrive on the realtime `media.player_state` stream (engine isolation, inv #10:
// a stale/absent stream just shows "no live state", never blocks).
//
// Trim (in/out/vamp frames) is a property of the referenced ASSET, not the
// player, so it is not shown per-player here — the live playhead + transport
// state is the authoritative VT readout.
import { useMemo, useState } from 'react';
import type { JSX } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import {
  CircleStop,
  FlagTriangleRight,
  FolderInput,
  Pause,
  Play,
  Repeat,
  SkipBack,
  XCircle,
} from 'lucide-react';

import {
  useMediaPlayerTransport,
  useMediaPlayers,
} from '../api/media-playersQueries';
import type {
  MediaPlayer,
  TransportAction,
} from '../api/media-playersQueries';
import { useMediaPlayerStates } from '../realtime/useEngineEvents';
import type { MediaPlayerEvent } from '../realtime/generated-types';
import { PageHeader } from '../components/PageHeader';
import { Badge } from '../components/ui/badge';
import type { BadgeProps } from '../components/ui/badge';
import { Button } from '../components/ui/button';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '../components/ui/dialog';
import { Input } from '../components/ui/input';
import { Label } from '../components/ui/label';
import {
  Table,
  TableBody,
  TableCaption,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '../components/ui/table';
import { toast } from '../components/ui/use-toast';

/** The transport-state `kind` discriminants we render badges for. */
type StateKind = MediaPlayerEvent['state']['kind'];

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null;
}

/** The configured-default asset id on a player body, if declared. */
function configuredDefaultAsset(player: MediaPlayer): string | undefined {
  const body: unknown = player.body;
  if (isRecord(body) && typeof body.default === 'string') {
    return body.default;
  }
  return undefined;
}

/** Whether the player's configured default loop is on (vamp-on-start). */
function configuredLoop(player: MediaPlayer): boolean {
  const body: unknown = player.body;
  return isRecord(body) && body.loop_default === true;
}

/** Map a transport-state kind to a status-badge variant. */
function stateVariant(kind: StateKind): BadgeProps['variant'] {
  switch (kind) {
    case 'playing':
    case 'vamping':
      return 'live';
    case 'cued':
    case 'paused':
      return 'reconnecting';
    case 'loading':
      return 'stale';
    case 'eof':
      return 'nosignal';
    case 'stopped':
      return 'offline';
  }
}

/** The media-player (VT) transport panel. */
export function MediaPlayersPage(): JSX.Element {
  const { t } = useLingui();
  const players = useMediaPlayers();
  const transport = useMediaPlayerTransport();
  const liveStates = useMediaPlayerStates();

  const [loadFor, setLoadFor] = useState<MediaPlayer | null>(null);
  const [assetDraft, setAssetDraft] = useState('');

  const data = useMemo<MediaPlayer[]>(() => players.data ?? [], [players.data]);

  /** Localized, human-readable label for a transport-state kind. */
  const stateLabel = (kind: StateKind): string => {
    switch (kind) {
      case 'loading':
        return t`Loading`;
      case 'cued':
        return t`Cued`;
      case 'playing':
        return t`Playing`;
      case 'paused':
        return t`Paused`;
      case 'stopped':
        return t`Stopped`;
      case 'vamping':
        return t`Vamping`;
      case 'eof':
        return t`End of file`;
    }
  };

  const runAction = (
    player: MediaPlayer,
    action: Exclude<TransportAction, 'load' | 'cue' | 'seek'>,
  ): void => {
    transport.mutate(
      { id: player.id, action },
      {
        onSuccess: (accepted): void => {
          toast({
            title: t`Command accepted`,
            description: `${t`Operation id`}: ${accepted.operation_id}`,
          });
        },
        onError: (error): void => {
          toast({
            title: t`Command failed`,
            description: error.message,
            variant: 'destructive',
          });
        },
      },
    );
  };

  const cueToInPoint = (player: MediaPlayer): void => {
    transport.mutate(
      { id: player.id, action: 'cue' },
      {
        onSuccess: (accepted): void => {
          toast({
            title: t`Cue accepted`,
            description: `${t`Operation id`}: ${accepted.operation_id}`,
          });
        },
        onError: (error): void => {
          toast({
            title: t`Command failed`,
            description: error.message,
            variant: 'destructive',
          });
        },
      },
    );
  };

  const openLoad = (player: MediaPlayer): void => {
    setAssetDraft(configuredDefaultAsset(player) ?? '');
    setLoadFor(player);
  };

  const submitLoad = (): void => {
    const player = loadFor;
    if (player === null) {
      return;
    }
    const asset = assetDraft.trim();
    if (asset === '') {
      toast({ title: t`An asset id is required`, variant: 'destructive' });
      return;
    }
    transport.mutate(
      { id: player.id, action: 'load', asset },
      {
        onSuccess: (accepted): void => {
          toast({
            title: t`Load accepted`,
            description: `${t`Operation id`}: ${accepted.operation_id}`,
          });
          setLoadFor(null);
        },
        onError: (error): void => {
          toast({
            title: t`Could not load asset`,
            description: error.message,
            variant: 'destructive',
          });
        },
      },
    );
  };

  return (
    <>
      <PageHeader
        title={<Trans>Media players</Trans>}
        description={
          <Trans>
            VT transport for the configured media-player channels. Load an asset,
            cue, play, pause, stop, and — for a vamp/loop fill — arm, take, or
            cancel a clean exit at the next vamp boundary. Each command is
            accepted asynchronously; the player&apos;s live state and playhead
            arrive on the realtime stream.
          </Trans>
        }
      />

      {players.isPending ? (
        <p role="status" aria-live="polite" className="text-sm text-muted-foreground">
          <Trans>Loading media players…</Trans>
        </p>
      ) : players.isError ? (
        <p role="alert" className="text-sm text-destructive">
          <Trans>Could not load media players:</Trans> {players.error.message}
        </p>
      ) : data.length === 0 ? (
        <div className="rounded-md border border-dashed p-8 text-center">
          <p className="text-sm text-muted-foreground">
            <Trans>
              No media players are configured. Declare them under
              <code className="mx-1">media_players</code> in the config.
            </Trans>
          </p>
        </div>
      ) : (
        <Table>
          <TableCaption>{t`All configured media players.`}</TableCaption>
          <TableHeader>
            <TableRow>
              <TableHead>{t`Player`}</TableHead>
              <TableHead>{t`State`}</TableHead>
              <TableHead>{t`Loaded asset`}</TableHead>
              <TableHead>{t`Configured default`}</TableHead>
              <TableHead className="text-right">{t`Position (frames)`}</TableHead>
              <TableHead>{t`Transport`}</TableHead>
              <TableHead>{t`Vamp exit`}</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {data.map((player) => {
              const live: MediaPlayerEvent | undefined = liveStates[player.id];
              const kind = live?.state.kind;
              const isVamping = live?.state.kind === 'vamping';
              const exitArmed =
                live?.state.kind === 'vamping' && live.state.exit_armed;
              // The LOADED asset is authoritative ONLY from live `media.player_state`
              // — never fabricated from config. A boot-idle player with a configured
              // default has loaded nothing until a load command confirms it on the
              // realtime stream, so show idle until then (rule 27 — no aspirational
              // state). The configured default is shown separately below.
              const loadedAsset = live?.asset;
              const configuredDefault = configuredDefaultAsset(player);
              const busy = transport.isPending;
              return (
                <TableRow key={player.id}>
                  <TableCell>
                    <div className="flex flex-col">
                      <span lang="" dir="auto" className="font-medium">
                        {player.name}
                      </span>
                      <code className="text-xs text-muted-foreground">
                        {player.id}
                      </code>
                    </div>
                  </TableCell>
                  <TableCell>
                    {kind !== undefined ? (
                      <Badge variant={stateVariant(kind)}>
                        {stateLabel(kind)}
                        {exitArmed ? ` · ${t`exit armed`}` : ''}
                      </Badge>
                    ) : (
                      <span className="text-xs text-muted-foreground">
                        <Trans>No live state</Trans>
                      </span>
                    )}
                  </TableCell>
                  <TableCell>
                    {loadedAsset !== undefined && loadedAsset !== '' ? (
                      <code className="text-xs">{loadedAsset}</code>
                    ) : (
                      <span className="text-xs text-muted-foreground">
                        <Trans>Not loaded</Trans>
                      </span>
                    )}
                  </TableCell>
                  <TableCell>
                    {configuredDefault !== undefined && configuredDefault !== '' ? (
                      <code className="text-xs text-muted-foreground">
                        {configuredDefault}
                      </code>
                    ) : (
                      <span className="text-xs text-muted-foreground">
                        <Trans>None</Trans>
                      </span>
                    )}
                  </TableCell>
                  <TableCell className="text-right tabular-nums">
                    {live !== undefined ? (
                      live.position_frames
                    ) : (
                      <span className="text-xs text-muted-foreground">—</span>
                    )}
                  </TableCell>
                  <TableCell>
                    <div className="flex flex-wrap items-center gap-1">
                      <Button
                        variant="outline"
                        size="sm"
                        disabled={busy}
                        aria-label={`${t`Load asset into player`}: ${player.id}`}
                        onClick={(): void => {
                          openLoad(player);
                        }}
                      >
                        <FolderInput aria-hidden="true" />
                        <Trans>Load</Trans>
                      </Button>
                      <Button
                        variant="outline"
                        size="sm"
                        disabled={busy}
                        aria-label={`${t`Cue player to in-point`}: ${player.id}`}
                        onClick={(): void => {
                          cueToInPoint(player);
                        }}
                      >
                        <SkipBack aria-hidden="true" />
                        <Trans>Cue</Trans>
                      </Button>
                      <Button
                        variant="outline"
                        size="sm"
                        disabled={busy}
                        aria-label={`${t`Play player`}: ${player.id}`}
                        onClick={(): void => {
                          runAction(player, 'play');
                        }}
                      >
                        <Play aria-hidden="true" />
                        <Trans>Play</Trans>
                      </Button>
                      <Button
                        variant="outline"
                        size="sm"
                        disabled={busy}
                        aria-label={`${t`Pause player`}: ${player.id}`}
                        onClick={(): void => {
                          runAction(player, 'pause');
                        }}
                      >
                        <Pause aria-hidden="true" />
                        <Trans>Pause</Trans>
                      </Button>
                      <Button
                        variant="ghost"
                        size="sm"
                        disabled={busy}
                        aria-label={`${t`Stop player`}: ${player.id}`}
                        onClick={(): void => {
                          runAction(player, 'stop');
                        }}
                      >
                        <CircleStop aria-hidden="true" />
                        <Trans>Stop</Trans>
                      </Button>
                      {configuredLoop(player) ? (
                        <Badge variant="secondary" className="gap-1">
                          <Repeat aria-hidden="true" className="size-3" />
                          <Trans>Loop default</Trans>
                        </Badge>
                      ) : null}
                    </div>
                  </TableCell>
                  <TableCell>
                    <div className="flex flex-wrap items-center gap-1">
                      <Button
                        variant="outline"
                        size="sm"
                        disabled={busy || !isVamping || exitArmed}
                        aria-label={`${t`Arm vamp exit`}: ${player.id}`}
                        onClick={(): void => {
                          runAction(player, 'arm-exit');
                        }}
                      >
                        <FlagTriangleRight aria-hidden="true" />
                        <Trans>Arm exit</Trans>
                      </Button>
                      <Button
                        variant="outline"
                        size="sm"
                        disabled={busy || !isVamping}
                        aria-label={`${t`Take vamp exit`}: ${player.id}`}
                        onClick={(): void => {
                          runAction(player, 'take-exit');
                        }}
                      >
                        <Play aria-hidden="true" />
                        <Trans>Take exit</Trans>
                      </Button>
                      <Button
                        variant="ghost"
                        size="sm"
                        disabled={busy || !exitArmed}
                        aria-label={`${t`Cancel vamp exit`}: ${player.id}`}
                        onClick={(): void => {
                          runAction(player, 'cancel-exit');
                        }}
                      >
                        <XCircle aria-hidden="true" />
                        <Trans>Cancel exit</Trans>
                      </Button>
                    </div>
                  </TableCell>
                </TableRow>
              );
            })}
          </TableBody>
        </Table>
      )}

      <Dialog
        open={loadFor !== null}
        onOpenChange={(open): void => {
          if (!open) {
            setLoadFor(null);
          }
        }}
      >
        <DialogContent>
          <DialogHeader>
            <DialogTitle>
              <Trans>Load asset</Trans>
            </DialogTitle>
            <DialogDescription>
              <Trans>
                Load a media-library asset into this player. It cues to the
                asset&apos;s in-point, ready to take.
              </Trans>
            </DialogDescription>
          </DialogHeader>
          {loadFor !== null ? (
            <div className="grid gap-4">
              <p className="text-sm">
                <Trans>Player</Trans>: <code>{loadFor.id}</code>
              </p>
              <div className="grid gap-1.5">
                <Label htmlFor="media-player-asset">
                  <Trans>Asset id</Trans>
                </Label>
                <Input
                  id="media-player-asset"
                  value={assetDraft}
                  placeholder={t`Media-library asset id`}
                  onChange={(e): void => {
                    setAssetDraft(e.target.value);
                  }}
                />
              </div>
            </div>
          ) : null}
          <DialogFooter>
            <Button
              variant="outline"
              onClick={(): void => {
                setLoadFor(null);
              }}
            >
              <Trans>Cancel</Trans>
            </Button>
            <Button onClick={submitLoad} disabled={transport.isPending}>
              <Trans>Load</Trans>
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </>
  );
}
