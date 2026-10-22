// MSW tests for the account-side global chrome: the header licence chip
// (→ /settings/account when unclaimed, → /settings/licence when claimed), the
// ladder banner (quiet when active, raised at warning-or-worse with the reason +
// remediation), and the pending-action strip (shown only when actions are
// queued). The config-lock interceptor hook is pinned in its own test below.
import type { JSX } from "react";
import { afterAll, afterEach, beforeAll, describe, expect, it } from "vitest";
import { screen, waitFor, within } from "@testing-library/react";
import { http, HttpResponse } from "msw";
import { setupServer } from "msw/node";
import { MemoryRouter } from "react-router-dom";

import {
  ConfigLockBanner,
  LadderBanner,
  LicenceChip,
  PendingActionStrip,
} from "./chrome";
import type {
  EnforcementLevel,
  LicenceResource,
  LicenceStatusDoc,
  PendingAction,
} from "../../api/conspect";
import { renderWithProviders } from "../../test/render";

let licenceBody: LicenceResource = { licensed: false, status: null };
let pending: PendingAction[] = [];

function statusWith(enforcement: EnforcementLevel): LicenceStatusDoc {
  return {
    tier: "studio",
    state: "compliant",
    enforcement,
    hardware_class: { licensed: "standard", detected: "standard" },
    gpu_limit: { kind: "unlimited" },
    gpus_in_use: 0,
    config_locked: enforcement === "config-locked" || enforcement === "watermark",
    watermark: enforcement === "watermark",
    blocks_new_instances: enforcement === "block-new-instance",
    lease: {
      serial: "LS-1",
      source: "online",
      granted_at: "2999-01-01T00:00:00Z",
      expires_at: "2999-02-05T00:00:00Z",
      grace_days: 14,
      grace_until: "2999-02-19T00:00:00Z",
      hard_at: "2999-03-30T00:00:00Z",
      next_contact_due: "2999-02-01T00:00:00Z",
    },
    reasons: ["lease-expiring"],
  };
}

const server = setupServer(
  http.get("*/api/v1/licence", () => HttpResponse.json(licenceBody)),
  http.get("*/api/v1/actions/pending", () => HttpResponse.json(pending)),
);

beforeAll(() => {
  server.listen();
});
afterEach(() => {
  licenceBody = { licensed: false, status: null };
  pending = [];
  server.resetHandlers();
});
afterAll(() => {
  server.close();
});

function render(ui: JSX.Element): void {
  renderWithProviders(<MemoryRouter>{ui}</MemoryRouter>);
}

describe("LicenceChip", () => {
  it("links to the account screen when unclaimed", async () => {
    licenceBody = { licensed: false, status: null };
    render(<LicenceChip />);
    const link = await screen.findByRole("link", { name: /unclaimed/i });
    expect(link).toHaveAttribute("href", "/settings/account");
  });

  it("links to the licence screen with the tier text when claimed", async () => {
    licenceBody = { licensed: true, status: statusWith("active") };
    render(<LicenceChip />);
    const link = await screen.findByRole("link", { name: /studio/i });
    expect(link).toHaveAttribute("href", "/settings/licence");
  });
});

describe("LadderBanner", () => {
  it("is quiet (renders nothing) when active", async () => {
    licenceBody = { licensed: true, status: statusWith("active") };
    const { container } = renderWithProviders(
      <MemoryRouter>
        <LadderBanner />
      </MemoryRouter>,
    );
    // Give the query a tick; the banner must stay empty for an active licence.
    await waitFor(() => {
      expect(container.querySelector('[data-testid="ladder-banner"]')).toBeNull();
    });
  });

  it("raises an actionable callout at warning-or-worse with a link to the licence screen", async () => {
    licenceBody = { licensed: true, status: statusWith("config-locked") };
    render(<LadderBanner />);
    const banner = await screen.findByTestId("ladder-banner");
    expect(within(banner).getByText(/reconfiguration is locked/i)).toBeInTheDocument();
    expect(within(banner).getByRole("link", { name: /licence/i })).toHaveAttribute(
      "href",
      "/settings/licence",
    );
  });
});

describe("ConfigLockBanner", () => {
  it("renders nothing when reconfiguration is not locked", async () => {
    licenceBody = { licensed: true, status: statusWith("active") };
    const { container } = renderWithProviders(
      <MemoryRouter>
        <ConfigLockBanner />
      </MemoryRouter>,
    );
    await waitFor(() => {
      expect(container.querySelector('[data-testid="config-lock-banner"]')).toBeNull();
    });
  });

  it("states the ladder reason and links to the licence screen when locked", async () => {
    licenceBody = { licensed: true, status: statusWith("config-locked") };
    render(<ConfigLockBanner />);
    const banner = await screen.findByTestId("config-lock-banner");
    expect(within(banner).getByText(/reconfiguration is locked/i)).toBeInTheDocument();
    expect(within(banner).getByRole("link", { name: /licence/i })).toHaveAttribute(
      "href",
      "/settings/licence",
    );
  });
});

describe("PendingActionStrip", () => {
  it("renders nothing when no actions are queued", async () => {
    pending = [];
    const { container } = renderWithProviders(
      <MemoryRouter>
        <PendingActionStrip />
      </MemoryRouter>,
    );
    await waitFor(() => {
      expect(container.querySelector('[data-testid="pending-action-strip"]')).toBeNull();
    });
  });

  it("shows a strip linking to system actions when an action is queued", async () => {
    pending = [
      {
        action_id: "act-9",
        kind: "restart",
        requested_by: "portal:ops",
        requested_at_nanos: 1,
        state: "pending",
        detail: null,
      },
    ];
    render(<PendingActionStrip />);
    const strip = await screen.findByTestId("pending-action-strip");
    expect(within(strip).getByRole("link", { name: /review/i })).toHaveAttribute(
      "href",
      "/system/actions",
    );
  });
});
