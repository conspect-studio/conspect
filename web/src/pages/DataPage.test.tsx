// MSW tests for the Data screen (/settings/data): TWO visibly separate panels
// (§4, never co-mingled) — the licensing heartbeat (LOCKED row, no toggle,
// "always on", last/next-due, the exhaustive payload list, the source-build
// caveat) and the telemetry pipe (consent toggle wired to GET/PUT, the schema
// summary incl. the never-sent list, what consent enables / what staying off
// costs, the incentive line) — plus the diagnostics-snapshot 202→download flow.
import { afterAll, afterEach, beforeAll, describe, expect, it } from "vitest";
import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { http, HttpResponse } from "msw";
import { setupServer } from "msw/node";
import { MemoryRouter } from "react-router-dom";

import { DataPage } from "./DataPage";
import { renderWithProviders } from "../test/render";

const HEARTBEAT = {
  transport: "direct",
  last_at: "2026-05-01T00:00:00Z",
  next_due: "2026-06-01T00:00:00Z",
  payload_fields: ["fingerprint_digest", "lease_request", "signed_assertion"],
};

const SCHEMA = {
  version: "1.0",
  sent: ["uptime_seconds", "tile_count", "codec_in_use"],
  never_sent: ["media_content", "source_urls", "raw_serials"],
};

let consentBody = { enabled: false, actor: "local", changed_at: null as string | null };
let lastConsentPut: boolean | undefined;
let snapshotRequested = 0;

const server = setupServer(
  http.get("*/api/v1/licensing/heartbeat-status", () => HttpResponse.json(HEARTBEAT)),
  http.get("*/api/v1/telemetry/schema", () => HttpResponse.json(SCHEMA)),
  http.get("*/api/v1/telemetry/consent", () => HttpResponse.json(consentBody)),
  http.put("*/api/v1/telemetry/consent", async ({ request }) => {
    const body = (await request.json()) as { enabled: boolean };
    lastConsentPut = body.enabled;
    consentBody = { enabled: body.enabled, actor: "local", changed_at: "2026-06-11T00:00:00Z" };
    return HttpResponse.json(consentBody);
  }),
  http.post("*/api/v1/diagnostics/snapshot", () => {
    snapshotRequested += 1;
    return HttpResponse.json({ snapshot_id: "snap-1" }, { status: 202 });
  }),
  http.get("*/api/v1/diagnostics/snap-1", () =>
    HttpResponse.json({
      snapshot_id: "snap-1",
      status: "ready",
      assembled_at_unix_seconds: 1_700_000_000,
      diagnostics: { bundle_id: "snap-1", window: "24h", composed_at_nanos: 1, redactions: [] },
    }),
  ),
);

beforeAll(() => {
  server.listen();
});
afterEach(() => {
  consentBody = { enabled: false, actor: "local", changed_at: null };
  lastConsentPut = undefined;
  snapshotRequested = 0;
  server.resetHandlers();
});
afterAll(() => {
  server.close();
});

function renderData(): void {
  renderWithProviders(
    <MemoryRouter>
      <DataPage />
    </MemoryRouter>,
  );
}

describe("DataPage — two-pipe separation", () => {
  it("renders TWO separate panels (heartbeat + telemetry), never co-mingled", async () => {
    renderData();
    await screen.findByTestId("heartbeat-panel");
    expect(screen.getByTestId("heartbeat-panel")).toBeInTheDocument();
    expect(screen.getByTestId("telemetry-panel")).toBeInTheDocument();
    // The two panels are distinct elements, not nested in one another.
    const hb = screen.getByTestId("heartbeat-panel");
    const tm = screen.getByTestId("telemetry-panel");
    expect(hb.contains(tm)).toBe(false);
    expect(tm.contains(hb)).toBe(false);
  });
});

describe("DataPage — licensing heartbeat (LOCKED)", () => {
  it("has NO toggle control in the heartbeat panel (locked, always on)", async () => {
    renderData();
    const panel = await screen.findByTestId("heartbeat-panel");
    // No switch/checkbox/toggle button inside the locked row.
    expect(within(panel).queryByRole("switch")).toBeNull();
    expect(within(panel).queryByRole("checkbox")).toBeNull();
    const pressables = within(panel)
      .queryAllByRole("button")
      .filter((b) => b.getAttribute("aria-pressed") !== null);
    expect(pressables.length).toBe(0);
  });

  it("states it is always on", async () => {
    renderData();
    const panel = await screen.findByTestId("heartbeat-panel");
    expect(within(panel).getByText(/always on/i)).toBeInTheDocument();
  });

  it("lists the exhaustive heartbeat payload fields", async () => {
    renderData();
    const panel = await screen.findByTestId("heartbeat-panel");
    // The payload list renders once the heartbeat query resolves.
    await within(panel).findByText(HEARTBEAT.payload_fields[0] ?? "");
    for (const field of HEARTBEAT.payload_fields) {
      expect(within(panel).getByText(field)).toBeInTheDocument();
    }
  });

  it("states the source-build caveat plainly", async () => {
    renderData();
    const panel = await screen.findByTestId("heartbeat-panel");
    expect(within(panel).getByText(/source build/i)).toBeInTheDocument();
  });
});

describe("DataPage — telemetry (opt-in)", () => {
  it("renders a consent toggle reflecting the GET state (off by default)", async () => {
    renderData();
    const toggle = await screen.findByRole("switch", { name: /telemetry/i });
    expect(toggle).toHaveAttribute("aria-checked", "false");
  });

  it("PUTs the new consent state when toggled on", async () => {
    renderData();
    const toggle = await screen.findByRole("switch", { name: /telemetry/i });
    await userEvent.click(toggle);
    await waitFor(() => {
      expect(lastConsentPut).toBe(true);
    });
    await waitFor(() => {
      expect(screen.getByRole("switch", { name: /telemetry/i })).toHaveAttribute(
        "aria-checked",
        "true",
      );
    });
  });

  it("summarises the schema including the never-sent list", async () => {
    renderData();
    const panel = await screen.findByTestId("telemetry-panel");
    // The schema lists render once the schema query resolves.
    await within(panel).findByText(SCHEMA.never_sent[0] ?? "");
    for (const field of SCHEMA.never_sent) {
      expect(within(panel).getByText(field)).toBeInTheDocument();
    }
    for (const field of SCHEMA.sent) {
      expect(within(panel).getByText(field)).toBeInTheDocument();
    }
    // Links to the full schema endpoint.
    expect(within(panel).getByRole("link", { name: /schema/i })).toHaveAttribute(
      "href",
      "/api/v1/telemetry/schema",
    );
  });

  it("states what consent enables and what staying off costs, plus the incentive", async () => {
    renderData();
    const panel = await screen.findByTestId("telemetry-panel");
    expect(within(panel).getByTestId("telemetry-enables")).toBeInTheDocument();
    expect(within(panel).getByTestId("telemetry-cost")).toBeInTheDocument();
    expect(within(panel).getByTestId("telemetry-incentive")).toBeInTheDocument();
  });
});

describe("DataPage — diagnostics snapshot", () => {
  it("requests a snapshot (202) then offers the assembled bundle to download", async () => {
    renderData();
    const button = await screen.findByRole("button", { name: /diagnostics snapshot/i });
    await userEvent.click(button);
    await waitFor(() => {
      expect(snapshotRequested).toBe(1);
    });
    // Once ready, a download control appears.
    expect(await screen.findByRole("button", { name: /download snapshot/i })).toBeInTheDocument();
  });
});
