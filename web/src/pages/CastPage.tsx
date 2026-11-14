// Cast — two ways to show the engine on a Google Cast / Chromecast receiver:
//
//   * "Cast from this browser" (`../cast/BrowserCastSection`): this Chrome or
//     Edge is the Cast sender; the TV fetches a token URL from a cast output.
//   * "Cast from the engine (same network)", below: the engine dials the
//     receiver itself (ADR-M011).
//
// Engine side: an operator casts a running output's rendition to a Cast receiver on the
// network as an EPHEMERAL session, then optionally promotes it to a permanent
// managed device. Discovered Cast receivers come from the shared discovery
// inventory (driver_kind === 'cast'); the live ephemeral session list is POLLED
// (the cast.session.started/.removed realtime events live on a separate lane, so
// this surface refetches — a live-WS upgrade is a later drop-in). Volume is a
// 202 command: the applied level lands on the realtime stream, surfaced here as
// the accepted operation id.
import { useEffect, useMemo, useState } from 'react';
import type { JSX } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import { Cast, Save, Trash2, Volume2 } from 'lucide-react';

import {
  useCastSessions,
  useSaveCastSession,
  useSetCastVolume,
  useStartCastSession,
  useStopCastSession,
} from '../api/cast-sessionsQueries';
import type { CastSession } from '../api/cast-sessionsQueries';
import { DISCOVERY_INVENTORY_QUERY_KEY, useDiscoveredInventory } from '../devices/queries';
import { scanDevices } from '../devices/api';
import { useQueryClient } from '@tanstack/react-query';
import { feedFromKey } from '../api/outputRouting';
import { BrowserCastSection } from '../cast/BrowserCastSection';
import { CastFeedPicker, useFeedOptions } from '../cast/CastFeedPicker';
import { parseReceiverReport, receiverErrorCodes } from '../cast/castReport';
import { PageHeader } from '../components/PageHeader';
import { Badge } from '../components/ui/badge';
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

/** The start-cast dialog draft. */
interface StartDraft {
  readonly address: string;
  readonly name: string;
  readonly output: string;
  /** The feed select value (`bus:program`, `src:<id>`, `aux:<layout>`). */
  readonly feed: string;
}

/** The engine-side session state line: player state, idle reason, receiver report. */
function sessionStateLine(session: CastSession): string | null {
  const parts: string[] = [];
  if (typeof session.player_state === 'string' && session.player_state !== '') parts.push(session.player_state);
  if (typeof session.idle_reason === 'string' && session.idle_reason !== '') parts.push(session.idle_reason);
  const report = parseReceiverReport(session.last_report?.report);
  if (report !== null) {
    if (report.width !== null && report.height !== null) parts.push(`${String(report.width)}×${String(report.height)}`);
    if (report.latencyS !== null) parts.push(`${report.latencyS.toFixed(1)} s`);
    const codes = report.state === 'error' ? receiverErrorCodes(report.error) : null;
    if (report.error?.message !== null && report.error?.message !== undefined) parts.push(report.error.message);
    if (codes !== null) parts.push(codes);
  }
  return parts.length === 0 ? null : parts.join(' · ');
}

/** The save-as-device dialog draft (keyed to a session). */
interface SaveDraft {
  readonly session: CastSession;
  readonly deviceId: string;
  readonly displayName: string;
}

/** The volume dialog draft (keyed to a session). */
interface VolumeDraft {
  readonly session: CastSession;
  readonly levelPercent: string;
}

/** The cast-session management page. */
export function CastPage(): JSX.Element {
  const { t } = useLingui();
  const sessions = useCastSessions();
  const discovery = useDiscoveredInventory();
  const start = useStartCastSession();
  const stop = useStopCastSession();
  const save = useSaveCastSession();
  const volume = useSetCastVolume();
  const feedOptions = useFeedOptions();
  const queryClient = useQueryClient();
  // Ask the engine for a fresh mDNS browse when the page opens; the rows land
  // in the inventory once the scan's budget has passed.
  useEffect(() => {
    let timer: ReturnType<typeof setTimeout> | undefined;
    let live = true;
    scanDevices()
      .then((accepted) => {
        if (!live) return;
        timer = setTimeout(() => {
          void queryClient.invalidateQueries({ queryKey: DISCOVERY_INVENTORY_QUERY_KEY });
        }, Math.min(Math.max(accepted.budget_ms, 500), 15_000) + 250);
      })
      .catch(() => undefined);
    return (): void => {
      live = false;
      if (timer !== undefined) clearTimeout(timer);
    };
  }, [queryClient]);

  const [startDraft, setStartDraft] = useState<StartDraft | null>(null);
  const [saveDraft, setSaveDraft] = useState<SaveDraft | null>(null);
  const [volumeDraft, setVolumeDraft] = useState<VolumeDraft | null>(null);
  const [pendingStop, setPendingStop] = useState<CastSession | null>(null);

  const liveSessions = useMemo<CastSession[]>(() => sessions.data ?? [], [sessions.data]);
  // Discovered Cast receivers only (driver_kind === 'cast').
  const castDevices = useMemo(
    () => (discovery.data ?? []).filter((service) => service.driverKind === 'cast'),
    [discovery.data],
  );

  const openStart = (address = '', name = ''): void => {
    setStartDraft({ address, name, output: '', feed: '' });
  };

  const submitStart = (): void => {
    if (startDraft === null) {
      return;
    }
    const address = startDraft.address.trim();
    if (address === '') {
      toast({ title: t`A receiver address is required`, variant: 'destructive' });
      return;
    }
    const name = startDraft.name.trim();
    const output = startDraft.output.trim();
    const feed = startDraft.feed === '' ? undefined : feedFromKey(startDraft.feed);
    start.mutate(
      {
        address,
        ...(name !== '' ? { name } : {}),
        ...(output !== '' ? { output } : {}),
        ...(feed !== undefined ? { feed } : {}),
      },
      {
        onSuccess: (session): void => {
          toast({ title: t`Cast session started`, description: session.id });
          setStartDraft(null);
        },
        onError: (error): void => {
          toast({
            title: t`Could not start the cast session`,
            description: error.message,
            variant: 'destructive',
          });
        },
      },
    );
  };

  const submitSave = (): void => {
    if (saveDraft === null) {
      return;
    }
    const deviceId = saveDraft.deviceId.trim();
    if (deviceId === '') {
      toast({ title: t`A device id is required`, variant: 'destructive' });
      return;
    }
    const displayName = saveDraft.displayName.trim();
    save.mutate(
      {
        id: saveDraft.session.id,
        request: {
          device_id: deviceId,
          ...(displayName !== '' ? { display_name: displayName } : {}),
        },
      },
      {
        onSuccess: (resource): void => {
          toast({ title: t`Saved as device`, description: resource.id });
          setSaveDraft(null);
        },
        onError: (error): void => {
          toast({
            title: t`Could not save as a device`,
            description: error.message,
            variant: 'destructive',
          });
        },
      },
    );
  };

  const submitVolume = (): void => {
    if (volumeDraft === null) {
      return;
    }
    const level = Number.parseInt(volumeDraft.levelPercent, 10);
    if (!Number.isFinite(level) || level < 0 || level > 100) {
      toast({ title: t`Volume must be between 0 and 100`, variant: 'destructive' });
      return;
    }
    volume.mutate(
      { id: volumeDraft.session.id, request: { level_percent: level } },
      {
        onSuccess: (accepted): void => {
          toast({
            title: t`Volume change accepted`,
            description: t`Operation ${accepted.operation_id}; the level arrives on the realtime stream.`,
          });
          setVolumeDraft(null);
        },
        onError: (error): void => {
          toast({
            title: t`Could not set the volume`,
            description: error.message,
            variant: 'destructive',
          });
        },
      },
    );
  };

  const confirmStop = (): void => {
    const target = pendingStop;
    if (target === null) {
      return;
    }
    stop.mutate(target.id, {
      onSuccess: (): void => {
        toast({ title: t`Cast session stopped` });
      },
      onError: (error): void => {
        toast({
          title: t`Could not stop the cast session`,
          description: error.message,
          variant: 'destructive',
        });
      },
    });
    setPendingStop(null);
  };

  return (
    <>
      <PageHeader
        title={<Trans>Cast</Trans>}
        description={
          <Trans>
            Show Program, Preview, a layout or a source on a Chromecast or Google Cast TV: from this browser,
            or from the engine when it shares the TV&apos;s network.
          </Trans>
        }
      />

      <BrowserCastSection />

      <section aria-labelledby="cast-engine" className="mb-4 grid gap-2">
        <div className="flex flex-wrap items-center justify-between gap-2">
          <h2 id="cast-engine" className="text-sm font-semibold">
            <Trans>Cast from the engine (same network)</Trans>
          </h2>
          <Button onClick={(): void => { openStart(); }}>
            <Cast aria-hidden="true" />
            <Trans>Start cast</Trans>
          </Button>
        </div>
        <p className="text-sm text-muted-foreground">
          <Trans>
            The engine dials the TV itself, so this only works when the engine and the TV share a network. A
            session can be saved as a permanent device. The session list refreshes on its own.
          </Trans>
        </p>
      </section>

      <section aria-labelledby="cast-discovered" className="mb-8">
        <h3 id="cast-discovered" className="mb-2 text-sm font-semibold">
          <Trans>Discovered receivers</Trans>
        </h3>
        {discovery.isPending ? (
          <p role="status" aria-live="polite" className="text-sm text-muted-foreground">
            <Trans>Looking for Cast receivers…</Trans>
          </p>
        ) : discovery.isError ? (
          <p role="alert" className="text-sm text-destructive">
            <Trans>Could not load discovery:</Trans> {discovery.error.message}
          </p>
        ) : castDevices.length === 0 ? (
          <p className="text-sm text-muted-foreground">
            <Trans>
              No Cast receivers discovered. You can still start a session by
              entering a receiver address directly.
            </Trans>
          </p>
        ) : (
          <Table>
            <TableCaption>{t`Cast receivers seen on the network (untrusted hints).`}</TableCaption>
            <TableHeader>
              <TableRow>
                <TableHead>{t`Name`}</TableHead>
                <TableHead>{t`Model`}</TableHead>
                <TableHead>{t`Address`}</TableHead>
                <TableHead>{t`Cast`}</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {castDevices.map((service) => {
                const friendly = service.txt.fn ?? service.name;
                return (
                <TableRow key={service.key}>
                  <TableCell>
                    <span lang="" dir="auto" className="font-medium">
                      {friendly}
                    </span>
                  </TableCell>
                  <TableCell>
                    <span lang="" dir="auto" className="text-sm text-muted-foreground">
                      {service.txt.md ?? '—'}
                    </span>
                  </TableCell>
                  <TableCell>
                    <code className="text-xs text-muted-foreground" lang="" dir="auto">
                      {service.primaryAddress}
                    </code>
                  </TableCell>
                  <TableCell>
                    <Button
                      variant="outline"
                      size="sm"
                      aria-label={`${t`Cast to`}: ${friendly}`}
                      onClick={(): void => {
                        openStart(service.primaryAddress, friendly);
                      }}
                    >
                      <Cast aria-hidden="true" />
                      <Trans>Cast to this</Trans>
                    </Button>
                  </TableCell>
                </TableRow>
                );
              })}
            </TableBody>
          </Table>
        )}
      </section>

      <section aria-labelledby="cast-sessions">
        <h3 id="cast-sessions" className="mb-2 text-sm font-semibold">
          <Trans>Live sessions</Trans>
        </h3>
        {sessions.isPending ? (
          <p role="status" aria-live="polite" className="text-sm text-muted-foreground">
            <Trans>Loading cast sessions…</Trans>
          </p>
        ) : sessions.isError ? (
          <p role="alert" className="text-sm text-destructive">
            <Trans>Could not load cast sessions:</Trans> {sessions.error.message}
          </p>
        ) : liveSessions.length === 0 ? (
          <div className="rounded-md border border-dashed p-8 text-center">
            <p className="text-sm text-muted-foreground">
              <Trans>No cast sessions are running.</Trans>
            </p>
          </div>
        ) : (
          <Table>
            <TableCaption>{t`Live ephemeral cast sessions.`}</TableCaption>
            <TableHeader>
              <TableRow>
                <TableHead>{t`Name`}</TableHead>
                <TableHead>{t`Receiver`}</TableHead>
                <TableHead>{t`Output`}</TableHead>
                <TableHead>{t`State`}</TableHead>
                <TableHead>{t`Actions`}</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {liveSessions.map((session) => (
                <TableRow key={session.id}>
                  <TableCell>
                    <span lang="" dir="auto" className="font-medium">
                      {session.name !== null && session.name !== undefined && session.name !== ''
                        ? session.name
                        : session.id}
                    </span>
                  </TableCell>
                  <TableCell>
                    <code className="text-xs text-muted-foreground" lang="" dir="auto">
                      {session.address}
                    </code>
                  </TableCell>
                  <TableCell>
                    <code className="text-xs">{session.output}</code>
                  </TableCell>
                  <TableCell>
                    <Badge variant="outline">{session.state}</Badge>
                    {sessionStateLine(session) !== null ? (
                      <p className="mt-1 text-xs text-muted-foreground" lang="" dir="auto">
                        {sessionStateLine(session)}
                      </p>
                    ) : null}
                  </TableCell>
                  <TableCell>
                    <div className="flex items-center gap-1">
                      <Button
                        variant="outline"
                        size="sm"
                        aria-label={`${t`Set volume`}: ${session.id}`}
                        onClick={(): void => {
                          setVolumeDraft({ session, levelPercent: '50' });
                        }}
                      >
                        <Volume2 aria-hidden="true" />
                        <Trans>Volume</Trans>
                      </Button>
                      <Button
                        variant="outline"
                        size="sm"
                        aria-label={`${t`Save as device`}: ${session.id}`}
                        onClick={(): void => {
                          setSaveDraft({
                            session,
                            deviceId: '',
                            displayName: session.name ?? '',
                          });
                        }}
                      >
                        <Save aria-hidden="true" />
                        <Trans>Save as device</Trans>
                      </Button>
                      <Button
                        variant="ghost"
                        size="sm"
                        disabled={stop.isPending}
                        aria-label={`${t`Stop session`}: ${session.id}`}
                        onClick={(): void => {
                          setPendingStop(session);
                        }}
                      >
                        <Trash2 aria-hidden="true" />
                        <Trans>Stop</Trans>
                      </Button>
                    </div>
                  </TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
        )}
      </section>

      {/* Start-cast dialog */}
      <Dialog
        open={startDraft !== null}
        onOpenChange={(open): void => {
          if (!open) {
            setStartDraft(null);
          }
        }}
      >
        <DialogContent>
          <DialogHeader>
            <DialogTitle>
              <Trans>Start a cast session</Trans>
            </DialogTitle>
            <DialogDescription>
              <Trans>
                Cast a running output's rendition to a Cast receiver. Leave the
                output blank to use the first declared rendition.
              </Trans>
            </DialogDescription>
          </DialogHeader>
          {startDraft !== null ? (
            <div className="grid gap-4">
              <div className="grid gap-1.5">
                <Label htmlFor="cast-address">
                  <Trans>Receiver address</Trans>
                </Label>
                <Input
                  id="cast-address"
                  value={startDraft.address}
                  placeholder="[2001:db8::5]:8009"
                  onChange={(e): void => {
                    setStartDraft({ ...startDraft, address: e.target.value });
                  }}
                />
              </div>
              <div className="grid gap-1.5">
                <Label htmlFor="cast-name">
                  <Trans>Name (optional)</Trans>
                </Label>
                <Input
                  id="cast-name"
                  value={startDraft.name}
                  placeholder={t`e.g. Lobby TV`}
                  onChange={(e): void => {
                    setStartDraft({ ...startDraft, name: e.target.value });
                  }}
                />
              </div>
              <CastFeedPicker
                options={[{ key: '', label: t`The output's own rendition`, group: 'buses' }, ...feedOptions]}
                value={startDraft.feed}
                label={t`What to show`}
                onChange={(key): void => {
                  setStartDraft({ ...startDraft, feed: key });
                }}
              />
              <div className="grid gap-1.5">
                <Label htmlFor="cast-output">
                  <Trans>Output id (optional)</Trans>
                </Label>
                <Input
                  id="cast-output"
                  value={startDraft.output}
                  placeholder={t`first declared rendition`}
                  onChange={(e): void => {
                    setStartDraft({ ...startDraft, output: e.target.value });
                  }}
                />
              </div>
            </div>
          ) : null}
          <DialogFooter>
            <Button variant="outline" onClick={(): void => { setStartDraft(null); }}>
              <Trans>Cancel</Trans>
            </Button>
            <Button onClick={submitStart} disabled={start.isPending}>
              <Trans>Start</Trans>
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      {/* Save-as-device dialog */}
      <Dialog
        open={saveDraft !== null}
        onOpenChange={(open): void => {
          if (!open) {
            setSaveDraft(null);
          }
        }}
      >
        <DialogContent>
          <DialogHeader>
            <DialogTitle>
              <Trans>Save as a device</Trans>
            </DialogTitle>
            <DialogDescription>
              <Trans>
                Promote this ephemeral session to a permanent managed Cast
                device.
              </Trans>
            </DialogDescription>
          </DialogHeader>
          {saveDraft !== null ? (
            <div className="grid gap-4">
              <div className="grid gap-1.5">
                <Label htmlFor="cast-save-id">
                  <Trans>Device id</Trans>
                </Label>
                <Input
                  id="cast-save-id"
                  value={saveDraft.deviceId}
                  placeholder={t`e.g. cast-lobby`}
                  onChange={(e): void => {
                    setSaveDraft({ ...saveDraft, deviceId: e.target.value });
                  }}
                />
              </div>
              <div className="grid gap-1.5">
                <Label htmlFor="cast-save-name">
                  <Trans>Display name (optional)</Trans>
                </Label>
                <Input
                  id="cast-save-name"
                  value={saveDraft.displayName}
                  onChange={(e): void => {
                    setSaveDraft({ ...saveDraft, displayName: e.target.value });
                  }}
                />
              </div>
            </div>
          ) : null}
          <DialogFooter>
            <Button variant="outline" onClick={(): void => { setSaveDraft(null); }}>
              <Trans>Cancel</Trans>
            </Button>
            <Button onClick={submitSave} disabled={save.isPending}>
              <Trans>Save</Trans>
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      {/* Volume dialog */}
      <Dialog
        open={volumeDraft !== null}
        onOpenChange={(open): void => {
          if (!open) {
            setVolumeDraft(null);
          }
        }}
      >
        <DialogContent>
          <DialogHeader>
            <DialogTitle>
              <Trans>Set the receiver volume</Trans>
            </DialogTitle>
            <DialogDescription>
              <Trans>
                The volume change is accepted asynchronously; the applied level
                arrives on the realtime stream.
              </Trans>
            </DialogDescription>
          </DialogHeader>
          {volumeDraft !== null ? (
            <div className="grid gap-1.5">
              <Label htmlFor="cast-volume">
                <Trans>Volume (0–100)</Trans>
              </Label>
              <Input
                id="cast-volume"
                type="number"
                min={0}
                max={100}
                value={volumeDraft.levelPercent}
                onChange={(e): void => {
                  setVolumeDraft({ ...volumeDraft, levelPercent: e.target.value });
                }}
              />
            </div>
          ) : null}
          <DialogFooter>
            <Button variant="outline" onClick={(): void => { setVolumeDraft(null); }}>
              <Trans>Cancel</Trans>
            </Button>
            <Button onClick={submitVolume} disabled={volume.isPending}>
              <Trans>Set volume</Trans>
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      {/* Stop confirmation */}
      <Dialog
        open={pendingStop !== null}
        onOpenChange={(open): void => {
          if (!open) {
            setPendingStop(null);
          }
        }}
      >
        <DialogContent>
          <DialogHeader>
            <DialogTitle>
              <Trans>Stop this cast session?</Trans>
            </DialogTitle>
            <DialogDescription>
              <Trans>The receiver stops playing the cast output.</Trans>
            </DialogDescription>
          </DialogHeader>
          {pendingStop !== null ? (
            <p className="text-sm">
              <code>{pendingStop.id}</code>
            </p>
          ) : null}
          <DialogFooter>
            <Button variant="outline" onClick={(): void => { setPendingStop(null); }}>
              <Trans>Cancel</Trans>
            </Button>
            <Button variant="destructive" onClick={confirmStop}>
              <Trans>Stop</Trans>
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </>
  );
}
