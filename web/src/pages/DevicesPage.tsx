// Devices — managed-device adoption + fleet list (managed-devices.md §9,
// ADR-M008/M009).
//
// The list joins the stored desired state (the `{id,name,body}` records) with
// the runtime status lane: the conflated `device.status` WebSocket topic
// first, the `/devices/{id}/status` REST snapshot as fallback — and shows
// state as icon+text (never colour alone), mode, temperature, last-seen, and
// the sync-group chip. Discovery is an UNTRUSTED inventory (ADR-0041): the
// panel says so explicitly, a scan only streams hints, and every row's Adopt
// button merely PREFILLS the confirm-adopt dialog — nothing is ever adopted
// without the operator's explicit confirmation.
import { useCallback, useEffect, useRef, useState } from 'react';
import type { JSX } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import { useQuery, useQueryClient } from '@tanstack/react-query';
import type { ColumnDef } from '@tanstack/react-table';
import { Link, useSearchParams } from 'react-router-dom';
import { ArrowRight, Radar } from 'lucide-react';

import { scanDevices } from '../devices/api';
import type { DiscoveredServiceView } from '../devices/api';
import { DeviceStateBadge } from '../devices/DeviceStateBadge';
import {
  DISCOVERY_INVENTORY_QUERY_KEY,
  useDeviceStatuses,
  useDevices,
  useDiscoveredInventory,
  useEngineClockRef,
  useSyncGroups,
} from '../devices/queries';
import {
  DEVICE_ALARM_CHOICES,
  deviceFormFromRecord,
  deviceFormToBody,
  driverRequiresAddress,
  emptyDeviceForm,
  validateDeviceForm,
} from '../devices/forms';
import type {
  DeviceAlarmChoice,
  DeviceField,
  DeviceFormState,
} from '../devices/forms';
import type { FieldErrors } from '../resources/forms';
import { DEVICE_DRIVERS } from '../devices/types';
import type { DeviceDriver, DeviceView } from '../devices/types';
import { LastSeenCell } from '../devices/lastSeen';
import { getResource } from '../resources/api';
import type { SaveResourceVars } from '../resources/queries';
import { CrudPage, KindCell, NameCell, RowActions } from '../resources/CrudPage';
import type { CrudSeed } from '../resources/CrudPage';
import {
  ApplySemanticsCallout,
  ExportConfigButton,
  FormField,
  SelectField,
} from '../resources/FormControls';
import {
  DISCOVERED_LIVE_QUERY_KEY,
} from '../realtime/useEngineEvents';
import type { DeviceStatus, DeviceDiscovered } from '../realtime/generated-types';
import { HelpLink } from '../components/HelpLink';
import { Badge } from '../components/ui/badge';
import { Button } from '../components/ui/button';
import { Card, CardContent, CardHeader, CardTitle } from '../components/ui/card';
import { toast } from '../components/ui/use-toast';

/** The localized display label for a driver family. */
function driverLabel(driver: DeviceDriver): JSX.Element {
  switch (driver) {
    case 'zowietek':
      return <Trans>ZowieBox-class decoder/encoder (zowietek)</Trans>;
    case 'displaynode':
      return <Trans>Multiview display node (displaynode)</Trans>;
    case 'cast':
      return <Trans>Cast device (cast)</Trans>;
  }
}

/** The localized display label for an offline-alarm choice. */
function alarmLabel(choice: DeviceAlarmChoice): JSX.Element {
  switch (choice) {
    case 'none':
      return <Trans>No offline alarm</Trans>;
    case 'warning':
      return <Trans>Warning</Trans>;
    case 'minor':
      return <Trans>Minor</Trans>;
    case 'major':
      return <Trans>Major</Trans>;
    case 'critical':
      return <Trans>Critical</Trans>;
  }
}

/** The adopt/edit form fields (shared shape with the config Device schema). */
function DeviceFormFields({
  form,
  setForm,
  creating,
  errors,
}: {
  readonly form: DeviceFormState;
  readonly setForm: (next: DeviceFormState) => void;
  readonly creating: boolean;
  readonly errors: FieldErrors<DeviceField>;
}): JSX.Element {
  const { t } = useLingui();
  return (
    <>
      <FormField
        id="device-id"
        label={t`Identifier`}
        value={form.id}
        disabled={!creating}
        required={creating}
        placeholder={t`e.g. dev-foyer`}
        error={errors.id}
        onChange={(next): void => {
          setForm({ ...form, id: next });
        }}
      />
      <FormField
        id="device-name"
        label={t`Name`}
        value={form.name}
        required
        error={errors.name}
        onChange={(next): void => {
          setForm({ ...form, name: next });
        }}
      />
      <SelectField<DeviceDriver>
        label={t`Driver`}
        value={form.driver}
        options={DEVICE_DRIVERS}
        optionLabel={driverLabel}
        trailing={<HelpLink to="/help/devices" label={t`About managed devices`} compact />}
        onChange={(next): void => {
          setForm({ ...form, driver: next });
        }}
      />
      <FormField
        id="device-address"
        label={t`Management address`}
        value={form.address}
        required={driverRequiresAddress(form.driver)}
        placeholder="http://[2001:db8::42]"
        error={errors.address}
        hint={
          form.driver === 'displaynode' ? (
            <Trans>
              Optional for display nodes — an enrolled node is located by its
              keypair identity.
            </Trans>
          ) : (
            <Trans>HTTP(S) management endpoint. Wrap IPv6 literals in brackets.</Trans>
          )
        }
        onChange={(next): void => {
          setForm({ ...form, address: next });
        }}
      />
      <FormField
        id="device-mode"
        label={t`Desired mode (optional)`}
        value={form.desiredMode}
        placeholder={form.driver === 'zowietek' ? t`encoder or decoder` : ''}
        hint={
          <Trans>
            Driver vocabulary; the driver re-converges the device onto this
            mode whenever it comes online. Leave blank to keep the device's
            current mode.
          </Trans>
        }
        onChange={(next): void => {
          setForm({ ...form, desiredMode: next });
        }}
      />
      <SelectField<DeviceAlarmChoice>
        label={t`Alarm when offline`}
        value={form.alarmOnOffline}
        options={DEVICE_ALARM_CHOICES}
        optionLabel={alarmLabel}
        onChange={(next): void => {
          setForm({ ...form, alarmOnOffline: next });
        }}
      />
      <FormField
        id="device-auth"
        label={t`Credentials secret reference`}
        value={form.authSecretRef}
        placeholder="op://Site/foyer-decoder/credentials"
        hint={<Trans>A reference only — never a plaintext secret. Leave blank for none.</Trans>}
        onChange={(next): void => {
          setForm({ ...form, authSecretRef: next });
        }}
      />
    </>
  );
}

/** A discovery row's adoptable driver, or `undefined` when not adoptable. */
function adoptableDriver(driverKind: string): DeviceDriver | undefined {
  switch (driverKind) {
    case 'zowietek-control':
    case 'zowietek':
      return 'zowietek';
    case 'cast':
      return 'cast';
    case 'displaynode':
      return 'displaynode';
    default:
      return undefined;
  }
}

/** The localized address-family label (IPv6 leads; IPv4 is legacy). */
function familyLabel(family: string | undefined): JSX.Element {
  return family === 'ipv4-legacy' ? (
    <Trans>IPv4 (legacy)</Trans>
  ) : (
    <Trans>IPv6</Trans>
  );
}

/** A unified discovery row (REST snapshot or live corr-correlated event). */
interface DiscoveryRow {
  readonly key: string;
  readonly name: string;
  readonly driverKind: string;
  readonly address: string;
  readonly family: string | undefined;
}

function rowsFromInventory(rows: readonly DiscoveredServiceView[]): DiscoveryRow[] {
  return rows.map((row) => ({
    key: row.key,
    name: row.name,
    driverKind: row.driverKind,
    address: row.primaryAddress,
    family: row.endpoints.at(0)?.family,
  }));
}

/**
 * The discovery panel: scan + untrusted inventory + per-row confirm-adopt
 * prefill. Live rows stream in correlated to the running scan's operation id
 * (the envelope `corr`); the REST snapshot fills in between scans.
 */
function DiscoveryPanel({
  onAdopt,
}: {
  readonly onAdopt: (prefill: { driver: DeviceDriver; address: string; name: string }) => void;
}): JSX.Element {
  const { t } = useLingui();
  const queryClient = useQueryClient();
  const inventory = useDiscoveredInventory();
  const [scanOp, setScanOp] = useState<string | undefined>(undefined);
  const refreshTimer = useRef<ReturnType<typeof setTimeout> | null>(null);

  useEffect(
    () => (): void => {
      if (refreshTimer.current !== null) {
        clearTimeout(refreshTimer.current);
      }
    },
    [],
  );

  // Live rows for the running scan, written by the realtime hook under the
  // scan's operation id (corr). Passive read; empty when no stream.
  const liveByCorr = useQuery<Record<string, readonly DeviceDiscovered[]>>({
    queryKey: DISCOVERED_LIVE_QUERY_KEY,
    queryFn: (): Record<string, readonly DeviceDiscovered[]> => ({}),
    enabled: false,
    initialData: {},
  }).data;

  const rows: DiscoveryRow[] = rowsFromInventory(inventory.data ?? []);
  const liveRows = scanOp === undefined ? [] : (liveByCorr[scanOp] ?? []);
  for (const live of liveRows) {
    if (!rows.some((row) => row.address === live.address)) {
      rows.push({
        key: `live:${live.address}`,
        name: live.name ?? live.address,
        driverKind: live.driver,
        address: live.address,
        family: live.family,
      });
    }
  }

  const scanNow = (): void => {
    scanDevices()
      .then((accepted): void => {
        setScanOp(accepted.operation_id);
        toast({
          title: t`Scan running`,
          description: t`Results stream in below as they are found. ${accepted.note}`,
        });
        // Re-read the inventory snapshot once the time-bounded browse ends.
        if (refreshTimer.current !== null) {
          clearTimeout(refreshTimer.current);
        }
        refreshTimer.current = setTimeout((): void => {
          void queryClient.invalidateQueries({
            queryKey: DISCOVERY_INVENTORY_QUERY_KEY,
          });
        }, accepted.budget_ms);
      })
      .catch((error: unknown): void => {
        toast({
          title: t`Could not start the scan`,
          description: error instanceof Error ? error.message : String(error),
          variant: 'destructive',
        });
      });
  };

  return (
    <Card className="mb-4" data-testid="discovery-panel">
      <CardHeader>
        <CardTitle className="flex flex-wrap items-center justify-between gap-2 text-base">
          <span className="inline-flex items-center gap-2">
            <Radar className="size-4" aria-hidden="true" />
            <Trans>Discovery</Trans>
          </span>
          <Button variant="outline" size="sm" onClick={scanNow}>
            <Trans>Scan for devices</Trans>
          </Button>
        </CardTitle>
      </CardHeader>
      <CardContent className="space-y-3 text-sm">
        <p className="text-muted-foreground">
          <Trans>
            Discovery results are an untrusted inventory of mDNS hints — they
            are not devices, and a scan never adopts anything. Adopting always
            requires your explicit confirmation: the Adopt button only prefills
            the dialog.
          </Trans>{' '}
          <HelpLink to="/help/devices/adopt" label={t`About adopting devices`} />
        </p>
        {rows.length === 0 ? (
          <p className="text-muted-foreground">
            <Trans>No services discovered yet. Run a scan to browse the network.</Trans>
          </p>
        ) : (
          <ul className="flex flex-col gap-2">
            {rows.map((row) => {
              const driver = adoptableDriver(row.driverKind);
              return (
                <li
                  key={row.key}
                  className="flex flex-wrap items-center justify-between gap-2 rounded-md border p-2"
                >
                  <span className="inline-flex flex-wrap items-center gap-2">
                    <span lang="" dir="auto" className="font-medium">
                      {row.name}
                    </span>
                    <Badge variant="outline">{row.driverKind}</Badge>
                    <code className="text-xs text-muted-foreground" dir="ltr">
                      {row.address}
                    </code>
                    <Badge variant="outline">{familyLabel(row.family)}</Badge>
                  </span>
                  {driver !== undefined ? (
                    <Button
                      size="sm"
                      variant="outline"
                      aria-label={`${t`Adopt`}: ${row.name}`}
                      onClick={(): void => {
                        onAdopt({
                          driver,
                          address: `http://${row.address}`,
                          name: row.name,
                        });
                      }}
                    >
                      <ArrowRight aria-hidden="true" />
                      <Trans>Adopt…</Trans>
                    </Button>
                  ) : (
                    <span className="text-xs text-muted-foreground">
                      {row.driverKind === 'ndi-source' ? (
                        <Trans>An NDI service is added as an NDI source, not adopted.</Trans>
                      ) : (
                        <Trans>No compiled-in driver recognises this service.</Trans>
                      )}
                    </span>
                  )}
                </li>
              );
            })}
          </ul>
        )}
      </CardContent>
    </Card>
  );
}

/**
 * An accessible empty-cell placeholder: the em-dash is decorative (hidden from
 * AT) and the meaning is carried by sr-only text — the State-cell pattern,
 * applied to every placeholder cell.
 */
function PlaceholderCell({ label }: { readonly label: JSX.Element }): JSX.Element {
  return (
    <span className="text-sm text-muted-foreground">
      <span aria-hidden="true">—</span>
      <span className="sr-only">{label}</span>
    </span>
  );
}

/** The sync-group chip for a device, when it is a member of one. */
function SyncGroupChip({
  deviceId,
  groups,
}: {
  readonly deviceId: string;
  readonly groups: readonly { id: string; name: string; members: readonly { device: string }[] }[];
}): JSX.Element {
  const member = groups.find((group) =>
    group.members.some((m) => m.device === deviceId),
  );
  if (member === undefined) {
    return <PlaceholderCell label={<Trans>Not in a sync group.</Trans>} />;
  }
  return (
    <Link to="/sync-groups" className="inline-flex">
      <Badge variant="outline">{member.name}</Badge>
    </Link>
  );
}

/**
 * The list's "current stream link" cell (managed-devices.md §9): the FIRST
 * device-reported stream's role + health, deep-linking to the detail page's
 * Streams tab where every stream (and binding) lives.
 */
function StreamCell({
  deviceId,
  status,
}: {
  readonly deviceId: string;
  readonly status: DeviceStatus | undefined;
}): JSX.Element {
  const { t } = useLingui();
  const first = (status?.streams ?? []).at(0);
  if (first === undefined) {
    return <PlaceholderCell label={<Trans>No active stream.</Trans>} />;
  }
  // Explicit accessible name: the link's content is two adjacent inline badges
  // ("encode" + "healthy") with only a visual gap, so the computed accessible
  // name concatenates to "encodehealthy" (no separator). Name it directly so a
  // screen reader announces "encode healthy" as two words and the name is
  // deterministic regardless of inline text-concatenation rules.
  const health = first.healthy ? t`healthy` : t`unhealthy`;
  return (
    <Link
      to={`/devices/${encodeURIComponent(deviceId)}?tab=streams`}
      aria-label={`${first.role} ${health}`}
      className="inline-flex items-center gap-1.5 underline-offset-2 hover:underline"
    >
      <Badge variant="outline">{first.role}</Badge>
      {first.healthy ? (
        <Badge variant="live">
          <span>
            <Trans>healthy</Trans>
          </span>
        </Badge>
      ) : (
        <Badge variant="stale">
          <span>
            <Trans>unhealthy</Trans>
          </span>
        </Badge>
      )}
    </Link>
  );
}

/** Devices management (adoption + fleet). */
export function DevicesPage(): JSX.Element {
  const { t } = useLingui();
  const devices = useDevices();
  const groups = useSyncGroups();
  const deviceIds = (devices.data ?? []).map((device) => device.id);
  const statuses = useDeviceStatuses(deviceIds);
  const clock = useEngineClockRef();
  const [seed, setSeed] = useState<CrudSeed<DeviceFormState> | undefined>(undefined);
  const seedCounter = useRef(0);
  const [searchParams, setSearchParams] = useSearchParams();

  const openSeed = useCallback((creating: boolean, form: DeviceFormState): void => {
    seedCounter.current += 1;
    setSeed({ key: seedCounter.current, creating, form });
  }, []);

  // `/devices?edit=<id>` (e.g. the detail page's "Update credentials") opens
  // the edit dialog for that device once.
  const editParam = searchParams.get('edit');
  useEffect(() => {
    if (editParam === null) {
      return;
    }
    setSearchParams({}, { replace: true });
    getResource('devices', editParam)
      .then((result): void => {
        const form = deviceFormFromRecord(result.record);
        if (form !== undefined) {
          openSeed(false, form);
        }
      })
      .catch((error: unknown): void => {
        toast({
          title: t`Could not load for editing`,
          description: error instanceof Error ? error.message : String(error),
          variant: 'destructive',
        });
      });
  }, [editParam, openSeed, setSearchParams, t]);

  const statusFor = (id: string): DeviceStatus | undefined => statuses[id];

  const columns = (
    onEdit: (row: DeviceView) => void,
    onDelete: (row: DeviceView) => void,
  ): ColumnDef<DeviceView>[] => [
    {
      accessorKey: 'name',
      header: t`Name`,
      cell: (ctx): JSX.Element => (
        <Link
          to={`/devices/${encodeURIComponent(ctx.row.original.id)}`}
          className="underline-offset-2 hover:underline"
        >
          <NameCell value={ctx.row.original.name} />
        </Link>
      ),
    },
    {
      accessorKey: 'driver',
      header: t`Driver`,
      cell: (ctx): JSX.Element => <KindCell value={ctx.row.original.rawDriver} />,
    },
    {
      id: 'state',
      header: (): JSX.Element => (
        <span className="inline-flex items-center gap-1.5">
          <Trans>State</Trans>
          <HelpLink to="/help/devices" label={t`About device states`} compact />
        </span>
      ),
      cell: (ctx): JSX.Element => {
        const status = statusFor(ctx.row.original.id);
        if (status === undefined) {
          return (
            <span
              className="text-sm text-muted-foreground"
              title={t`No runtime status yet — the device may not be adopted into the running control plane.`}
            >
              <span aria-hidden="true">—</span>
              <span className="sr-only">
                <Trans>No runtime status yet.</Trans>
              </span>
            </span>
          );
        }
        return <DeviceStateBadge state={status.state} />;
      },
    },
    {
      id: 'mode',
      header: t`Mode`,
      cell: (ctx): JSX.Element => {
        const status = statusFor(ctx.row.original.id);
        const mode = status?.mode ?? undefined;
        if (mode !== undefined) {
          return <code className="text-xs">{mode}</code>;
        }
        const desired = ctx.row.original.desiredMode;
        return desired !== undefined ? (
          <span className="text-xs text-muted-foreground">
            <Trans>{desired} (desired)</Trans>
          </span>
        ) : (
          <PlaceholderCell label={<Trans>No mode reported.</Trans>} />
        );
      },
    },
    {
      id: 'temperature',
      header: t`Temperature`,
      cell: (ctx): JSX.Element => {
        const temperature = statusFor(ctx.row.original.id)?.temperature_c;
        return temperature !== undefined ? (
          <span className="text-xs">{`${String(temperature)} °C`}</span>
        ) : (
          <PlaceholderCell label={<Trans>No temperature reported.</Trans>} />
        );
      },
    },
    {
      id: 'stream',
      header: t`Stream`,
      cell: (ctx): JSX.Element => (
        <StreamCell
          deviceId={ctx.row.original.id}
          status={statusFor(ctx.row.original.id)}
        />
      ),
    },
    {
      id: 'last-seen',
      header: t`Last seen`,
      cell: (ctx): JSX.Element => (
        <LastSeenCell
          lastSeenTs={statusFor(ctx.row.original.id)?.last_seen_ts ?? undefined}
          clock={clock}
        />
      ),
    },
    {
      id: 'sync',
      header: t`Sync group`,
      cell: (ctx): JSX.Element => (
        <SyncGroupChip deviceId={ctx.row.original.id} groups={groups.data ?? []} />
      ),
    },
    {
      id: 'actions',
      header: t`Actions`,
      cell: (ctx): JSX.Element => (
        <RowActions
          row={ctx.row.original}
          name={ctx.row.original.name}
          editLabel={t`Edit device`}
          deleteLabel={t`Remove device`}
          editDisabledReason={
            ctx.row.original.editable
              ? undefined
              : t`driver "${ctx.row.original.rawDriver}" isn't editable in this UI; the document is preserved as authored`
          }
          onEdit={onEdit}
          onDelete={onDelete}
        />
      ),
    },
  ];

  return (
    <CrudPage<DeviceView, DeviceFormState, DeviceField>
      kind="devices"
      title={<Trans>Devices</Trans>}
      description={
        <span className="inline-flex flex-wrap items-center gap-x-3 gap-y-1">
          <Trans>
            Adopt and manage hardware around the multiview — decoders,
            display nodes, and cast targets.
          </Trans>
          <HelpLink to="/help/devices" label={t`About managed devices`} />
        </span>
      }
      newLabel={t`Adopt device`}
      dialogCreateTitle={t`Adopt device`}
      dialogEditTitle={t`Edit device`}
      dialogDescription={t`A managed device is declarative desired state: adopting starts a supervised driver that probes and converges it.`}
      caption={t`Managed devices.`}
      emptyMessage={<Trans>No devices adopted.</Trans>}
      loadingMessage={<Trans>Loading devices…</Trans>}
      errorPrefix={<Trans>Could not load devices:</Trans>}
      headerExtras={
        <>
          <Button asChild variant="outline">
            <Link to="/sync-groups">
              <Trans>Sync groups</Trans>
            </Link>
          </Button>
          <ExportConfigButton compact />
        </>
      }
      callout={
        <>
          <DiscoveryPanel
            onAdopt={(prefill): void => {
              openSeed(true, {
                ...emptyDeviceForm(),
                driver: prefill.driver,
                address: prefill.address,
                name: prefill.name,
              });
            }}
          />
          <ApplySemanticsCallout
            helpTo="/help/devices"
            helpLabel={t`How device changes apply`}
            message={
              <Trans>
                Adopting applies immediately: the control plane seeds the
                device in ADOPTING and starts its supervised driver. Program
                output never depends on any device (it rides tile
                resilience/failover instead), and config export captures
                devices for restarts.
              </Trans>
            }
          />
        </>
      }
      savedDescription={t`Stored and applied: the device's supervised driver converges it now. Config export captures it for restarts.`}
      deletedDescription={t`Removed from the registry. A device still referenced by a Source or Output (device_ref) is refused with a conflict instead.`}
      list={devices.data ?? []}
      isPending={devices.isPending}
      isError={devices.isError}
      errorMessage={devices.error?.message}
      columns={columns}
      rowId={(row): string => row.id}
      rowName={(row): string => row.name}
      emptyForm={emptyDeviceForm}
      formFromRecord={deviceFormFromRecord}
      validate={validateDeviceForm}
      toSaveVars={(form, creating): SaveResourceVars => ({
        id: creating ? form.id.trim() : form.id,
        create: creating,
        input: { name: form.name.trim(), body: deviceFormToBody(form) },
      })}
      seed={seed}
      renderFields={(form, setForm, creating, errors): JSX.Element => (
        <DeviceFormFields
          form={form}
          setForm={setForm}
          creating={creating}
          errors={errors}
        />
      )}
    />
  );
}
