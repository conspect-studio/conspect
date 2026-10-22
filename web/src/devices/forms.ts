// Pure form-state <-> config-body mapping for the managed-device and
// sync-group forms (managed-devices.md §7.3; ADR-M008/M010).
//
// Framework-free (no React, no Lingui) so the mapping and validation unit-test
// in isolation, exactly like ../resources/forms. The bodies produced here
// mirror the Rust config schema EXACTLY:
//   * `Device` (crates/multiview-config/src/device.rs) — `deny_unknown_fields`,
//     so the form writes ONLY schema fields and preserves the
//     unmanaged-but-known blocks (`reconnect`, `display`) verbatim via `extra`.
//   * `SyncGroup` / `SyncMember` (sync_group.rs) — `target_skew_ms` in
//     `1..=10_000`, member offsets in `0..=10_000`, at least one member, no
//     duplicate members; the authored `mode` rides `extra`.
// The control plane validates POST/PUT bodies against those types with 422
// (ADR-W015), so a wrong body is a rejected body. DO NOT add fields that do
// not exist in the Rust schema.
import { extraOf, parseIntStrict, urlErrorCode } from '../resources/forms';
import type { FieldErrors } from '../resources/forms';
import { stringField } from '../resources/api';
import type { ResourceRecord } from '../resources/types';
import { DEVICE_DRIVERS } from './types';
import type { DeviceDriver, DeviceView } from './types';

// --- device form -------------------------------------------------------------

/**
 * The offline-alarm severity choices: the lowercase device tokens the schema
 * accepts, plus `'none'` (= omit the field; no offline alarm). `cleared` and
 * `indeterminate` are rejected by the schema — an authored offline alarm must
 * be definite — so they are never offered.
 */
export type DeviceAlarmChoice = 'none' | 'warning' | 'minor' | 'major' | 'critical';

/** All offline-alarm choices, for building selectors. */
export const DEVICE_ALARM_CHOICES: readonly DeviceAlarmChoice[] = [
  'none',
  'warning',
  'minor',
  'major',
  'critical',
];

/** The editable state behind the device (adopt/edit) form. */
export interface DeviceFormState {
  readonly id: string;
  readonly name: string;
  readonly driver: DeviceDriver;
  /** Management address ('' = omit; required for zowietek/cast). */
  readonly address: string;
  /** Desired converged work mode ('' = omit; driver vocabulary). */
  readonly desiredMode: string;
  /** Offline-alarm severity ('none' = omit the field). */
  readonly alarmOnOffline: DeviceAlarmChoice;
  /** `auth.secret_ref` ('' = no `auth` block; never a plaintext secret). */
  readonly authSecretRef: string;
  /** Unmanaged body fields (`reconnect`, `display`) preserved verbatim. */
  readonly extra: Readonly<Record<string, unknown>>;
}

/** The device-form fields that can carry a validation error. */
export type DeviceField = 'id' | 'name' | 'address' | 'desiredMode' | 'authSecretRef';

/** A fresh, empty device form. */
export function emptyDeviceForm(): DeviceFormState {
  return {
    id: '',
    name: '',
    driver: 'zowietek',
    address: '',
    desiredMode: '',
    alarmOnOffline: 'none',
    authSecretRef: '',
    extra: {},
  };
}

/** The body keys the device form manages (everything else is preserved). */
const DEVICE_MANAGED_KEYS: readonly string[] = [
  'id',
  'display_name',
  'driver',
  'address',
  'desired_mode',
  'alarm_on_offline',
  'auth',
];

/**
 * Whether a driver requires a management address: `zowietek`/`cast` are
 * reached by address; a `displaynode` is located by its enrolled keypair
 * identity instead (device.rs).
 */
export function driverRequiresAddress(driver: DeviceDriver): boolean {
  return driver !== 'displaynode';
}

/** Parse a stored driver tag onto the editable form driver, or `undefined`. */
export function parseDeviceDriver(tag: string | undefined): DeviceDriver | undefined {
  return DEVICE_DRIVERS.find((driver) => driver === tag);
}

/** Build the exact config `Device` body from a valid form. */
export function deviceFormToBody(form: DeviceFormState): Record<string, unknown> {
  const body: Record<string, unknown> = { ...form.extra };
  body.id = form.id.trim();
  const displayName = form.name.trim();
  if (displayName !== '') {
    body.display_name = displayName;
  }
  body.driver = form.driver;
  const address = form.address.trim();
  if (address !== '') {
    body.address = address;
  }
  const desiredMode = form.desiredMode.trim();
  if (desiredMode !== '') {
    body.desired_mode = desiredMode;
  }
  if (form.alarmOnOffline !== 'none') {
    body.alarm_on_offline = form.alarmOnOffline;
  }
  const secretRef = form.authSecretRef.trim();
  if (secretRef !== '') {
    body.auth = { secret_ref: secretRef };
  }
  return body;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null;
}

/**
 * Project a stored record back onto the editable device form, or `undefined`
 * when the body's driver is not one this UI can edit (the page refuses Edit;
 * the document stays as authored).
 */
export function deviceFormFromRecord(record: ResourceRecord): DeviceFormState | undefined {
  const body = record.body;
  const driver = parseDeviceDriver(stringField(body, 'driver'));
  if (driver === undefined) {
    return undefined;
  }
  const auth = body.auth;
  const alarm = stringField(body, 'alarm_on_offline');
  return {
    id: record.id,
    // An authored display_name wins over the store name (the form writes its
    // Name back as display_name; see sourceFormFromRecord for the rationale).
    name: stringField(body, 'display_name') ?? record.name,
    driver,
    address: stringField(body, 'address') ?? '',
    desiredMode: stringField(body, 'desired_mode') ?? '',
    alarmOnOffline:
      DEVICE_ALARM_CHOICES.find((choice) => choice !== 'none' && choice === alarm) ?? 'none',
    authSecretRef: isRecord(auth) ? (stringField(auth, 'secret_ref') ?? '') : '',
    extra: extraOf(body, DEVICE_MANAGED_KEYS),
  };
}

/** Validate a device form, returning per-field machine codes. */
export function validateDeviceForm(
  form: DeviceFormState,
  creating: boolean,
): FieldErrors<DeviceField> {
  const errors: FieldErrors<DeviceField> = {};
  if (creating && form.id.trim() === '') {
    errors.id = 'required';
  }
  if (form.name.trim() === '') {
    errors.name = 'required';
  }
  const address = form.address.trim();
  if (address === '') {
    if (driverRequiresAddress(form.driver)) {
      errors.address = 'required';
    }
  } else {
    // The management channel is HTTP(S), IPv6-first (bracket IPv6 literals).
    const code = urlErrorCode(address, ['http', 'https'], 'scheme-http');
    if (code !== undefined) {
      errors.address = code;
    }
  }
  return errors;
}

// --- sync-group form -----------------------------------------------------------

/** One editable member row (numbers kept as input strings). */
export interface SyncMemberFormRow {
  /** The member device id. */
  readonly device: string;
  /** Per-member offset trim in ms (`0..=10_000`). */
  readonly offsetMs: string;
}

/** The editable state behind the sync-group form. */
export interface SyncGroupFormState {
  readonly id: string;
  readonly name: string;
  /** Drift-alarm threshold in ms (`1..=10_000`). */
  readonly targetSkewMs: string;
  readonly members: readonly SyncMemberFormRow[];
  /** Unmanaged body fields (`mode`) preserved verbatim. */
  readonly extra: Readonly<Record<string, unknown>>;
}

/**
 * The sync-group fields that can carry a validation error. Member rows carry
 * theirs under `member-<index>`.
 */
export type SyncGroupField =
  | 'id'
  | 'name'
  | 'targetSkewMs'
  | 'members'
  | `member-${string}`;

/** A fresh, empty sync-group form. */
export function emptySyncGroupForm(): SyncGroupFormState {
  return {
    id: '',
    name: '',
    targetSkewMs: '100',
    members: [],
    extra: {},
  };
}

/** The body keys the sync-group form manages (everything else is preserved). */
const SYNC_GROUP_MANAGED_KEYS: readonly string[] = ['id', 'target_skew_ms', 'members'];

/** Build the exact config `SyncGroup` body from a valid form. */
export function syncGroupFormToBody(form: SyncGroupFormState): Record<string, unknown> {
  const body: Record<string, unknown> = { ...form.extra };
  body.id = form.id.trim();
  body.target_skew_ms = parseIntStrict(form.targetSkewMs) ?? 0;
  body.members = form.members.map((member) => ({
    device: member.device.trim(),
    offset_ms: parseIntStrict(member.offsetMs) ?? 0,
  }));
  return body;
}

/**
 * Project a stored record back onto the editable sync-group form, or
 * `undefined` when the body shape is not one this UI can round-trip.
 */
export function syncGroupFormFromRecord(
  record: ResourceRecord,
): SyncGroupFormState | undefined {
  const body = record.body;
  const skew = body.target_skew_ms;
  const rawMembers = body.members;
  if (typeof skew !== 'number' || !Number.isFinite(skew) || !Array.isArray(rawMembers)) {
    return undefined;
  }
  const members: SyncMemberFormRow[] = [];
  for (const raw of rawMembers) {
    if (!isRecord(raw) || typeof raw.device !== 'string') {
      return undefined;
    }
    const offset = raw.offset_ms;
    members.push({
      device: raw.device,
      offsetMs:
        typeof offset === 'number' && Number.isFinite(offset) ? String(offset) : '0',
    });
  }
  return {
    id: record.id,
    name: record.name,
    targetSkewMs: String(skew),
    members,
    extra: extraOf(body, SYNC_GROUP_MANAGED_KEYS),
  };
}

/** `target_skew_ms` bounds (sync_group.rs: `1..=10_000`). */
const SKEW_MIN_MS = 1;
const SKEW_MAX_MS = 10_000;
/** Member `offset_ms` bounds (sync_group.rs: `0..=10_000`). */
const OFFSET_MIN_MS = 0;
const OFFSET_MAX_MS = 10_000;

/** Validate a sync-group form, returning per-field machine codes. */
export function validateSyncGroupForm(
  form: SyncGroupFormState,
  creating: boolean,
): FieldErrors<SyncGroupField> {
  const errors: FieldErrors<SyncGroupField> = {};
  if (creating && form.id.trim() === '') {
    errors.id = 'required';
  }
  if (form.name.trim() === '') {
    errors.name = 'required';
  }
  const skew = parseIntStrict(form.targetSkewMs);
  if (skew === undefined || skew < SKEW_MIN_MS || skew > SKEW_MAX_MS) {
    errors.targetSkewMs = 'int-range';
  }
  if (form.members.length === 0) {
    errors.members = 'members-required';
  }
  const seen = new Set<string>();
  form.members.forEach((member, index) => {
    const device = member.device.trim();
    if (device === '') {
      errors[`member-${String(index)}`] = 'required';
      return;
    }
    if (seen.has(device)) {
      errors[`member-${String(index)}`] = 'duplicate-member';
      return;
    }
    seen.add(device);
    const offset = parseIntStrict(member.offsetMs);
    if (offset === undefined || offset < OFFSET_MIN_MS || offset > OFFSET_MAX_MS) {
      errors[`member-${String(index)}`] = 'int-range';
    }
  });
  return errors;
}

/**
 * The device ids offerable as sync-group members: cast devices are Tier D —
 * never part of a synchronized canvas (managed-devices.md §8) — so they are
 * never offered. Unknown-driver devices stay eligible (membership only ever
 * weakens the claimed tier; it is never over-claimed).
 */
export function syncMemberDeviceOptions(devices: readonly DeviceView[]): string[] {
  return devices.filter((device) => device.driver !== 'cast').map((device) => device.id);
}
