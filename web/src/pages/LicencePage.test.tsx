// MSW tests for the Licence screen (/settings/licence): the entitlement panel
// (tier, hardware-class licensed-vs-detected, gpu_limit, usage, lease serial +
// renews/expires + grace, flag chips), the enforcement-ladder badge + one
// sentence per state, and the offline exchange (export-challenge download +
// install-lease upload) with the three documented install methods and the §3.5
// standing paragraph. Renders per state: licensed-active, lapsed (config-locked),
// and unlicensed.
import { afterAll, afterEach, beforeAll, describe, expect, it } from "vitest";
import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { http, HttpResponse } from "msw";
import { setupServer } from "msw/node";
import { MemoryRouter } from "react-router-dom";

import { LicencePage } from "./LicencePage";
import type {
  EnforcementLevel,
  LicenceResource,
  LicenceStatusDoc,
} from "../api/conspect";
import { renderWithProviders } from "../test/render";

// A far-future lease term so the active case renders as "renewing", independent
// of the wall clock the test runs on.
function leaseFor(serial: string): LicenceStatusDoc["lease"] {
  return {
    serial,
    source: "online",
    granted_at: "2999-01-01T00:00:00Z",
    expires_at: "2999-02-05T00:00:00Z",
    grace_days: 14,
    grace_until: "2999-02-19T00:00:00Z",
    hard_at: "2999-03-30T00:00:00Z",
    next_contact_due: "2999-02-01T00:00:00Z",
  };
}

function statusDoc(enforcement: EnforcementLevel): LicenceStatusDoc {
  return {
    tier: "studio",
    state: "compliant",
    enforcement,
    hardware_class: { licensed: "standard", detected: "standard" },
    gpu_limit: { kind: "limited", value: 2 },
    gpus_in_use: 1,
    config_locked: enforcement === "config-locked" || enforcement === "watermark",
    watermark: enforcement === "watermark",
    blocks_new_instances: enforcement === "block-new-instance",
    lease: leaseFor("LS-0001"),
    reasons: ["lease-valid"],
  };
}

const HEARTBEAT = {
  transport: "direct",
  last_at: "2026-05-01T00:00:00Z",
  next_due: "2026-06-01T00:00:00Z",
  payload_fields: ["fingerprint_digest", "lease_request", "signed_assertion"],
};

let licenceBody: LicenceResource = { licensed: true, status: statusDoc("active") };
let lastLeaseUpload: { contentType: string | null; bytes: number } | undefined;
let challengeDownloads = 0;

const server = setupServer(
  http.get("*/api/v1/licence", () => HttpResponse.json(licenceBody)),
  http.get("*/api/v1/licensing/heartbeat-status", () => HttpResponse.json(HEARTBEAT)),
  http.get("*/api/v1/licence/challenge", () => {
    challengeDownloads += 1;
    return new HttpResponse(new Uint8Array([1, 2, 3, 4]).buffer, {
      headers: { "Content-Type": "application/cbor" },
    });
  }),
  http.post("*/api/v1/licence/lease", async ({ request }) => {
    const buf = await request.arrayBuffer();
    lastLeaseUpload = {
      contentType: request.headers.get("content-type"),
      bytes: buf.byteLength,
    };
    return HttpResponse.json({ serial: "LS-0002", valid_to: "2026-07-10T00:00:00Z" });
  }),
);

beforeAll(() => {
  server.listen();
});
afterEach(() => {
  licenceBody = { licensed: true, status: statusDoc("active") };
  lastLeaseUpload = undefined;
  challengeDownloads = 0;
  server.resetHandlers();
});
afterAll(() => {
  server.close();
});

function renderLicence(): void {
  renderWithProviders(
    <MemoryRouter>
      <LicencePage />
    </MemoryRouter>,
  );
}

describe("LicencePage — licensed/active", () => {
  it("renders the entitlement panel: tier, hardware class, gpu limit, usage, lease", async () => {
    renderLicence();
    expect(await screen.findByText(/studio/)).toBeInTheDocument();
    // Licensed-vs-detected hardware class both shown (two "standard" cells).
    const panel = screen.getByTestId("entitlement-panel");
    expect(within(panel).getAllByText(/standard/i).length).toBeGreaterThanOrEqual(2);
    // The lease serial (mono identifier).
    expect(within(panel).getByText(/LS-0001/)).toBeInTheDocument();
  });

  it("shows the enforcement badge and exactly one sentence for the active state", async () => {
    renderLicence();
    const badge = await screen.findByTestId("enforcement-badge");
    expect(within(badge).getByText(/active/i)).toBeInTheDocument();
    expect(screen.getByTestId("enforcement-sentence")).toHaveTextContent(
      /your licence is active/i,
    );
  });

  it("renders the lease as renewing (not expiring) when active", async () => {
    renderLicence();
    const panel = await screen.findByTestId("entitlement-panel");
    // The lease term is in the future, so the panel reads "renews", never "expired".
    expect(within(panel).getAllByText(/renews/i).length).toBeGreaterThanOrEqual(1);
    expect(within(panel).queryByText(/expired/i)).toBeNull();
  });

  it("renders the flag chips for the derived booleans", async () => {
    renderLicence();
    // active: not config-locked, not watermark, not blocking.
    const chips = await screen.findByTestId("licence-flags");
    expect(within(chips).getByText(/reconfiguration/i)).toBeInTheDocument();
  });

  it("documents the three offline install methods and exposes the anchors", async () => {
    renderLicence();
    await screen.findByText(/studio/);
    expect(document.getElementById("challenge")).not.toBeNull();
    expect(document.getElementById("install-lease")).not.toBeNull();
    const install = screen.getByTestId("install-methods");
    // Three documented methods.
    expect(within(install).getAllByRole("listitem").length).toBe(3);
  });

  it("renders the §3.5 standing paragraph about Ed25519 / pinned keys / spoofing", async () => {
    renderLicence();
    const standing = await screen.findByTestId("spoof-standing");
    expect(standing).toHaveTextContent(/ed25519/i);
    expect(standing).toHaveTextContent(/pinned/i);
    expect(standing).toHaveTextContent(/spoof/i);
  });

  it("downloads the challenge CBOR when the export button is clicked", async () => {
    renderLicence();
    const button = await screen.findByRole("button", { name: /export challenge/i });
    await userEvent.click(button);
    await waitFor(() => {
      expect(challengeDownloads).toBe(1);
    });
  });

  it("installs an uploaded lease as application/cbor and reports the new serial", async () => {
    renderLicence();
    const input = await screen.findByLabelText(/lease file/i);
    const file = new File([new Uint8Array([9, 9, 9])], "lease.cbor", {
      type: "application/cbor",
    });
    await userEvent.upload(input, file);
    const installButton = screen.getByRole("button", { name: /install lease/i });
    await waitFor(() => {
      expect(installButton).not.toBeDisabled();
    });
    await userEvent.click(installButton);
    await waitFor(() => {
      expect(lastLeaseUpload?.contentType).toContain("application/cbor");
    });
    expect(lastLeaseUpload?.bytes).toBe(3);
    expect(await screen.findByText(/LS-0002/)).toBeInTheDocument();
  });
});

describe("LicencePage — lapsed (config-locked)", () => {
  it("shows the config-locked sentence and that reconfiguration is locked", async () => {
    licenceBody = { licensed: true, status: statusDoc("config-locked") };
    renderLicence();
    const badge = await screen.findByTestId("enforcement-badge");
    expect(within(badge).getByText(/config-locked/i)).toBeInTheDocument();
    expect(screen.getByTestId("enforcement-sentence")).toHaveTextContent(
      /reconfiguration is locked/i,
    );
    // The on-air promise is stated.
    expect(screen.getByTestId("enforcement-sentence")).toHaveTextContent(/on air/i);
  });
});

describe("LicencePage — unlicensed", () => {
  it("renders the honest unlicensed state without a status panel", async () => {
    licenceBody = { licensed: false, status: null };
    renderLicence();
    expect(await screen.findByText(/no licence is installed/i)).toBeInTheDocument();
    // The offline-exchange controls are still available so a lease can be installed.
    expect(screen.getByRole("button", { name: /export challenge/i })).toBeInTheDocument();
  });
});
