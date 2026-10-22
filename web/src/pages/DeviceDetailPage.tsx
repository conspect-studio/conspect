// Device detail — Overview / Streams / Sync / Maintenance / Events
// (managed-devices.md §9) with the §11 failure UX.
//
// The page joins the stored desired state with the runtime status lane
// (conflated `device.status` WS topic first, REST fallback otherwise). The
// failure panel gives the operator the §11 guidance: UNREACHABLE shows
// last-seen + the supervised-reconnect story and a "Probe now" override;
// AUTH_FAILED is distinct, prompts a secret update, and explains that there
// are NO blind retries. Maintenance verbs ride the documented routes —
// long-running ones return 202 + operation id (outcome on the realtime
// stream), and set-mode shows the declared DEV-class impact BEFORE the
// operator applies anything (ADR-M009, instant-apply doctrine). Stream
// binding creates ORDINARY Sources/Outputs carrying `device_ref`.
import { useState } from 'react';
import type { JSX, ReactNode } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import { useQuery } from '@tanstack/react-query';
import { Link, useNavigate, useParams, useSearchParams } from 'react-router-dom';
import {
  Activity,
  KeyRound,
  MonitorPlay,
  Power,
  RadioTower,
  Wrench,
} from 'lucide-react';

import {
  identifyDevice,
  measureSyncGroup,
  probeDevice,
  rebootDevice,
  setDeviceMode,
  testPatternDevice,
  toDeviceView,
} from '../devices/api';
import type { OutputTargetView, SourceCandidateView } from '../devices/api';
import { DeviceStateBadge } from '../devices/DeviceStateBadge';
import { InputStreamsInspector } from '../devices/InputStreamsInspector';
import { LastSeenCell } from '../devices/lastSeen';
import {
  useDeviceStatuses,
  useEngineClockRef,
  useOutputTargets,
  useSourceCandidates,
  useSyncGroups,
} from '../devices/queries';
import type { DeviceView } from '../devices/types';
import { getResource } from '../resources/api';
import { useSaveResource } from '../resources/queries';
import {
  emptyOutputForm,
  emptySourceForm,
  outputFormToBody,
  sourceFormToBody,
  validateOutputForm,
  validateSourceForm,
  withSourceKind,
} from '../resources/forms';
import type {
  FieldErrors,
  OutputField,
  OutputFormState,
  SourceField,
  SourceFormState,
} from '../resources/forms';
import type { OutputKind } from '../resources/types';
import type { SourceFormKind } from '../resources/forms';
import { FormField } from '../resources/FormControls';
import {
  DEVICE_EVENTS_QUERY_KEY,
} from '../realtime/useEngineEvents';
import type { DeviceEventEntry } from '../realtime/useEngineEvents';
import type { DeviceStatus } from '../realtime/generated-types';
import { HelpLink } from '../components/HelpLink';
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
import {
  Tabs,
  TabsContent,
  TabsList,
  TabsTrigger,
} from '../components/ui/tabs';
import { toast } from '../components/ui/use-toast';

/** The continuity statement (§11): shown on every device, always. */
function ContinuityNote(): JSX.Element {
  return (
    <p role="note" className="mb-4 rounded-md border bg-muted/40 p-3 text-sm">
      <Trans>
        Program output never depends on this device: if it fails, bound
        sources ride the tile ladder and outputs ride their failover policy —
        the multiview keeps running.
      </Trans>
    </p>
  );
}

/** The §11 failure-guidance panel for UNREACHABLE / AUTH_FAILED. */
function FailureGuidance({
  device,
  status,
  onProbe,
  onUpdateCredentials,
}: {
  readonly device: DeviceView;
  readonly status: DeviceStatus;
  readonly onProbe: () => void;
  readonly onUpdateCredentials: () => void;
}): JSX.Element | null {
  const clock = useEngineClockRef();
  if (status.state === 'UNREACHABLE') {
    return (
      <div
        data-testid="failure-guidance"
        role="alert"
        className="mb-4 flex flex-wrap items-center justify-between gap-3 rounded-md border border-destructive/40 p-3 text-sm"
      >
        <p className="max-w-prose">
          <Trans>
            The device is unreachable. The control plane keeps trying to
            reconnect with backoff and jitter, and re-converges the desired
            state when the device answers — no action is required. Last seen:
          </Trans>{' '}
          <LastSeenCell lastSeenTs={status.last_seen_ts ?? undefined} clock={clock} />
        </p>
        <Button variant="outline" size="sm" onClick={onProbe}>
          <RadioTower aria-hidden="true" />
          <Trans>Probe now</Trans>
        </Button>
      </div>
    );
  }
  if (status.state === 'AUTH_FAILED') {
    return (
      <div
        data-testid="failure-guidance"
        role="alert"
        className="mb-4 flex flex-wrap items-center justify-between gap-3 rounded-md border border-destructive/40 p-3 text-sm"
      >
        <p className="max-w-prose">
          <Trans>
            The device rejected its credentials ({device.id}). There are no
            blind retries — probing is paused until the credentials secret
            reference is updated, and the next probe fires automatically when
            it changes.
          </Trans>
        </p>
        <Button variant="outline" size="sm" onClick={onUpdateCredentials}>
          <KeyRound aria-hidden="true" />
          <Trans>Update credentials</Trans>
        </Button>
      </div>
    );
  }
  return null;
}

/** A labelled definition row in the Overview tab. */
function OverviewRow({
  label,
  children,
}: {
  readonly label: ReactNode;
  readonly children: ReactNode;
}): JSX.Element {
  return (
    <div className="flex flex-wrap items-center gap-2">
      <dt className="w-40 shrink-0 text-sm text-muted-foreground">{label}</dt>
      <dd className="text-sm">{children}</dd>
    </div>
  );
}

/** Capability chips, only when the driver has probed them (never invented). */
function CapabilityChips({ status }: { readonly status: DeviceStatus | undefined }): JSX.Element {
  const capabilities = status?.capabilities;
  if (capabilities === undefined) {
    return (
      <span className="text-sm text-muted-foreground">
        <Trans>Not probed yet.</Trans>
      </span>
    );
  }
  return (
    <span className="inline-flex flex-wrap gap-1.5">
      {capabilities.encode ? <Badge variant="outline">{'encode'}</Badge> : null}
      {capabilities.decode ? <Badge variant="outline">{'decode'}</Badge> : null}
      {capabilities.display ? <Badge variant="outline">{'display'}</Badge> : null}
      {capabilities.audio ? <Badge variant="outline">{'audio'}</Badge> : null}
      {capabilities.reboot ? <Badge variant="outline">{'reboot'}</Badge> : null}
      {capabilities.firmware_update ? (
        <Badge variant="outline">{'firmware-update'}</Badge>
      ) : null}
      <Badge variant="outline">{`sync: ${capabilities.sync}`}</Badge>
    </span>
  );
}

const ZOWIETEK_MODES: readonly string[] = ['encoder', 'decoder'];

/** Map a candidate's transport kind onto the source form kind. */
function candidateSourceKind(kind: string): SourceFormKind {
  switch (kind) {
    case 'srt':
      return 'srt';
    case 'ndi':
      return 'ndi';
    default:
      return 'rtsp';
  }
}

/** Map a decode target's transport kind onto the output display kind. */
function targetOutputKind(kind: string): OutputKind {
  switch (kind) {
    case 'srt':
      return 'srt';
    case 'rtmp':
      return 'rtmp';
    case 'ndi':
      return 'ndi';
    default:
      return 'rtsp';
  }
}

/** The §9 tab ids; `?tab=` deep links (e.g. the list's stream column) validate against this set. */
const DETAIL_TABS = ['overview', 'streams', 'sync', 'maintenance', 'events'] as const;

/** A device-detail tab id. */
type DetailTab = (typeof DETAIL_TABS)[number];

/** The initial tab for a `?tab=` query value; unknown/absent falls back to Overview. */
function detailTabFromParam(param: string | null): DetailTab {
  return DETAIL_TABS.find((tab) => tab === param) ?? 'overview';
}

/** Device detail with the §9 tabs. */
export function DeviceDetailPage(): JSX.Element {
  const { t } = useLingui();
  const params = useParams();
  const navigate = useNavigate();
  const [searchParams] = useSearchParams();
  const deviceId = params.id ?? '';
  const initialTab = detailTabFromParam(searchParams.get('tab'));

  const record = useQuery({
    queryKey: ['devices', 'record', deviceId],
    queryFn: async () => getResource('devices', deviceId),
    enabled: deviceId !== '',
  });
  const device = record.data === undefined ? undefined : toDeviceView(record.data.record);
  const statuses = useDeviceStatuses(deviceId === '' ? [] : [deviceId]);
  const status = statuses[deviceId];
  const clock = useEngineClockRef();
  const groups = useSyncGroups();
  const candidates = useSourceCandidates(deviceId === '' ? undefined : deviceId);
  const targets = useOutputTargets(deviceId === '' ? undefined : deviceId);
  const saveSource = useSaveResource('sources');
  const saveOutput = useSaveResource('outputs');

  // Session ring of lifecycle events, scoped to this device (passive read).
  const eventsRing = useQuery<readonly DeviceEventEntry[]>({
    queryKey: DEVICE_EVENTS_QUERY_KEY,
    queryFn: (): readonly DeviceEventEntry[] => [],
    enabled: false,
    initialData: [],
  }).data;
  const deviceEvents = eventsRing.filter((entry) => entry.event.deviceId === deviceId);

  const [rebootOpen, setRebootOpen] = useState(false);
  const [modeOpen, setModeOpen] = useState(false);
  const [modeChoice, setModeChoice] = useState<string | undefined>(undefined);
  const [bindSource, setBindSource] = useState<
    { candidate: SourceCandidateView; form: SourceFormState } | null
  >(null);
  const [bindSourceErrors, setBindSourceErrors] = useState<FieldErrors<SourceField>>({});
  const [bindOutput, setBindOutput] = useState<
    { target: OutputTargetView; form: OutputFormState } | null
  >(null);
  const [bindOutputErrors, setBindOutputErrors] = useState<FieldErrors<OutputField>>({});

  const actionToast = (title: string) => (error: unknown): void => {
    toast({
      title,
      description: error instanceof Error ? error.message : String(error),
      variant: 'destructive',
    });
  };

  const probeNow = (): void => {
    probeDevice(deviceId)
      .then((): void => {
        toast({
          title: t`Probe requested`,
          description: t`The driver probes on its own cadence; the latest status shows here as it lands.`,
        });
      })
      .catch(actionToast(t`Could not probe`));
  };

  const updateCredentials = (): void => {
    void navigate(`/devices?edit=${encodeURIComponent(deviceId)}`);
  };

  if (record.isPending) {
    return (
      <p role="status" aria-live="polite" className="text-sm text-muted-foreground">
        <Trans>Loading device…</Trans>
      </p>
    );
  }
  if (record.isError || device === undefined) {
    return (
      <p role="alert" className="text-sm text-destructive">
        <Trans>Could not load device:</Trans> {record.error?.message ?? ''}
      </p>
    );
  }

  const currentMode = status?.mode ?? undefined;
  const defaultModeChoice =
    ZOWIETEK_MODES.find((mode) => mode !== currentMode) ?? 'decoder';
  const activeModeChoice = modeChoice ?? defaultModeChoice;

  const memberGroups = (groups.data ?? []).filter((group) =>
    group.members.some((member) => member.device === deviceId),
  );

  const submitBindSource = (): void => {
    if (bindSource === null) {
      return;
    }
    const errors = validateSourceForm(bindSource.form, true);
    setBindSourceErrors(errors);
    if (Object.keys(errors).length > 0) {
      return;
    }
    saveSource.mutate(
      {
        id: bindSource.form.id.trim(),
        create: true,
        input: {
          name: bindSource.form.name.trim(),
          body: sourceFormToBody(bindSource.form),
        },
      },
      {
        onSuccess: (): void => {
          toast({
            title: t`Source bound`,
            description: t`An ordinary managed source was created carrying device_ref ${deviceId}.`,
          });
          setBindSource(null);
        },
        onError: actionToast(t`Could not bind the source`),
      },
    );
  };

  const submitBindOutput = (): void => {
    if (bindOutput === null) {
      return;
    }
    const errors = validateOutputForm(bindOutput.form, true);
    setBindOutputErrors(errors);
    if (Object.keys(errors).length > 0) {
      return;
    }
    saveOutput.mutate(
      {
        id: bindOutput.form.id.trim(),
        create: true,
        input: {
          name: bindOutput.form.name.trim(),
          body: outputFormToBody(bindOutput.form),
        },
      },
      {
        onSuccess: (): void => {
          toast({
            title: t`Output bound`,
            description: t`An ordinary managed output was created carrying device_ref ${deviceId}; the driver points the device's decode slot at it.`,
          });
          setBindOutput(null);
        },
        onError: actionToast(t`Could not bind the output`),
      },
    );
  };

  return (
    <>
      <PageHeader
        title={<span lang="" dir="auto">{device.name}</span>}
        description={
          <span className="inline-flex flex-wrap items-center gap-2">
            <Badge variant="outline">{device.rawDriver}</Badge>
            {status !== undefined ? <DeviceStateBadge state={status.state} /> : null}
            {device.address !== undefined ? (
              <code className="text-xs" dir="ltr">
                {device.address}
              </code>
            ) : null}
            <HelpLink to="/help/devices" label={t`About managed devices`} compact />
          </span>
        }
        actions={
          <Button asChild variant="outline">
            <Link to="/devices">
              <Trans>All devices</Trans>
            </Link>
          </Button>
        }
      />

      <ContinuityNote />

      {status !== undefined ? (
        <FailureGuidance
          device={device}
          status={status}
          onProbe={probeNow}
          onUpdateCredentials={updateCredentials}
        />
      ) : null}

      <Tabs defaultValue={initialTab}>
        <TabsList>
          <TabsTrigger value="overview">
            <Trans>Overview</Trans>
          </TabsTrigger>
          <TabsTrigger value="streams">
            <Trans>Streams</Trans>
          </TabsTrigger>
          <TabsTrigger value="sync">
            <Trans>Sync</Trans>
          </TabsTrigger>
          <TabsTrigger value="maintenance">
            <Trans>Maintenance</Trans>
          </TabsTrigger>
          <TabsTrigger value="events">
            <Trans>Events</Trans>
          </TabsTrigger>
        </TabsList>

        <TabsContent value="overview">
          <dl className="flex flex-col gap-2 py-4">
            <OverviewRow label={<Trans>Driver</Trans>}>
              <Badge variant="outline">{device.rawDriver}</Badge>
            </OverviewRow>
            <OverviewRow label={<Trans>Management address</Trans>}>
              {device.address !== undefined ? (
                <code dir="ltr">{device.address}</code>
              ) : (
                <Trans>none (located by enrolled identity)</Trans>
              )}
            </OverviewRow>
            {/* The lifecycle state badge lives in the page header (always
                visible); repeating it here would duplicate the at-a-glance
                signal without adding information. */}
            <OverviewRow label={<Trans>Mode</Trans>}>
              {currentMode ?? <span aria-hidden="true">—</span>}
              {device.desiredMode !== undefined && device.desiredMode !== currentMode ? (
                <span className="ms-2 text-xs text-muted-foreground">
                  <Trans>(desired: {device.desiredMode})</Trans>
                </span>
              ) : null}
            </OverviewRow>
            <OverviewRow label={<Trans>Temperature</Trans>}>
              {status?.temperature_c !== undefined ? (
                `${String(status.temperature_c)} °C`
              ) : (
                <span aria-hidden="true">—</span>
              )}
            </OverviewRow>
            <OverviewRow label={<Trans>Last seen</Trans>}>
              <LastSeenCell lastSeenTs={status?.last_seen_ts ?? undefined} clock={clock} />
            </OverviewRow>
            <OverviewRow label={<Trans>Capabilities</Trans>}>
              <CapabilityChips status={status} />
            </OverviewRow>
          </dl>
        </TabsContent>

        <TabsContent value="streams">
          <div className="flex flex-col gap-6 py-4">
            <section aria-labelledby="device-streams">
              <h2 id="device-streams" className="mb-2 text-sm font-semibold">
                <Trans>Device-reported streams</Trans>
              </h2>
              {status?.streams !== undefined && status.streams.length > 0 ? (
                <ul className="flex flex-col gap-2">
                  {status.streams.map((stream, index) => (
                    <li
                      key={`${stream.role}-${String(index)}`}
                      className="flex flex-wrap items-center gap-2 rounded-md border p-2 text-sm"
                    >
                      <Badge variant="outline">{stream.role}</Badge>
                      {stream.healthy ? (
                        <Badge variant="live">
                          <Activity className="size-3.5" aria-hidden="true" />
                          <span>
                            <Trans>healthy</Trans>
                          </span>
                        </Badge>
                      ) : (
                        <span className="text-destructive">
                          <Trans>
                            decoding stalled (device-reported) — program output
                            is unaffected
                          </Trans>
                        </span>
                      )}
                      {stream.bitrate_bps !== undefined ? (
                        <span className="text-xs text-muted-foreground">
                          {`${(stream.bitrate_bps / 1_000_000).toFixed(1)} Mb/s`}
                        </span>
                      ) : null}
                      {stream.fps !== undefined ? (
                        <span className="text-xs text-muted-foreground">
                          {`${String(stream.fps)} fps`}
                        </span>
                      ) : null}
                      {stream.output_ref !== undefined ? (
                        <code className="text-xs">{stream.output_ref}</code>
                      ) : null}
                    </li>
                  ))}
                </ul>
              ) : (
                <p className="text-sm text-muted-foreground">
                  <Trans>The device reports no active streams.</Trans>
                </p>
              )}
            </section>

            <section aria-labelledby="device-candidates">
              <h2 id="device-candidates" className="mb-2 text-sm font-semibold">
                <Trans>Bindable streams</Trans>
              </h2>
              <p className="mb-2 max-w-prose text-sm text-muted-foreground">
                <Trans>
                  Streams the device serves. Binding one creates an ordinary
                  managed Source carrying device_ref — the engine ingest path
                  is untouched.
                </Trans>
              </p>
              {(candidates.data ?? []).length === 0 ? (
                <p className="text-sm text-muted-foreground">
                  <Trans>
                    Nothing enumerated yet — the list fills in once the
                    device's driver has probed it.
                  </Trans>
                </p>
              ) : (
                <ul className="flex flex-col gap-2">
                  {(candidates.data ?? []).map((candidate) => (
                    <li
                      key={candidate.id}
                      className="flex flex-wrap items-center justify-between gap-2 rounded-md border p-2 text-sm"
                    >
                      <span className="inline-flex flex-wrap items-center gap-2">
                        <span className="font-medium">{candidate.id}</span>
                        <Badge variant="outline">{candidate.kind}</Badge>
                        {candidate.url !== undefined ? (
                          <code className="text-xs" dir="ltr">
                            {candidate.url}
                          </code>
                        ) : null}
                        {candidate.unverified ? (
                          <Badge variant="stale">
                            <span>
                              <Trans>unverified</Trans>
                            </span>
                          </Badge>
                        ) : null}
                      </span>
                      <Button
                        size="sm"
                        variant="outline"
                        aria-label={`${t`Bind as source`}: ${candidate.id}`}
                        onClick={(): void => {
                          const kind = candidateSourceKind(candidate.kind);
                          const base = withSourceKind(emptySourceForm(), kind);
                          setBindSourceErrors({});
                          setBindSource({
                            candidate,
                            form: {
                              ...base,
                              url: candidate.url ?? '',
                              extra: { device_ref: deviceId },
                            },
                          });
                        }}
                      >
                        <MonitorPlay aria-hidden="true" />
                        <Trans>Bind as source…</Trans>
                      </Button>
                    </li>
                  ))}
                </ul>
              )}
            </section>

            <section aria-labelledby="device-input-streams">
              <h2 id="device-input-streams" className="mb-2 text-sm font-semibold">
                <Trans>Input elementary streams</Trans>
              </h2>
              <p className="mb-2 max-w-prose text-sm text-muted-foreground">
                <Trans>
                  Inspect the cached elementary-stream inventory of a configured
                  input (read-only). A bindable stream above becomes an input id
                  once you bind it as a source.
                </Trans>
              </p>
              <InputStreamsInspector
                suggestions={(candidates.data ?? []).map((candidate) => candidate.id)}
              />
            </section>

            <section aria-labelledby="device-targets">
              <h2 id="device-targets" className="mb-2 text-sm font-semibold">
                <Trans>Decode targets</Trans>
              </h2>
              <p className="mb-2 max-w-prose text-sm text-muted-foreground">
                <Trans>
                  Decode slots the device offers. Binding one creates an
                  ordinary managed Output carrying device_ref; the driver
                  points the slot at the rendition.
                </Trans>
              </p>
              {(targets.data ?? []).length === 0 ? (
                <p className="text-sm text-muted-foreground">
                  <Trans>
                    Nothing enumerated yet — decode slots appear once the
                    driver reads the device's decode table.
                  </Trans>
                </p>
              ) : (
                <ul className="flex flex-col gap-2">
                  {(targets.data ?? []).map((target) => {
                    const label = target.label ?? target.id;
                    return (
                      <li
                        key={target.id}
                        className="flex flex-wrap items-center justify-between gap-2 rounded-md border p-2 text-sm"
                      >
                        <span className="inline-flex flex-wrap items-center gap-2">
                          <span className="font-medium">{label}</span>
                          <Badge variant="outline">{target.kind}</Badge>
                        </span>
                        <Button
                          size="sm"
                          variant="outline"
                          aria-label={`${t`Bind as output`}: ${label}`}
                          onClick={(): void => {
                            setBindOutputErrors({});
                            setBindOutput({
                              target,
                              form: {
                                ...emptyOutputForm(),
                                kind: targetOutputKind(target.kind),
                                extra: { device_ref: deviceId },
                              },
                            });
                          }}
                        >
                          <MonitorPlay aria-hidden="true" />
                          <Trans>Bind as output…</Trans>
                        </Button>
                      </li>
                    );
                  })}
                </ul>
              )}
            </section>
          </div>
        </TabsContent>

        <TabsContent value="sync">
          <div className="flex flex-col gap-3 py-4 text-sm">
            {device.driver === 'cast' ? (
              <p className="max-w-prose text-muted-foreground">
                <Trans>
                  Cast devices never join a synchronized canvas (their latency
                  is multiple seconds and uncontrolled). Use a display node or
                  a vendor decoder for synchronized walls.
                </Trans>
              </p>
            ) : memberGroups.length === 0 ? (
              <p className="max-w-prose text-muted-foreground">
                <Trans>Not a member of any sync group.</Trans>{' '}
                <Link to="/sync-groups" className="underline underline-offset-2">
                  <Trans>Manage sync groups</Trans>
                </Link>
              </p>
            ) : (
              memberGroups.map((group) => {
                const member = group.members.find((m) => m.device === deviceId);
                const sync = status?.sync;
                const measured = sync?.group === group.id ? sync : undefined;
                return (
                  <div
                    key={group.id}
                    className="flex flex-wrap items-center justify-between gap-2 rounded-md border p-3"
                  >
                    <span className="inline-flex flex-wrap items-center gap-2">
                      <span className="font-medium" lang="" dir="auto">
                        {group.name}
                      </span>
                      <Badge variant="outline">{`target ${String(group.targetSkewMs)} ms`}</Badge>
                      {member !== undefined ? (
                        <Badge variant="outline">{`offset ${String(member.offsetMs)} ms`}</Badge>
                      ) : null}
                      {measured !== undefined ? (
                        <Badge variant="outline">{`achieved: ${measured.achieved}`}</Badge>
                      ) : (
                        <span className="text-xs text-muted-foreground">
                          <Trans>achieved tier not measured yet</Trans>
                        </span>
                      )}
                    </span>
                    <Button
                      size="sm"
                      variant="outline"
                      aria-label={`${t`Measure skew`}: ${group.name}`}
                      onClick={(): void => {
                        measureSyncGroup(group.id)
                          .then((accepted): void => {
                            toast({
                              title: t`Measurement running`,
                              description: t`Operation ${accepted.operation_id}; the result arrives on the realtime stream.`,
                            });
                          })
                          .catch(actionToast(t`Could not measure`));
                      }}
                    >
                      <Trans>Measure now</Trans>
                    </Button>
                  </div>
                );
              })
            )}
            <HelpLink to="/help/sync" label={t`About synchronized output and tiers`} />
          </div>
        </TabsContent>

        <TabsContent value="maintenance">
          <div className="flex flex-col gap-4 py-4">
            <div className="flex flex-wrap gap-2">
              <Button variant="outline" onClick={probeNow}>
                <RadioTower aria-hidden="true" />
                <Trans>Probe now</Trans>
              </Button>
              <Button
                variant="outline"
                onClick={(): void => {
                  identifyDevice(deviceId)
                    .then((): void => {
                      toast({
                        title: t`Identify requested`,
                        description: t`The device flashes its identify indicator.`,
                      });
                    })
                    .catch(actionToast(t`Could not identify`));
                }}
              >
                <Trans>Identify</Trans>
              </Button>
              <Button
                variant="outline"
                onClick={(): void => {
                  testPatternDevice(deviceId)
                    .then((): void => {
                      toast({
                        title: t`Test pattern requested`,
                        description: t`The device shows its test pattern.`,
                      });
                    })
                    .catch(actionToast(t`Could not show the test pattern`));
                }}
              >
                <Trans>Test pattern</Trans>
              </Button>
              {device.driver === 'zowietek' ? (
                <Button
                  variant="outline"
                  onClick={(): void => {
                    setModeChoice(undefined);
                    setModeOpen(true);
                  }}
                >
                  <Wrench aria-hidden="true" />
                  <Trans>Change mode…</Trans>
                </Button>
              ) : null}
              <Button
                variant="destructive"
                onClick={(): void => {
                  setRebootOpen(true);
                }}
              >
                <Power aria-hidden="true" />
                <Trans>Reboot…</Trans>
              </Button>
            </div>
            <p className="max-w-prose text-sm text-muted-foreground">
              <Trans>
                Long-running actions are accepted with an operation id and
                their outcome arrives on the realtime stream — watch the
                Events tab. None of them can interrupt Multiview program
                output.
              </Trans>
            </p>
          </div>
        </TabsContent>

        <TabsContent value="events">
          <div className="flex flex-col gap-2 py-4">
            {deviceEvents.length === 0 ? (
              <p className="text-sm text-muted-foreground">
                <Trans>
                  No device events seen in this session yet — lifecycle
                  changes (adopt, mode, errors) stream in live while the app
                  is open.
                </Trans>
              </p>
            ) : (
              <ul className="flex flex-col gap-2">
                {deviceEvents.map((entry) => (
                  <li
                    key={entry.seq}
                    className="flex flex-wrap items-center gap-2 rounded-md border p-2 text-sm"
                  >
                    <Badge variant="outline">{entry.event.kind}</Badge>
                    {entry.event.kind === 'mode' ? (
                      <span>
                        <Trans>
                          mode {entry.event.mode} — {entry.event.phase}
                        </Trans>
                        {entry.event.detail === undefined ? '' : ` (${entry.event.detail})`}
                      </span>
                    ) : null}
                    {entry.event.kind === 'error' ? (
                      <span className="text-destructive">{entry.event.message}</span>
                    ) : null}
                    {entry.event.kind === 'adopted' ? (
                      <span>
                        <Trans>adopted (driver {entry.event.driver})</Trans>
                      </span>
                    ) : null}
                    {entry.event.kind === 'removed' ? (
                      <span>
                        <Trans>removed from the registry</Trans>
                      </span>
                    ) : null}
                    <span className="text-xs text-muted-foreground">
                      <LastSeenCell lastSeenTs={entry.ts} clock={clock} />
                    </span>
                  </li>
                ))}
              </ul>
            )}
          </div>
        </TabsContent>
      </Tabs>

      {/* Reboot confirmation (destructive; nothing fires until confirmed). */}
      <Dialog open={rebootOpen} onOpenChange={setRebootOpen}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>
              <Trans>Reboot this device?</Trans>
            </DialogTitle>
            <DialogDescription>
              <Trans>
                The device restarts and is unreachable while it boots. Bound
                sources ride the tile ladder; Multiview program output is not
                interrupted.
              </Trans>
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button
              variant="outline"
              onClick={(): void => {
                setRebootOpen(false);
              }}
            >
              <Trans>Cancel</Trans>
            </Button>
            <Button
              variant="destructive"
              onClick={(): void => {
                rebootDevice(deviceId)
                  .then((accepted): void => {
                    toast({
                      title: t`Reboot accepted`,
                      description: t`Operation ${accepted.operation_id}; the outcome arrives on the realtime stream.`,
                    });
                  })
                  .catch(actionToast(t`Could not reboot`));
                setRebootOpen(false);
              }}
            >
              <Trans>Reboot</Trans>
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      {/* Set-mode: the DEV-class impact is declared BEFORE apply (ADR-M009). */}
      <Dialog open={modeOpen} onOpenChange={setModeOpen}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>
              <Trans>Change device mode</Trans>
            </DialogTitle>
            <DialogDescription>
              <Trans>
                Changing the mode restarts the device's own pipeline: its
                bound sources ride the tile ladder until it returns. No
                Multiview program output is interrupted.
              </Trans>
            </DialogDescription>
          </DialogHeader>
          <div className="flex flex-col gap-2 text-sm">
            <label className="flex items-center gap-2" htmlFor="device-mode-choice">
              <Trans>Target mode</Trans>
              <select
                id="device-mode-choice"
                className="rounded-md border bg-background px-2 py-1"
                value={activeModeChoice}
                onChange={(event): void => {
                  setModeChoice(event.target.value);
                }}
              >
                {ZOWIETEK_MODES.map((mode) => (
                  <option key={mode} value={mode}>
                    {mode}
                  </option>
                ))}
              </select>
            </label>
            {currentMode !== undefined ? (
              <p className="text-xs text-muted-foreground">
                <Trans>Current mode: {currentMode}</Trans>
              </p>
            ) : null}
          </div>
          <DialogFooter>
            <Button
              variant="outline"
              onClick={(): void => {
                setModeOpen(false);
              }}
            >
              <Trans>Cancel</Trans>
            </Button>
            <Button
              aria-label={`${t`Apply mode`}: ${activeModeChoice}`}
              onClick={(): void => {
                setDeviceMode(deviceId, activeModeChoice)
                  .then((accepted): void => {
                    toast({
                      title: t`Mode change accepted`,
                      description: `${accepted.detail} (${accepted.operation_id})`,
                    });
                  })
                  .catch(actionToast(t`Could not change the mode`));
                setModeOpen(false);
              }}
            >
              <Trans>Apply mode</Trans>: {activeModeChoice}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      {/* Bind a candidate as an ordinary Source carrying device_ref. */}
      <Dialog
        open={bindSource !== null}
        onOpenChange={(open): void => {
          if (!open) {
            setBindSource(null);
          }
        }}
      >
        <DialogContent>
          <DialogHeader>
            <DialogTitle>
              <Trans>Bind device stream as a source</Trans>
            </DialogTitle>
            <DialogDescription>
              {bindSource?.candidate.unverified === true ? (
                <Trans>
                  The vendor does not document this mount, so the URL is
                  operator-supplied and unverified — enter the stream URL the
                  device serves.
                </Trans>
              ) : (
                <Trans>
                  Creates an ordinary managed source bound to this device via
                  device_ref.
                </Trans>
              )}
            </DialogDescription>
          </DialogHeader>
          {bindSource !== null ? (
            <form
              className="flex flex-col gap-4"
              noValidate
              onSubmit={(event): void => {
                event.preventDefault();
                submitBindSource();
              }}
            >
              <FormField
                id="bind-source-id"
                label={t`Identifier`}
                value={bindSource.form.id}
                required
                placeholder={t`e.g. src-foyer-main`}
                error={bindSourceErrors.id}
                onChange={(next): void => {
                  setBindSource({ ...bindSource, form: { ...bindSource.form, id: next } });
                }}
              />
              <FormField
                id="bind-source-name"
                label={t`Name`}
                value={bindSource.form.name}
                required
                error={bindSourceErrors.name}
                onChange={(next): void => {
                  setBindSource({
                    ...bindSource,
                    form: { ...bindSource.form, name: next },
                  });
                }}
              />
              {bindSource.form.kind === 'ndi' ? (
                <FormField
                  id="bind-source-ndi"
                  label={t`NDI source name`}
                  value={bindSource.form.ndiName}
                  required
                  error={bindSourceErrors.ndiName}
                  onChange={(next): void => {
                    setBindSource({
                      ...bindSource,
                      form: { ...bindSource.form, ndiName: next },
                    });
                  }}
                />
              ) : (
                <FormField
                  id="bind-source-url"
                  label={t`Source URL`}
                  value={bindSource.form.url}
                  required
                  placeholder="rtsp://[2001:db8::1]:8554/stream"
                  error={bindSourceErrors.url}
                  onChange={(next): void => {
                    setBindSource({
                      ...bindSource,
                      form: { ...bindSource.form, url: next },
                    });
                  }}
                />
              )}
              <DialogFooter>
                <Button
                  type="button"
                  variant="outline"
                  onClick={(): void => {
                    setBindSource(null);
                  }}
                >
                  <Trans>Cancel</Trans>
                </Button>
                <Button type="submit" disabled={saveSource.isPending}>
                  <Trans>Create</Trans>
                </Button>
              </DialogFooter>
            </form>
          ) : null}
        </DialogContent>
      </Dialog>

      {/* Bind a decode target as an ordinary Output carrying device_ref. */}
      <Dialog
        open={bindOutput !== null}
        onOpenChange={(open): void => {
          if (!open) {
            setBindOutput(null);
          }
        }}
      >
        <DialogContent>
          <DialogHeader>
            <DialogTitle>
              <Trans>Bind decode slot to an output</Trans>
            </DialogTitle>
            <DialogDescription>
              <Trans>
                Creates an ordinary managed output bound to this device via
                device_ref; the driver points the decode slot at it.
              </Trans>
            </DialogDescription>
          </DialogHeader>
          {bindOutput !== null ? (
            <form
              className="flex flex-col gap-4"
              noValidate
              onSubmit={(event): void => {
                event.preventDefault();
                submitBindOutput();
              }}
            >
              <FormField
                id="bind-output-id"
                label={t`Identifier`}
                value={bindOutput.form.id}
                required
                placeholder={t`e.g. out-foyer`}
                error={bindOutputErrors.id}
                onChange={(next): void => {
                  setBindOutput({ ...bindOutput, form: { ...bindOutput.form, id: next } });
                }}
              />
              <FormField
                id="bind-output-name"
                label={t`Name`}
                value={bindOutput.form.name}
                required
                error={bindOutputErrors.name}
                onChange={(next): void => {
                  setBindOutput({
                    ...bindOutput,
                    form: { ...bindOutput.form, name: next },
                  });
                }}
              />
              {bindOutput.form.kind === 'rtsp' ? (
                <FormField
                  id="bind-output-mount"
                  label={t`Mount point`}
                  value={bindOutput.form.mount}
                  required
                  placeholder="/multiview"
                  error={bindOutputErrors.mount}
                  onChange={(next): void => {
                    setBindOutput({
                      ...bindOutput,
                      form: { ...bindOutput.form, mount: next },
                    });
                  }}
                />
              ) : null}
              {bindOutput.form.kind === 'srt' || bindOutput.form.kind === 'rtmp' ? (
                <FormField
                  id="bind-output-url"
                  label={t`Destination URL`}
                  value={bindOutput.form.url}
                  required
                  error={bindOutputErrors.url}
                  onChange={(next): void => {
                    setBindOutput({
                      ...bindOutput,
                      form: { ...bindOutput.form, url: next },
                    });
                  }}
                />
              ) : null}
              {bindOutput.form.kind === 'ndi' ? (
                <FormField
                  id="bind-output-ndi"
                  label={t`Advertised NDI name`}
                  value={bindOutput.form.ndiName}
                  required
                  error={bindOutputErrors.ndiName}
                  onChange={(next): void => {
                    setBindOutput({
                      ...bindOutput,
                      form: { ...bindOutput.form, ndiName: next },
                    });
                  }}
                />
              ) : null}
              <DialogFooter>
                <Button
                  type="button"
                  variant="outline"
                  onClick={(): void => {
                    setBindOutput(null);
                  }}
                >
                  <Trans>Cancel</Trans>
                </Button>
                <Button type="submit" disabled={saveOutput.isPending}>
                  <Trans>Create</Trans>
                </Button>
              </DialogFooter>
            </form>
          ) : null}
        </DialogContent>
      </Dialog>
    </>
  );
}
