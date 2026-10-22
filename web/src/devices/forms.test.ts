// Pure unit tests for the managed-device + sync-group form mappings
// (managed-devices.md §7.3 config shapes; ADR-M008/M010).
//
// The device body is strict (`deny_unknown_fields` on `multiview_config::Device`),
// so the form writes ONLY schema fields and preserves unmanaged-but-known keys
// (reconnect / display) verbatim via `extra`. Sync-group members carry
// per-member offset_ms; cast devices are never offered as members (Tier D).
import { describe, expect, it } from "vitest";

import {
  deviceFormFromRecord,
  deviceFormToBody,
  driverRequiresAddress,
  emptyDeviceForm,
  emptySyncGroupForm,
  syncGroupFormFromRecord,
  syncGroupFormToBody,
  syncMemberDeviceOptions,
  validateDeviceForm,
  validateSyncGroupForm,
} from "./forms";
import type { DeviceView } from "./types";

describe("device form", () => {
  it("builds the exact config Device body from a full form", () => {
    const body = deviceFormToBody({
      ...emptyDeviceForm(),
      id: " dev-foyer ",
      name: "Foyer decoder",
      driver: "zowietek",
      address: "http://[fd00:db8::42]",
      desiredMode: "decoder",
      alarmOnOffline: "major",
      authSecretRef: "op://Site/foyer-decoder/credentials",
    });
    expect(body).toEqual({
      id: "dev-foyer",
      display_name: "Foyer decoder",
      driver: "zowietek",
      address: "http://[fd00:db8::42]",
      desired_mode: "decoder",
      alarm_on_offline: "major",
      auth: { secret_ref: "op://Site/foyer-decoder/credentials" },
    });
  });

  it("omits every optional block left empty (deny_unknown_fields safety)", () => {
    const body = deviceFormToBody({
      ...emptyDeviceForm(),
      id: "dev-node",
      name: "Node",
      driver: "displaynode",
      address: "",
      desiredMode: "",
      alarmOnOffline: "none",
      authSecretRef: "",
    });
    expect(body).toEqual({
      id: "dev-node",
      display_name: "Node",
      driver: "displaynode",
    });
  });

  it("preserves unmanaged body keys (reconnect / display) across an edit", () => {
    const record = {
      id: "dev-1",
      name: "Box",
      body: {
        id: "dev-1",
        driver: "zowietek",
        address: "http://[fd00::1]",
        reconnect: { initial_ms: 500, max_ms: 30000 },
      },
    };
    const form = deviceFormFromRecord(record);
    expect(form).toBeDefined();
    if (form === undefined) {
      throw new Error("expected a parsed form");
    }
    expect(form.address).toBe("http://[fd00::1]");
    const body = deviceFormToBody(form);
    expect(body.reconnect).toEqual({ initial_ms: 500, max_ms: 30000 });
  });

  it("refuses to edit an unknown driver (the document stays as authored)", () => {
    const form = deviceFormFromRecord({
      id: "dev-x",
      name: "X",
      body: { id: "dev-x", driver: "vendorzz" },
    });
    expect(form).toBeUndefined();
  });

  it("requires the management address for zowietek and cast but not displaynode", () => {
    expect(driverRequiresAddress("zowietek")).toBe(true);
    expect(driverRequiresAddress("cast")).toBe(true);
    expect(driverRequiresAddress("displaynode")).toBe(false);
    const errors = validateDeviceForm(
      { ...emptyDeviceForm(), id: "d", name: "n", driver: "zowietek", address: "" },
      true,
    );
    expect(errors.address).toBe("required");
    const nodeErrors = validateDeviceForm(
      { ...emptyDeviceForm(), id: "d", name: "n", driver: "displaynode", address: "" },
      true,
    );
    expect(nodeErrors.address).toBeUndefined();
  });

  it("rejects a non-http management address and a missing id on create", () => {
    const errors = validateDeviceForm(
      {
        ...emptyDeviceForm(),
        id: "",
        name: "n",
        driver: "zowietek",
        address: "ftp://box",
      },
      true,
    );
    expect(errors.id).toBe("required");
    expect(errors.address).toBe("scheme-http");
  });
});

describe("sync-group form", () => {
  it("builds the exact config SyncGroup body with integer skew and offsets", () => {
    const body = syncGroupFormToBody({
      ...emptySyncGroupForm(),
      id: "lobby-wall",
      name: "Lobby wall",
      targetSkewMs: "50",
      members: [
        { device: "dev-node-left", offsetMs: "0" },
        { device: "dev-foyer", offsetMs: "120" },
      ],
    });
    expect(body).toEqual({
      id: "lobby-wall",
      target_skew_ms: 50,
      members: [
        { device: "dev-node-left", offset_ms: 0 },
        { device: "dev-foyer", offset_ms: 120 },
      ],
    });
  });

  it("round-trips a stored group and preserves the authored mode via extra", () => {
    const form = syncGroupFormFromRecord({
      id: "g1",
      name: "G1",
      body: {
        id: "g1",
        mode: "auto",
        target_skew_ms: 80,
        members: [{ device: "dev-a", offset_ms: 10 }],
      },
    });
    expect(form).toBeDefined();
    if (form === undefined) {
      throw new Error("expected a parsed form");
    }
    expect(form.targetSkewMs).toBe("80");
    expect(form.members).toEqual([{ device: "dev-a", offsetMs: "10" }]);
    expect(syncGroupFormToBody(form).mode).toBe("auto");
  });

  it("requires at least one member", () => {
    const errors = validateSyncGroupForm(
      { ...emptySyncGroupForm(), id: "g", name: "G", targetSkewMs: "50", members: [] },
      true,
    );
    expect(errors.members).toBe("members-required");
  });

  it("bounds target_skew_ms to 1..=10000", () => {
    const zero = validateSyncGroupForm(
      {
        ...emptySyncGroupForm(),
        id: "g",
        name: "G",
        targetSkewMs: "0",
        members: [{ device: "dev-a", offsetMs: "0" }],
      },
      true,
    );
    expect(zero.targetSkewMs).toBe("int-range");
    const big = validateSyncGroupForm(
      {
        ...emptySyncGroupForm(),
        id: "g",
        name: "G",
        targetSkewMs: "10001",
        members: [{ device: "dev-a", offsetMs: "0" }],
      },
      true,
    );
    expect(big.targetSkewMs).toBe("int-range");
  });

  it("flags a duplicate member device and an out-of-range offset per row", () => {
    const errors = validateSyncGroupForm(
      {
        ...emptySyncGroupForm(),
        id: "g",
        name: "G",
        targetSkewMs: "50",
        members: [
          { device: "dev-a", offsetMs: "0" },
          { device: "dev-a", offsetMs: "20000" },
        ],
      },
      true,
    );
    expect(errors["member-1"]).toBeDefined();
  });

  it("never offers cast devices as sync-group members (Tier D)", () => {
    const devices: readonly DeviceView[] = [
      {
        id: "dev-a",
        name: "A",
        driver: "displaynode",
        rawDriver: "displaynode",
        address: undefined,
        desiredMode: undefined,
        editable: true,
      },
      {
        id: "dev-tv",
        name: "TV",
        driver: "cast",
        rawDriver: "cast",
        address: "http://192.0.2.7",
        desiredMode: undefined,
        editable: true,
      },
    ];
    expect(syncMemberDeviceOptions(devices)).toEqual(["dev-a"]);
  });
});
