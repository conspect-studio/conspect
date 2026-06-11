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
    expect(await screen.findByRole("note")).toHaveTextContent(
      /config export.*restart|exporting the configuration/i,
    );
  });

  it("shows an honest dash when a source is not in the running engine", async () => {
    renderSources();
    expect(await screen.findByText("Camera 1")).toBeInTheDocument();
    // No realtime tile cache entries exist in this test, so both rows show the
    // explained placeholder (text, not colour).
    const dashes = screen.getAllByText("Not in the running engine", { exact: false });
    expect(dashes.length).toBeGreaterThanOrEqual(2);
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
