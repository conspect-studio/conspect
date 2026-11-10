// SMOKE test: every resource dialog (create / edit / delete) renders over a
// populated table, plus the kind-specific form behaviours (inline per-field
// errors, the apply-semantics callout, the honest live-status dash).
//
// NOTE — this does NOT guard the renderer-OOM re-render loop that the
// `ResourceTable` `data` memo fixes. That loop only reproduces in a REAL browser
// (it needs Radix Dialog's focus/scroll-lock effects + `ResizeObserver` to
// sustain the re-render cascade), which jsdom does not implement. The actual
// regression guard is the Playwright e2e in `e2e/dialogs.spec.ts`, which drives
// a real chromium. This suite keeps the dialog markup/forms honest.
import { afterAll, afterEach, beforeAll, describe, expect, it } from "vitest";
import { screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { http, HttpResponse } from "msw";
import { setupServer } from "msw/node";
import { MemoryRouter } from "react-router-dom";

import { OutputsPage, SourcesPage } from "./SimplePages";
import { OutputLinkStatus } from "./OutputsPage";
import { renderWithProviders } from "../test/render";

const SOURCES = [
  { id: "cam-1", name: "Camera 1", body: { id: "cam-1", kind: "rtsp", url: "rtsp://host/one" } },
  { id: "cam-2", name: "Camera 2", body: { id: "cam-2", kind: "bars" } },
  // A kind this UI has no form for: rendered + deletable, but NOT editable —
  // the authored document must never be folded onto a different kind.
  { id: "aoip-1", name: "AoIP feed", body: { id: "aoip-1", kind: "aes67", url: "aes67://x" } },
];

const OUTPUTS = [
  {
    id: "out-rtsp",
    name: "RTSP server",
    body: { id: "out-rtsp", kind: "rtsp_server", mount: "/multiview", codec: "h264" },
  },
  {
    id: "out-hls",
    name: "HLS",
    body: { id: "out-hls", kind: "hls", path: "/hls/m", codec: "h264" },
  },
];

const server = setupServer(
  http.get("*/api/v1/sources/status", () => HttpResponse.json([])),
  http.get("*/api/v1/sources", () => HttpResponse.json(SOURCES)),
  http.get("*/api/v1/sources/:id", ({ params }) => {
    const found = SOURCES.find((s) => s.id === String(params.id));
    return found
      ? HttpResponse.json(found, { headers: { ETag: '"1"' } })
      : new HttpResponse(null, { status: 404 });
  }),
  http.get("*/api/v1/outputs", () => HttpResponse.json(OUTPUTS)),
  http.get("*/api/v1/outputs/:id", ({ params }) => {
    const found = OUTPUTS.find((o) => o.id === String(params.id));
    return found
      ? HttpResponse.json(found, { headers: { ETag: '"1"' } })
      : new HttpResponse(null, { status: 404 });
  }),
);

beforeAll(() => {
  server.listen();
});
afterEach(() => {
  server.resetHandlers();
});
afterAll(() => {
  server.close();
});

// The pages render router links (HelpLink), so they need a Router in tests.
function renderSources(): void {
  renderWithProviders(
    <MemoryRouter>
      <SourcesPage />
    </MemoryRouter>,
  );
}

function renderOutputs(): void {
  renderWithProviders(
    <MemoryRouter>
      <OutputsPage />
    </MemoryRouter>,
  );
}

describe("SourcesPage dialogs render over a populated table (smoke)", () => {
  it("opens the create dialog", async () => {
    renderSources();
    // Table populated from the list query (the loop needs rows present).
    expect(await screen.findByText("Camera 1")).toBeInTheDocument();
    // This click re-renders the page with the table mounted — must not hang.
    await userEvent.click(screen.getByRole("button", { name: "New source" }));
    const dialog = await screen.findByRole("dialog");
    expect(dialog).toBeInTheDocument();
    expect(await screen.findByLabelText("Identifier")).toBeInTheDocument();
  });

  it("opens the delete confirmation", async () => {
    renderSources();
    expect(await screen.findByText("Camera 1")).toBeInTheDocument();
    await userEvent.click(
      screen.getByRole("button", { name: "Delete source: Camera 1" }),
    );
    expect(await screen.findByText("Delete this resource?")).toBeInTheDocument();
  });

  it("opens the edit dialog (loads the record)", async () => {
    renderSources();
    expect(await screen.findByText("Camera 1")).toBeInTheDocument();
    await userEvent.click(
      screen.getByRole("button", { name: "Edit source: Camera 1" }),
    );
    expect(await screen.findByRole("dialog")).toBeInTheDocument();
    // The kind-specific URL field prefills from the stored body.
    expect(await screen.findByDisplayValue("rtsp://host/one")).toBeInTheDocument();
  });
});

describe("SourcesPage honest surfaces", () => {
  it("shows the apply-semantics callout", async () => {
    renderSources();
    // The approved layout keeps the callout in the Help panel (still reachable).
    await userEvent.click(await screen.findByRole("button", { name: /help/i }));
    expect(await screen.findByRole("note")).toHaveTextContent(
      /config export.*restart|exporting the configuration/i,
    );
  });

  it("shows unknown status without lifecycle actions when demand is absent", async () => {
    renderSources();
    expect(await screen.findByText("Camera 1")).toBeInTheDocument();
    expect(screen.getAllByText("Status unknown")).toHaveLength(SOURCES.length);
    expect(screen.queryByText("Idle")).not.toBeInTheDocument();
    expect(screen.queryByText(/not running and costs nothing/)).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /^(Start|Stop)/ })).not.toBeInTheDocument();
    expect(screen.queryByText(/No signal/i)).toBeNull();
  });

  it("renders an unknown-kind row as authored but refuses to edit it", async () => {
    renderSources();
    // The row renders normally, showing the RAW authored kind (never folded
    // to bars — the old fold silently rewrote the record on save).
    expect(await screen.findByText("AoIP feed")).toBeInTheDocument();
    expect(screen.getByText("aes67")).toBeInTheDocument();
    // Edit is disabled with an accessible explanation; delete stays allowed.
    const edit = screen.getByRole("button", { name: /Edit source: AoIP feed/ });
    expect(edit).toHaveAttribute("aria-disabled", "true");
    expect(edit.getAttribute("aria-label")).toMatch(/isn't editable in this UI/i);
    await userEvent.click(edit);
    // No edit dialog opened — the document is preserved as authored.
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: "Delete source: AoIP feed" }),
    ).toBeEnabled();
  });

  it("renders inline per-field errors on an invalid submit", async () => {
    renderSources();
    expect(await screen.findByText("Camera 1")).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "New source" }));
    await screen.findByRole("dialog");
    // Wrong scheme for an RTSP source: per-field message, wired to the input.
    const url = await screen.findByLabelText("Source URL");
    await userEvent.type(url, "http://host/stream");
    await userEvent.click(screen.getByRole("button", { name: "Create" }));
    expect(
      await screen.findByText(/must start with rtsp:\/\//i),
    ).toBeInTheDocument();
    expect(url).toHaveAttribute("aria-invalid", "true");
    const describedBy = url.getAttribute("aria-describedby") ?? "";
    expect(describedBy).not.toBe("");
  });
});

describe("OutputsPage runnability honesty", () => {
  it("marks rtsp_server as not yet runnable and hls as runnable", async () => {
    renderOutputs();
    expect(await screen.findByText("RTSP server")).toBeInTheDocument();
    expect(
      screen.getByText("Not yet runnable in this build"),
    ).toBeInTheDocument();
    expect(screen.getByText("Runnable")).toBeInTheDocument();
  });
});

describe("OutputLinkStatus", () => {
  function renderLink(body: unknown, status = 200): void {
    server.use(
      http.get("*/api/v1/outputs/:id/link", () =>
        HttpResponse.json(body as object, { status }),
      ),
    );
    renderWithProviders(<OutputLinkStatus id="srt-main" />);
  }
  const link = {
    state: "listening",
    mode: "listener",
    transport: "srt",
    url: "srt://[::]:9000",
    clients: 0,
    sessions: 0,
    dropped: 0,
    written: 0,
    peers: [],
  };

  it("tells the operator no caller is attached", async () => {
    renderLink(link);
    expect(
      await screen.findByText("Listening, no caller attached"),
    ).toBeInTheDocument();
  });

  it("shows attached peers, the last error and drops", async () => {
    renderLink({
      ...link,
      state: "connected",
      clients: 2,
      peers: ["[2001:db8::7]:5000", "192.0.2.4:5001"],
      last_error: "connection lost",
      dropped: 42,
    });
    expect(await screen.findByText("Connected (2)")).toBeInTheDocument();
    expect(screen.getByText("[2001:db8::7]:5000")).toBeInTheDocument();
    expect(screen.getByText("192.0.2.4:5001")).toBeInTheDocument();
    expect(screen.getByText("connection lost")).toBeInTheDocument();
    // A body with no reason counters (an older server) keeps its conflated
    // total; the split is only possible once the server sends it.
    expect(screen.getByText("42 packets dropped")).toBeInTheDocument();
  });

  it("reports no-caller discards as information, never as drops", async () => {
    renderLink({
      ...link,
      state: "listening",
      clients: 0,
      dropped: 1_412_904,
      discarded_no_client: 1_412_904,
      discarded_awaiting_keyframe: 0,
      dropped_slow_peer: 0,
      dropped_upstream: 0,
    });
    expect(
      await screen.findByText("Listening, no caller attached"),
    ).toBeInTheDocument();
    const discards = screen.getByText(
      "1412904 packets discarded while no caller was attached",
    );
    expect(discards).toHaveClass("text-muted-foreground");
    expect(discards).not.toHaveClass("text-destructive");
    expect(screen.queryByText(/packets dropped/)).not.toBeInTheDocument();
  });

  it("still alarms on a real slow-peer drop and separates the benign holds", async () => {
    renderLink({
      ...link,
      state: "connected",
      clients: 1,
      dropped: 45,
      discarded_no_client: 0,
      discarded_awaiting_keyframe: 15,
      dropped_slow_peer: 30,
      dropped_upstream: 0,
    });
    expect(await screen.findByText("Connected (1)")).toBeInTheDocument();
    const slow = screen.getByText(
      "30 packets dropped (slow peer, incl. keyframe resync)",
    );
    expect(slow).toHaveClass("text-destructive");
    expect(
      screen.getByText("15 packets discarded waiting for the first keyframe"),
    ).toHaveClass("text-muted-foreground");
    expect(screen.queryByText(/discarded while no caller/)).not.toBeInTheDocument();
  });

  it("alarms on a slow peer's resync hold, never muting it as benign", async () => {
    // t10 / SRT-STATUS-01 regression: after an overflow the peer's held packets
    // (audio and non-IDR video) are loss it caused, so they arrive in
    // `dropped_slow_peer` and must render as an alarm — not under the muted
    // "waiting for a keyframe" line they were once conflated with.
    renderLink({
      ...link,
      state: "connected",
      clients: 1,
      dropped: 61,
      discarded_no_client: 0,
      discarded_awaiting_keyframe: 0,
      dropped_slow_peer: 61,
      dropped_upstream: 0,
    });
    expect(await screen.findByText("Connected (1)")).toBeInTheDocument();
    const lost = screen.getByText(
      "61 packets dropped (slow peer, incl. keyframe resync)",
    );
    expect(lost).toHaveClass("text-destructive");
    expect(lost).not.toHaveClass("text-muted-foreground");
    expect(
      screen.queryByText(/waiting for the first keyframe/),
    ).not.toBeInTheDocument();
  });

  it("keeps a fresh session's first-keyframe wait muted", async () => {
    renderLink({
      ...link,
      state: "connected",
      clients: 1,
      dropped: 12,
      discarded_no_client: 0,
      discarded_awaiting_keyframe: 12,
      dropped_slow_peer: 0,
      dropped_upstream: 0,
    });
    const waiting = await screen.findByText(
      "12 packets discarded waiting for the first keyframe",
    );
    expect(waiting).toHaveClass("text-muted-foreground");
    expect(waiting).not.toHaveClass("text-destructive");
    expect(screen.queryByText(/slow peer/)).not.toBeInTheDocument();
  });

  it("keeps a previous session's error as muted history, not a current alarm", async () => {
    renderLink({
      ...link,
      state: "listening",
      clients: 0,
      dropped: 5,
      discarded_no_client: 5,
      discarded_awaiting_keyframe: 0,
      dropped_slow_peer: 0,
      dropped_upstream: 0,
      last_error: null,
      last_session_error: "Input/output error",
    });
    const history = await screen.findByText(
      "Last session ended: Input/output error",
    );
    expect(history).toHaveClass("text-muted-foreground");
    expect(history).not.toHaveClass("text-destructive");
  });

  it("renders nothing while the program is stopped", async () => {
    renderLink({ title: "not found", status: 404 }, 404);
    await new Promise((resolve) => setTimeout(resolve, 50));
    expect(screen.queryByTestId("output-link")).not.toBeInTheDocument();
  });
});
