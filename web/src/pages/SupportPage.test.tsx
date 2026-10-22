// MSW tests for the Support surface (/help/support): entitlement-gated (GET
// /api/v1/support/entitlement). Eligible → raise-ticket form (severity
// Question/Degraded/Blocking + auto-attached context shown before submit +
// routing shown per entitlement) + ticket list/thread/reply. Free → community
// links + the one quiet line. Plus the context-pack composer (window +
// include[] → bundle preview incl. the redaction list + the never-media
// statement on-screen).
import { afterAll, afterEach, beforeAll, describe, expect, it } from "vitest";
import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { http, HttpResponse } from "msw";
import { setupServer } from "msw/node";
import { MemoryRouter } from "react-router-dom";

import { SupportPage } from "./SupportPage";
import type { LicenceResource, SupportEntitlement } from "../api/conspect";
import { renderWithProviders } from "../test/render";

let entitlement: SupportEntitlement = {
  eligible: true,
  route: { to: "studio-queue", first_line: "conspect" },
  sla: "standard",
};
let lastTicket: unknown;
let lastBundle: { window: string; include: string[] } | undefined;

const TICKETS = [
  { ticket_id: "CS-0001", subject: "Encoder stalls", severity: "degraded", state: "open", updates: 2 },
];
const TICKET_FULL = {
  ticket_id: "CS-0001",
  subject: "Encoder stalls",
  severity: "degraded",
  state: "open",
  route: { to: "studio-queue", first_line: "conspect" },
  context: {
    app_version: "1.2.3",
    entitlement: { tier: "studio", licensed: true },
    enforcement: { level: "active" },
    fingerprint_score: 95,
  },
  updates: [
    { author: "local", at_nanos: 1_000_000_000, body: "It stalls under load." },
    { author: "support:aperim", at_nanos: 2_000_000_000, body: "Can you attach a bundle?" },
  ],
};

const BUNDLE = {
  bundle_id: "bnd-1",
  window: "24h",
  composed_at_nanos: 1,
  redactions: [
    { path: "cam-1.auth.secret_ref", reason: "secret" },
    { path: "cam-2.url", reason: "url" },
  ],
};

const LICENCE: LicenceResource = {
  licensed: true,
  status: {
    tier: "studio",
    state: "compliant",
    enforcement: "active",
    hardware_class: { licensed: "standard", detected: "standard" },
    gpu_limit: { kind: "unlimited" },
    gpus_in_use: 0,
    config_locked: false,
    watermark: false,
    blocks_new_instances: false,
    lease: {
      serial: "LS-0001",
      source: "online",
      granted_at: "2999-01-01T00:00:00Z",
      expires_at: "2999-02-05T00:00:00Z",
      grace_days: 14,
      grace_until: "2999-02-19T00:00:00Z",
      hard_at: "2999-03-30T00:00:00Z",
      next_contact_due: "2999-02-01T00:00:00Z",
    },
    reasons: [],
  },
};

const server = setupServer(
  http.get("*/api/v1/licence", () => HttpResponse.json(LICENCE)),
  http.get("*/api/v1/support/entitlement", () => HttpResponse.json(entitlement)),
  http.get("*/api/v1/support/tickets", () => HttpResponse.json(TICKETS)),
  http.get("*/api/v1/support/tickets/CS-0001", () => HttpResponse.json(TICKET_FULL)),
  http.post("*/api/v1/support/tickets", async ({ request }) => {
    lastTicket = await request.json();
    return HttpResponse.json(TICKET_FULL, { status: 201 });
  }),
  http.post("*/api/v1/support/tickets/CS-0001/reply", async ({ request }) => {
    const body = (await request.json()) as { body: string };
    return HttpResponse.json({
      ...TICKET_FULL,
      updates: [...TICKET_FULL.updates, { author: "local", at_nanos: 3e9, body: body.body }],
    });
  }),
  http.post("*/api/v1/support/bundle", async ({ request }) => {
    lastBundle = (await request.json()) as { window: string; include: string[] };
    return HttpResponse.json({ bundle_id: "bnd-1" }, { status: 202 });
  }),
  http.get("*/api/v1/support/bundle/bnd-1", () => HttpResponse.json(BUNDLE)),
);

beforeAll(() => {
  server.listen();
});
afterEach(() => {
  entitlement = {
    eligible: true,
    route: { to: "studio-queue", first_line: "conspect" },
    sla: "standard",
  };
  lastTicket = undefined;
  lastBundle = undefined;
  server.resetHandlers();
});
afterAll(() => {
  server.close();
});

function renderSupport(): void {
  renderWithProviders(
    <MemoryRouter>
      <SupportPage />
    </MemoryRouter>,
  );
}

describe("SupportPage — eligible (entitled)", () => {
  it("shows the raise-ticket form with the three severities and the routing", async () => {
    renderSupport();
    const form = await screen.findByTestId("raise-ticket-form");
    const severity = within(form).getByLabelText(/severity/i);
    const options = within(severity).getAllByRole("option");
    const labels = options.map((o) => o.textContent.toLowerCase());
    expect(labels.some((l) => l.includes("question"))).toBe(true);
    expect(labels.some((l) => l.includes("degraded"))).toBe(true);
    expect(labels.some((l) => l.includes("blocking"))).toBe(true);
    // Routing shown per the entitlement.
    expect(within(form).getByText(/studio-queue/i)).toBeInTheDocument();
  });

  it("shows the auto-attached context before submit", async () => {
    renderSupport();
    const form = await screen.findByTestId("raise-ticket-form");
    const context = within(form).getByTestId("ticket-context");
    // The operator-visible context (tier + enforcement) is shown before submit,
    // sourced from the live licence resource.
    await waitFor(() => {
      expect(context).toHaveTextContent(/studio/i);
    });
    expect(context).toHaveTextContent(/active/i);
  });

  it("raises a ticket with the chosen severity", async () => {
    renderSupport();
    const form = await screen.findByTestId("raise-ticket-form");
    await userEvent.type(within(form).getByLabelText(/subject/i), "New issue");
    await userEvent.type(within(form).getByLabelText(/description/i), "Details here");
    await userEvent.selectOptions(within(form).getByLabelText(/severity/i), "blocking");
    await userEvent.click(within(form).getByRole("button", { name: /raise ticket/i }));
    await waitFor(() => {
      expect(lastTicket).toMatchObject({
        subject: "New issue",
        body: "Details here",
        severity: "blocking",
      });
    });
  });

  it("lists tickets and opens a thread, then replies", async () => {
    renderSupport();
    const list = await screen.findByTestId("ticket-list");
    await userEvent.click(within(list).getByRole("button", { name: /CS-0001/i }));
    const thread = await screen.findByTestId("ticket-thread");
    expect(within(thread).getByText(/it stalls under load/i)).toBeInTheDocument();
    await userEvent.type(within(thread).getByLabelText(/reply/i), "Attaching now");
    await userEvent.click(within(thread).getByRole("button", { name: /send reply/i }));
    await waitFor(() => {
      expect(within(screen.getByTestId("ticket-thread")).getByText(/attaching now/i)).toBeInTheDocument();
    });
  });
});

describe("SupportPage — free tier", () => {
  it("shows community links and the one quiet line, not a ticket form", async () => {
    entitlement = {
      eligible: false,
      route: { to: "community", first_line: "community" },
      sla: "community-best-effort",
    };
    renderSupport();
    expect(await screen.findByTestId("community-support")).toBeInTheDocument();
    expect(screen.queryByTestId("raise-ticket-form")).toBeNull();
    // The one quiet line references the SLA token.
    expect(screen.getByTestId("community-quiet-line")).toBeInTheDocument();
  });
});

describe("SupportPage — context-pack composer", () => {
  it("composes a bundle and previews the redaction list + never-media statement", async () => {
    renderSupport();
    const composer = await screen.findByTestId("context-pack-composer");
    // Window + at least one include checkbox.
    await userEvent.selectOptions(within(composer).getByLabelText(/window/i), "24h");
    await userEvent.click(within(composer).getByLabelText(/diagnostics/i));
    await userEvent.click(within(composer).getByRole("button", { name: /compose/i }));
    await waitFor(() => {
      expect(lastBundle).toMatchObject({ window: "24h" });
    });
    expect(lastBundle?.include).toContain("diagnostics");
    // The preview lists every redaction (a location, never the value).
    const preview = await screen.findByTestId("bundle-preview");
    expect(within(preview).getByText("cam-1.auth.secret_ref")).toBeInTheDocument();
    expect(within(preview).getByText("cam-2.url")).toBeInTheDocument();
    // The never-media statement is on-screen.
    expect(within(composer).getByTestId("never-media-statement")).toBeInTheDocument();
  });
});
