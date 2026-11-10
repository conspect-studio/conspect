// MSW test for the Audio page's broadcast console (the adopted designer
// direction, honest to the REAL API): loads the singleton document
// (GET /api/v1/audio-routing), renders the whole-mix overview + strip console
// (focused strip below xl, bank at xl+), lists the selectable tracks, validates
// inline, and saves the WHOLE document with PUT + If-Match. The console's
// core honesty is pinned here: gain/mute/PGM edits STAGE THE DRAFT ONLY (no
// request leaves the page on a nudge, a fader change or a key tap), Draft vs
// Saved is always explicit, per-strip Undo is a LOCAL draft revert, and the
// save status reports exactly the server's X-Multiview-Apply (live / partial /
// restart) plus its authoritative outputs_updated report.
import { afterAll, afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import { fireEvent, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { http, HttpResponse } from "msw";
import { setupServer } from "msw/node";
import { MemoryRouter } from "react-router-dom";

import { AudioPage } from "./AudioPage";
import { renderWithProviders } from "../test/render";

const SOURCES = [
  { id: "cam-1", name: "Camera 1", body: { id: "cam-1", kind: "rtsp", url: "rtsp://host/one" } },
  { id: "cam-2", name: "Camera 2", body: { id: "cam-2", kind: "bars" } },
];

const ROUTING = {
  configured: true,
  routing: {
    sample_rate_hz: 48000,
    routes: [
      {
        input_id: "cam-1",
        channels: { kind: "stereo" },
        target_track: "cam1-clean",
        language: "eng",
        title: "Camera 1",
        include_in_program_bus: true,
        gain_db: -3,
        mute: false,
      },
    ],
  },
  selectable_tracks: ["prog", "cam1-clean"],
};

/** A second two-channel document for the strip-picker / whole-mix tests. */
const ROUTING_TWO = {
  ...ROUTING,
  routing: {
    sample_rate_hz: 48000,
    routes: [
      ROUTING.routing.routes[0],
      {
        input_id: "cam-2",
        channels: { kind: "mono" },
        include_in_program_bus: false,
        gain_db: 0,
        mute: false,
      },
    ],
  },
  selectable_tracks: ["prog", "cam1-clean"],
};

/** An output currently selecting the discrete track the ROUTING document
 * above declares (`cam1-clean`) — the fixture the pending-impact warning and
 * the outputs_updated note exercise. */
const OUTPUTS = [
  {
    id: "hls-out",
    name: "HLS",
    body: {
      id: "hls-out",
      kind: "hls",
      path: "/tmp/out.m3u8",
      codec: "h264",
      audio: { mode: "tracks", tracks: ["prog", "cam1-clean"] },
    },
  },
];

/** The last PUT body + If-Match header the mock server saw. */
let lastPut: { body: unknown; ifMatch: string | null } | undefined;

const server = setupServer(
  http.get("*/api/v1/sources", () => HttpResponse.json(SOURCES)),
  http.get("*/api/v1/outputs", () => HttpResponse.json(OUTPUTS)),
  http.get("*/api/v1/audio-routing", () =>
    HttpResponse.json(ROUTING, { headers: { ETag: 'W/"7"' } }),
  ),
  http.post("*/api/v1/ws/ticket", () => new HttpResponse(null, { status: 401 })),
  http.put("*/api/v1/audio-routing", async ({ request }) => {
    lastPut = {
      body: await request.json(),
      ifMatch: request.headers.get("if-match"),
    };
    return HttpResponse.json(
      { ...ROUTING, configured: true },
      { headers: { ETag: 'W/"8"', "X-Multiview-Apply": "restart" } },
    );
  }),
);

beforeAll(() => {
  server.listen();
});
afterEach(() => {
  lastPut = undefined;
  server.resetHandlers();
});
afterAll(() => {
  server.close();
});

it("locks every draft control until a gated PUT installs the canonical baseline", async () => {
  let release!: () => void;
  const gate = new Promise<void>((resolve) => { release = resolve; });
  server.use(http.put("*/api/v1/audio-routing", async ({ request }) => {
    lastPut = { body: await request.json(), ifMatch: request.headers.get("if-match") };
    await gate;
    return HttpResponse.json(ROUTING, { headers: { ETag: 'W/"8"' } });
  }));
  const user = userEvent.setup();
  renderAudio();
  const strip = (await screen.findAllByTestId("audio-strip"))[0]!;
  await user.click(within(strip).getAllByTestId("audio-nudge")[1]!);
  await user.click(screen.getByTestId("audio-save"));
  await waitFor(() => { expect(lastPut).toBeDefined(); });
  try {
    expect(screen.getByText("Saving audio routing. Draft controls are locked until the save completes.")).toBeInTheDocument();
    for (const control of document.querySelectorAll('fieldset input, fieldset select, fieldset textarea, fieldset button')) {
      expect(control).toBeDisabled();
    }
    await user.click(within(strip).getByTestId("audio-mute"));
    expect(within(strip).getByTestId("audio-mute")).toHaveAttribute("aria-pressed", "false");
    expect(within(strip).getByTestId("audio-strip-readout")).toHaveTextContent("-2");
  } finally { release(); }
  await waitFor(() => { expect(screen.getByTestId("audio-save")).toBeEnabled(); });
  expect(within(strip).getByTestId("audio-strip-readout")).toHaveTextContent("-3");
  expect(screen.queryByTestId("audio-draft-summary")).not.toBeInTheDocument();
});

function renderAudio(): void {
  renderWithProviders(
    <MemoryRouter>
      <AudioPage />
    </MemoryRouter>,
  );
}

/** Narrow a possibly-undefined indexed lookup (`noUncheckedIndexedAccess`). */
function required<T>(value: T | undefined, what: string): T {
  if (value === undefined) {
    throw new Error(`expected ${what}`);
  }
  return value;
}

/** The console's surfaces, scoped: the whole-mix overview, the (sub-xl)
 * focused strip + channel picker, and the (xl+) bank of strips. */
async function console() {
  const overview = await screen.findByTestId("audio-mix-overview");
  return {
    overview,
    focused: screen.getByTestId("audio-focused-strip"),
    bank: screen.getByTestId("audio-bank"),
    picker: screen.getByTestId("audio-strip-picker"),
  };
}

describe("AudioPage", () => {
  it("loads the saved routing into the console and lists the selectable tracks", async () => {
    renderAudio();
    const { overview, focused, bank } = await console();
    // The sample rate field (inside the Bus-setup disclosure) prefills.
    expect(await screen.findByLabelText(/sample rate/i)).toHaveValue(48000);
    // One channel everywhere: one overview row (plus the pinned PROGRAM row),
    // one bank strip, one focused strip, one picker chip.
    expect(within(overview).getAllByTestId("audio-overview-row")).toHaveLength(1);
    expect(within(bank).getAllByTestId("audio-strip")).toHaveLength(1);
    expect(within(focused).getAllByTestId("audio-strip")).toHaveLength(1);
    expect(within(screen.getByTestId("audio-strip-picker")).getAllByTestId("audio-strip-chip")).toHaveLength(1);
    expect(within(overview).getByTestId("audio-overview-program")).toBeInTheDocument();
    // The saved values are explicit: Draft -3 dB · Saved -3 dB on the strip.
    expect(within(focused).getByTestId("audio-strip-readout")).toHaveTextContent(
      /Draft -3 dB.*Saved -3 dB/s,
    );
    // The resulting selectable tracks are listed: the program bus + declared.
    const tracks = screen.getByTestId("audio-tracks-list");
    expect(within(tracks).getByText("prog")).toBeInTheDocument();
    expect(within(tracks).getByText("cam1-clean")).toBeInTheDocument();
    // A clean document shows no draft banner: nothing is claimed pending.
    expect(screen.queryByTestId("audio-draft-summary")).not.toBeInTheDocument();
  });

  it("stacks the four nudges beside the fader (side-stack) and renders one toolbar with one Add route / Save pair", async () => {
    renderAudio();
    const { focused } = await console();
    const fader = within(focused).getByTestId("audio-fader");
    const nudges = within(focused).getByTestId("audio-nudges");
    expect(nudges).toHaveClass("audio-nudge-col");
    expect(fader.parentElement?.parentElement).toContainElement(nudges);
    expect(within(nudges).getAllByTestId("audio-nudge")).toHaveLength(4);
    expect(screen.getAllByTestId("audio-toolbar")).toHaveLength(1);
    expect(screen.getAllByTestId("audio-save")).toHaveLength(1);
    expect(screen.getAllByTestId("audio-add-route")).toHaveLength(1);
  });

  it("puts the full-height fader before secondary strip controls and keeps context after the console", async () => {
    renderAudio();
    const { focused } = await console();
    const fader = within(focused).getByTestId("audio-fader");
    const mute = within(focused).getByTestId("audio-mute");
    const nudges = within(focused).getByTestId("audio-nudges");
    expect(fader.compareDocumentPosition(mute) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    expect(fader.compareDocumentPosition(nudges) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    // The fader is a custom pointer-event slider (not a rotated native range).
    expect(fader).toHaveAttribute("role", "slider");
    // Tablet first-fold contract: all essential keys share the fader's two
    // rows, not a stack below its 240 px track. Browser evidence also checks
    // every control's rectangle and hit target against the fixed footer.
    const essential = within(focused).getByTestId("audio-strip-essential");
    expect(essential).toHaveClass("flex-col", "md:max-xl:grid", "md:max-xl:grid-cols-2");
    expect(fader.parentElement?.parentElement).toHaveClass("md:max-xl:row-span-2");
    const pgm = within(essential).getByTestId("audio-pgm");
    expect(within(essential).getByTestId("audio-mute")).toBe(mute);
    expect(mute.compareDocumentPosition(pgm) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    // Round 2: the nudges are the fader's side-stack, so they precede the keys.
    expect(nudges.compareDocumentPosition(mute) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    expect(mute).toHaveClass("h-12", "min-w-12");
    expect(pgm).toHaveClass("h-12", "min-w-12");
    for (const nudge of within(essential).getAllByTestId("audio-nudge")) {
      expect(nudge).toHaveClass("h-12", "min-w-12");
    }
    expect(screen.getByRole("heading", { name: "Channel strip" })).toHaveClass("sr-only");
    const context = screen.getByTestId("audio-page-context");
    expect(focused.compareDocumentPosition(context) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    expect(context).toHaveTextContent(/Route each input's audio/);
    expect(within(context).getByRole("button", { name: /export configuration/i })).toBeInTheDocument();
  });

  it("bounds essential toolbar controls and wraps labels without reducing touch targets", async () => {
    renderAudio();
    await console();
    for (const control of [
      screen.getByTestId("audio-save"),
      screen.getByTestId("audio-add-route"),
      screen.getByRole("button", { name: "Whole mix overview" }),
      ...screen.getAllByTestId("audio-strip-chip"),
    ]) {
      expect(control).toHaveClass("h-auto", "min-h-12", "min-w-12", "max-w-full", "whitespace-normal", "break-words");
      expect(control).not.toHaveClass("h-12");
    }
    expect(screen.getByTestId("audio-save")).toHaveTextContent("Save audio routing");
  });

  it("shows the apply-semantics callout", async () => {
    renderAudio();
    expect(await screen.findByRole("note")).toHaveTextContent(
      /config export.*restart|exporting the configuration/i,
    );
  });

  it("toggles mute and program mix in the draft only — no request, saved state unchanged", async () => {
    renderAudio();
    const { overview, focused } = await console();
    const mute = within(focused).getByRole("button", { name: /Camera 1 MUTE/i });
    expect(mute).toHaveAttribute("aria-pressed", "false");
    await userEvent.click(mute);
    expect(mute).toHaveAttribute("aria-pressed", "true");
    expect(within(focused).getByTestId("audio-strip-status")).toHaveTextContent(/MUTED/);
    expect(within(focused).getByTestId("audio-strip-status")).toHaveTextContent(/in PGM/);
    // The overview keeps reporting the SAVED state, marked as edited.
    expect(within(overview).getByTestId("audio-draft-mark")).toBeInTheDocument();
    expect(lastPut).toBeUndefined();
    // PGM (program-bus include) is a draft toggle too — never a live claim.
    const pgm = within(focused).getByRole("button", { name: /Camera 1 PGM/i });
    expect(pgm).toHaveAttribute("aria-pressed", "true");
    await userEvent.click(pgm);
    expect(pgm).toHaveAttribute("aria-pressed", "false");
    expect(within(focused).getByTestId("audio-strip-status")).toHaveTextContent(/off PGM/);
    expect(lastPut).toBeUndefined();
  });

  it("stages a tap nudge in the draft without any request, and never clamps it silently", async () => {
    renderAudio();
    const { focused } = await console();
    await userEvent.click(
      within(focused).getByRole("button", { name: /Camera 1 gain −1 dB/i }),
    );
    const readout = within(focused).getByTestId("audio-strip-readout");
    expect(readout).toHaveTextContent(/Draft -4 dB/s);
    expect(readout).toHaveTextContent(/Saved -3 dB/s);
    expect(within(focused).getByTestId("audio-fader")).toHaveAttribute(
      "aria-valuetext",
      "draft -4 dB, saved -3 dB",
    );
    expect(lastPut).toBeUndefined();
    // The page-level draft banner says exactly what is pending.
    expect(screen.getByTestId("audio-draft-summary")).toHaveTextContent(/channels changed: 1/i);
    expect(screen.getByTestId("audio-draft-summary")).toHaveTextContent(/nothing reaches the bus until you save/i);
    // +6 from the staged -4 dB is +2 dB — beyond the fader's top, but still
    // an exact, legal draft: the fader pins at 0 while the readout carries
    // the truth (never a silent clamp).
    await userEvent.click(
      within(focused).getByRole("button", { name: /Camera 1 gain \+6 dB/i }),
    );
    expect(within(focused).getByTestId("audio-strip-readout")).toHaveTextContent(/Draft 2 dB/s);
    // The fader PINS at its 0 dB top while the readout carries the exact +2.
    expect(within(focused).getByTestId("audio-fader")).toHaveAttribute("aria-valuenow", "0");
    expect(lastPut).toBeUndefined();
  });

  it("stages fader drags and PageUp/PageDown keys, and Escape reverts the draft gain to the saved one", async () => {
    renderAudio();
    const { focused } = await console();
    const fader = within(focused).getByTestId("audio-fader");
    // A drag (pointer events) stages the draft; nothing is sent. The fader
    // track is 240 px tall in a real browser; jsdom has no layout, so stub it.
    // -12 dB = 80% up the 192 px knob-centre travel => y = 24 + 192 * 0.2.
    vi.spyOn(fader, "getBoundingClientRect").mockReturnValue(new DOMRect(0, 0, 56, 240));
    fireEvent.pointerDown(fader, { pointerId: 1, clientY: 24 + 192 * 0.2, button: 0 });
    fireEvent.pointerUp(fader, { pointerId: 1, clientY: 24 + 192 * 0.2 });
    expect(within(focused).getByTestId("audio-strip-readout")).toHaveTextContent(/Draft -12 dB/s);
    expect(within(focused).getByTestId("audio-strip-readout")).toHaveTextContent(/Saved -3 dB/s);
    expect(lastPut).toBeUndefined();
    // PageUp/PageDown step by ±6 dB, staged like the nudges.
    fireEvent.keyDown(fader, { key: "PageDown" });
    expect(within(focused).getByTestId("audio-strip-readout")).toHaveTextContent(/Draft -18 dB/s);
    fireEvent.keyDown(fader, { key: "PageUp" });
    expect(within(focused).getByTestId("audio-strip-readout")).toHaveTextContent(/Draft -12 dB/s);
    expect(lastPut).toBeUndefined();
    // Escape reverts the DRAFT gain to the saved one (a local, reversible
    // draft edit — never an on-air event undo).
    fireEvent.keyDown(fader, { key: "Escape" });
    expect(within(focused).getByTestId("audio-strip-readout")).toHaveTextContent(/Draft -3 dB/s);
    expect(within(focused).getByTestId("audio-strip-readout")).toHaveTextContent(/Saved -3 dB/s);
    expect(lastPut).toBeUndefined();
  });

  it("two bank faders driven by interleaved pointers in the same frame both keep their draft (no lost update)", async () => {
    renderAudio();
    await console();
    await userEvent.click(screen.getByTestId("audio-add-route"));
    const bank = screen.getByTestId("audio-bank");
    const faders = within(bank).getAllByTestId("audio-fader");
    const [a, b] = [required(faders[0], "fader 1"), required(faders[1], "fader 2")];
    for (const f of [a, b]) vi.spyOn(f, "getBoundingClientRect").mockReturnValue(new DOMRect(0, 0, 56, 240));
    const pe = (type: string, id: number, y: number): PointerEvent => {
      const e = new MouseEvent(type, { bubbles: true, cancelable: true, clientY: y, button: 0 });
      Object.defineProperty(e, "pointerId", { value: id });
      Object.defineProperty(e, "pointerType", { value: "touch" });
      return e as unknown as PointerEvent;
    };
    const y = (db: number): number => 24 + 192 * (1 - (db + 60) / 60);
    // Dispatch both moves back to back, with no React flush between them.
    fireEvent(a, pe("pointerdown", 1, y(Number(a.getAttribute("aria-valuenow")))));
    fireEvent(b, pe("pointerdown", 2, y(Number(b.getAttribute("aria-valuenow")))));
    fireEvent(a, pe("pointermove", 1, y(-20)));
    fireEvent(b, pe("pointermove", 2, y(-40)));
    expect(a).toHaveAttribute("aria-valuenow", "-20");
    expect(b).toHaveAttribute("aria-valuenow", "-40");
    expect(lastPut).toBeUndefined();
  });

  it("keeps strip identity and focus stable while the draft changes (stable keys)", async () => {
    renderAudio();
    const { focused } = await console();
    const fader = within(focused).getByTestId("audio-fader");
    const nudge = required(within(focused).getAllByTestId("audio-nudge")[3], "the −6 nudge");
    await userEvent.click(nudge);
    // Same DOM nodes across the re-render: the strip never remounts.
    expect(within(focused).getByTestId("audio-fader")).toBe(fader);
    expect(document.activeElement).toBe(nudge);
    // A second nudge keeps both properties.
    await userEvent.click(nudge);
    expect(within(focused).getByTestId("audio-fader")).toBe(fader);
    expect(within(focused).getByTestId("audio-strip-readout")).toHaveTextContent(/Draft -15 dB/s);
  });

  it("rides a channel from the whole-mix overview: tap a row, the focused strip follows (touch selection)", async () => {
    server.use(
      http.get("*/api/v1/audio-routing", () =>
        HttpResponse.json(ROUTING_TWO, { headers: { ETag: 'W/"7"' } }),
      ),
    );
    renderAudio();
    const { overview, focused, picker } = await console();
    expect(within(overview).getAllByTestId("audio-overview-row")).toHaveLength(2);
    // Camera 2 is off the mix and carries no track — the overview says so.
    const cam2 = within(overview).getByRole("button", { name: /Ride Camera 2/i });
    expect(within(cam2).getByText(/off PGM/i)).toBeInTheDocument();
    await userEvent.click(cam2);
    // The focused strip + picker follow the tapped row.
    expect(within(focused).getByTestId("audio-strip-name")).toHaveTextContent("Camera 2");
    const chips = within(picker).getAllByTestId("audio-strip-chip");
    expect(required(chips[0], "the first chip")).toHaveAttribute("aria-pressed", "false");
    expect(required(chips[1], "the second chip")).toHaveAttribute("aria-pressed", "true");
    // Nudges act on the ridden channel, and the nudge key keeps focus
    // across the re-render (no remount).
    await userEvent.click(
      within(focused).getByRole("button", { name: /Camera 2 gain −6 dB/i }),
    );
    expect(within(focused).getByTestId("audio-strip-readout")).toHaveTextContent(/Draft -6 dB/s);
    expect(within(focused).getByTestId("audio-strip-readout")).toHaveTextContent(/Saved 0 dB/s);
    expect(document.activeElement).toBe(within(focused).getAllByTestId("audio-nudge")[3]);
    expect(lastPut).toBeUndefined();
  });

  it("adds and removes route rows from the console", async () => {
    renderAudio();
    const { overview, focused, bank } = await console();
    await screen.findByLabelText(/sample rate/i);
    await userEvent.click(screen.getByTestId("audio-add-route"));
    expect(within(overview).getAllByTestId("audio-overview-row")).toHaveLength(2);
    expect(within(bank).getAllByTestId("audio-strip")).toHaveLength(2);
    // The new route is explicitly new — nothing saved to compare against.
    const newStrip = required(within(bank).getAllByTestId("audio-strip")[1], "the second bank strip");
    expect(within(newStrip).getByTestId("audio-strip-readout")).toHaveTextContent(/not saved yet/i);
    // Each strip carries an accessible remove control inside its Details;
    // ride the new route, remove it there, and the console is back to one
    // channel — the original one.
    await userEvent.click(
      required(within(overview).getAllByTestId("audio-overview-row")[1], "the second overview row"),
    );
    await userEvent.click(within(focused).getByTestId("audio-remove-route"));
    expect(within(overview).getAllByTestId("audio-overview-row")).toHaveLength(1);
    expect(within(bank).getAllByTestId("audio-strip")).toHaveLength(1);
    expect(required(within(bank).getAllByTestId("audio-strip")[0], "the first bank strip")).toHaveTextContent(/Camera 1/);
  });

  it("renders an inline error instead of saving an invalid form", async () => {
    renderAudio();
    const rate = await screen.findByLabelText(/sample rate/i);
    await userEvent.clear(rate);
    await userEvent.click(screen.getByTestId("audio-save"));
    expect(await screen.findByText(/required/i)).toBeInTheDocument();
    expect(rate).toHaveAttribute("aria-invalid", "true");
    expect(lastPut).toBeUndefined();
    // The Bus-setup disclosure opens itself so the error is not hidden.
    expect(screen.getByTestId("audio-bus-setup")).toHaveAttribute("open");
  });

  it("selects the first invalid non-selected route and shows an alert summary (focused strip)", async () => {
    const bad = { ...ROUTING_TWO, routing: { ...ROUTING_TWO.routing, routes: [
      ROUTING.routing.routes[0],
      { ...ROUTING_TWO.routing.routes[1], input_id: "" },
    ] } };
    server.use(http.get("*/api/v1/audio-routing", () => HttpResponse.json(bad, { headers: { ETag: 'W/"7"' } })));
    renderAudio();
    const { picker } = await console();
    expect(within(picker).getAllByTestId("audio-strip-chip")[0]).toHaveAttribute("aria-pressed", "true");
    await userEvent.click(screen.getByTestId("audio-save"));
    const summary = await screen.findByTestId("audio-validation-summary");
    expect(summary).toHaveAttribute("role", "alert");
    expect(within(summary).getByRole("button", { name: /Route 2/ })).toBeInTheDocument();
    expect(within(picker).getAllByTestId("audio-strip-chip")[1]).toHaveAttribute("aria-pressed", "true");
    expect(summary).toHaveFocus();
    expect(lastPut).toBeUndefined();
  });

  it("expands a collapsed desktop bank on invalid Save so the erroneous strip is visible", async () => {
    const bad = { ...ROUTING_TWO, routing: { ...ROUTING_TWO.routing, routes: [
      ROUTING.routing.routes[0],
      { ...ROUTING_TWO.routing.routes[1], input_id: "" },
    ] } };
    server.use(http.get("*/api/v1/audio-routing", () => HttpResponse.json(bad, { headers: { ETag: 'W/"7"' } })));
    renderAudio();
    const { bank } = await console();
    await userEvent.click(within(bank).getByRole("button", { name: /Show selected channel/i }));
    expect(within(bank).getByRole("button", { name: /Show all channels/i })).toHaveAttribute("aria-expanded", "false");
    await userEvent.click(screen.getByTestId("audio-save"));
    expect(await screen.findByTestId("audio-validation-summary")).toBeInTheDocument();
    expect(within(bank).getByRole("button", { name: /Show selected channel/i })).toHaveAttribute("aria-expanded", "true");
  });

  it("saves the whole document with PUT + If-Match", async () => {
    renderAudio();
    const rate = await screen.findByLabelText(/sample rate/i);
    await userEvent.clear(rate);
    await userEvent.type(rate, "44100");
    await userEvent.click(screen.getByTestId("audio-save"));
    // The save confirmation lands in the page's live status region (not a
    // toast — the region is announced by AT and visible next to the button).
    const status = screen.getByRole("status");
    await waitFor(() => {
      expect(status).toHaveTextContent(/stored, not applied live: this change needs a restart/i);
    });
    expect(lastPut).toBeDefined();
    expect(lastPut?.ifMatch).toBe('W/"7"');
    expect(lastPut?.body).toMatchObject({
      sample_rate_hz: 44100,
      routes: [
        {
          input_id: "cam-1",
          channels: { kind: "stereo" },
          target_track: "cam1-clean",
          include_in_program_bus: true,
          gain_db: -3,
          mute: false,
        },
      ],
    });
  });

  it("saves again with the refreshed ETag from the previous save", async () => {
    let revision = 7;
    server.use(
      http.get("*/api/v1/audio-routing", () => HttpResponse.json(ROUTING, { headers: { ETag: `W/"${String(revision)}"` } })),
      http.put("*/api/v1/audio-routing", async ({ request }) => {
        lastPut = { body: await request.json(), ifMatch: request.headers.get("if-match") };
        revision++;
        return HttpResponse.json(ROUTING, { headers: { ETag: `W/"${String(revision)}"`, "X-Multiview-Apply": "restart" } });
      }),
    );
    renderAudio();
    await screen.findByLabelText(/sample rate/i);
    await userEvent.click(screen.getByTestId("audio-save"));
    const status = screen.getByRole("status");
    await waitFor(() => expect(status).toHaveTextContent(/stored, not applied live/i));
    await userEvent.click(screen.getByTestId("audio-save"));
    await waitFor(() => {
      expect(lastPut?.ifMatch).toBe('W/"8"');
    });
  });

  it("adopts the canonical successful response as both saved baseline and clean draft", async () => {
    let revision = 7;
    let state = ROUTING;
    server.use(
      http.get("*/api/v1/audio-routing", () => HttpResponse.json(state, { headers: { ETag: `W/"${String(revision)}"` } })),
      http.put("*/api/v1/audio-routing", async ({ request }) => {
        lastPut = { body: await request.json(), ifMatch: request.headers.get("if-match") };
        revision++;
        state = { ...ROUTING, routing: { ...ROUTING.routing, routes: [{ ...required(ROUTING.routing.routes[0], "canonical route"), gain_db: -6, title: "Normalized by server" }] } };
        return HttpResponse.json(state, { headers: { ETag: `W/"${String(revision)}"`, "X-Multiview-Apply": "partial" } });
      }),
    );
    renderAudio();
    const focused = (await console()).focused;
    await userEvent.click(within(focused).getByRole("button", { name: /Camera 1 gain \+1 dB/i }));
    await userEvent.click(screen.getByTestId("audio-save"));
    await waitFor(() => expect(within(focused).getByTestId("audio-strip-readout")).toHaveTextContent(/Draft -6 dB.*Saved -6 dB/s));
    expect(screen.queryByTestId("audio-draft-summary")).not.toBeInTheDocument();
    expect(within(focused).queryByTestId("audio-strip-undo")).not.toBeInTheDocument();
    await userEvent.click(within(focused).getByRole("button", { name: /Camera 1 gain \+1 dB/i }));
    await userEvent.click(within(focused).getByTestId("audio-strip-undo"));
    expect(within(focused).getByTestId("audio-strip-readout")).toHaveTextContent(/Draft -6 dB.*Saved -6 dB/s);
    expect(lastPut?.ifMatch).toBe('W/"7"');
  });

  it("keeps invalid exact gain visible instead of nudging it into zero", async () => {
    renderAudio();
    const focused = (await console()).focused;
    await userEvent.click(within(focused).getByTestId("audio-strip-details").querySelector("summary")!);
    const gain = within(focused).getByLabelText(/Gain in dB/i);
    await userEvent.clear(gain);
    await userEvent.type(gain, "-");
    expect(gain).toHaveValue("-");
    expect(within(focused).getByTestId("audio-strip-readout")).toHaveTextContent(/invalid gain/i);
    for (const button of within(focused).getAllByTestId("audio-nudge")) expect(button).toBeDisabled();
    // A custom slider is disabled via aria-disabled (no native disabled attr).
    expect(within(focused).getByTestId("audio-fader")).toHaveAttribute("aria-disabled", "true");
    await userEvent.click(screen.getByTestId("audio-save"));
    expect(lastPut).toBeUndefined();
    expect(gain).toHaveValue("-");
  });

  it("preserves row association after removing the first route, including duplicate input ids", async () => {
    const three = { ...ROUTING_TWO, routing: { ...ROUTING_TWO.routing, routes: [
      ROUTING.routing.routes[0],
      { ...ROUTING.routing.routes[0], gain_db: -9, target_track: "duplicate-clean", title: "Duplicate camera" },
      { ...ROUTING.routing.routes[0], input_id: "cam-2", target_track: "third-clean", gain_db: -12, title: "Third camera" },
    ] } };
    server.use(
      http.get("*/api/v1/audio-routing", () => HttpResponse.json(three, { headers: { ETag: 'W/"7"' } })),
      http.put("*/api/v1/audio-routing", async ({ request }) => {
        lastPut = { body: await request.json(), ifMatch: request.headers.get("if-match") };
        return HttpResponse.json(three, { headers: { ETag: 'W/"8"', "X-Multiview-Apply": "partial" } });
      }),
    );
    renderAudio();
    const focused = (await console()).focused;
    await userEvent.click(within(focused).getByRole("button", { name: /remove route/i }));
    expect(within(focused).getByTestId("audio-strip-readout")).toHaveTextContent(/Draft -9 dB.*Saved -9 dB/s);
    await userEvent.click(within(focused).getByRole("button", { name: /MUTE/i }));
    await userEvent.click(within(focused).getByTestId("audio-strip-undo"));
    expect(within(focused).getByTestId("audio-strip-readout")).toHaveTextContent(/Draft -9 dB.*Saved -9 dB/s);
    await userEvent.click(screen.getByTestId("audio-save"));
    expect(screen.getByTestId("audio-save")).not.toBeDisabled();
    expect(screen.queryAllByRole("alert").map((node) => node.textContent)).toEqual([]);
    await waitFor(() => { expect(lastPut).toBeDefined(); });
    expect(lastPut?.body).toMatchObject({ routes: [
      { input_id: "cam-1", gain_db: -9, title: "Duplicate camera" },
      { input_id: "cam-2", gain_db: -12, title: "Third camera" },
    ] });
  });

  it("blocks stale whole-document writes after a 412 until explicit reload", async () => {
    let revision = 7;
    let seenConflict = false;
    let state = ROUTING;
    const writes: { body: unknown; ifMatch: string | null }[] = [];
    server.use(
      http.get("*/api/v1/audio-routing", () => HttpResponse.json(state, { headers: { ETag: `W/"${String(revision)}"` } })),
      http.put("*/api/v1/audio-routing", async ({ request }) => {
        const write = { body: await request.json(), ifMatch: request.headers.get("if-match") };
        writes.push(write);
        if (write.ifMatch !== `W/"${String(revision)}"`) {
          seenConflict = true;
          return new HttpResponse(null, { status: 412 });
        }
        revision++;
        return HttpResponse.json(state, { headers: { ETag: `W/"${String(revision)}"`, "X-Multiview-Apply": "live" } });
      }),
    );
    renderAudio();
    const focused = (await console()).focused;
    await userEvent.click(within(focused).getByRole("button", { name: /Camera 1 gain \+1 dB/i }));
    revision = 8;
    expect(seenConflict).toBe(false);
    state = { ...ROUTING, routing: { ...ROUTING.routing, routes: [{ ...required(ROUTING.routing.routes[0], "remote route"), title: "Remote operator" }] } };
    expect(screen.getByTestId("audio-save")).not.toBeDisabled();
    await userEvent.click(screen.getByTestId("audio-save"));
    await waitFor(() => { expect(writes).toHaveLength(1); }, { timeout: 1500 });
    expect(screen.queryAllByRole("alert").map((node) => node.textContent)).not.toContain("This channel has draft errors — they are marked inside Details.");
    await screen.findByRole("button", { name: /reload latest routing/i });
    expect(within(focused).getByTestId("audio-strip-readout")).toHaveTextContent(/Draft -2 dB.*Saved -3 dB/s);
    expect(screen.getByTestId("audio-save")).toBeDisabled();
    expect(writes[0]?.ifMatch).toBe('W/"7"');
    await userEvent.click(screen.getByRole("button", { name: /reload latest routing/i }));
    await userEvent.click(within(focused).getByRole("button", { name: /Camera 1 gain \+1 dB/i }));
    await userEvent.click(screen.getByTestId("audio-save"));
    await waitFor(() => { expect(writes).toHaveLength(2); });
    expect(writes[1]).toMatchObject({ ifMatch: 'W/"8"', body: { routes: [{ title: "Remote operator", gain_db: -2 }] } });
  });

  it("reports a live-applied save without asking for a restart", async () => {
    server.use(
      http.put("*/api/v1/audio-routing", () =>
        HttpResponse.json(
          { ...ROUTING, configured: true },
          { headers: { ETag: 'W/"8"', "X-Multiview-Apply": "live" } },
        ),
      ),
    );
    renderAudio();
    await screen.findByLabelText(/sample rate/i);
    await userEvent.click(screen.getByTestId("audio-save"));
    const status = screen.getByRole("status");
    await waitFor(() => {
      expect(status).toHaveTextContent(/applied live/i);
    });
    expect(status).not.toHaveTextContent(/restart/i);
  });

  it("reports partial apply without saying the running program mix is unchanged", async () => {
    server.use(
      http.put("*/api/v1/audio-routing", () =>
        HttpResponse.json(
          { ...ROUTING, configured: true },
          { headers: { ETag: 'W/"8"', "X-Multiview-Apply": "partial" } },
        ),
      ),
    );
    renderAudio();
    await screen.findByLabelText(/sample rate/i);
    await userEvent.click(screen.getByTestId("audio-save"));
    const status = screen.getByRole("status");
    await waitFor(() => expect(status).toHaveTextContent(/program mix.*apply live/i));
    expect(status).toHaveTextContent(/tracks remain pending/i);
    expect(status).not.toHaveTextContent(/program mix is unchanged/i);
  });

  it("surfaces a failed save as an error, never a fake success", async () => {
    server.use(
      http.put("*/api/v1/audio-routing", () =>
        HttpResponse.json(
          { title: "Precondition Failed", status: 412, detail: "the routing document changed on the server" },
          { status: 412 },
        ),
      ),
    );
    renderAudio();
    await screen.findByLabelText(/sample rate/i);
    await userEvent.click(screen.getByTestId("audio-save"));
    const status = screen.getByRole("status");
    await waitFor(() =>
      expect(status).toHaveTextContent(/could not save the audio routing: precondition failed/i),
    );
    expect(screen.queryByTestId("audio-outputs-updated-note")).not.toBeInTheDocument();
  });

  it("warns inline, BEFORE saving, that an output carrying the renamed track will be updated", async () => {
    renderAudio();
    const { focused } = await console();
    const trackField = within(focused).getByLabelText(/discrete track name/i);
    await userEvent.clear(trackField);
    await userEvent.type(trackField, "us-cnn");
    // The warning appears before any save attempt.
    const alert = await screen.findByRole("alert", {
      name: /this save will update the following outputs/i,
    });
    expect(alert).toHaveTextContent(/HLS/);
    expect(alert).toHaveTextContent(/cam1-clean/);
    expect(alert).toHaveTextContent(/us-cnn/);
    expect(lastPut).toBeUndefined();
  });

  it("shows no pending-impact warning when the edit does not touch a selected track", async () => {
    renderAudio();
    const rate = await screen.findByLabelText(/sample rate/i);
    await userEvent.clear(rate);
    await userEvent.type(rate, "44100");
    expect(screen.queryByTestId("audio-pending-impact")).not.toBeInTheDocument();
  });

  it("shows the server's outputs_updated report as a success note after saving", async () => {
    server.use(
      http.put("*/api/v1/audio-routing", async ({ request }) => {
        lastPut = {
          body: await request.json(),
          ifMatch: request.headers.get("if-match"),
        };
        return HttpResponse.json(
          {
            ...ROUTING,
            configured: true,
            outputs_updated: [
              {
                output: "hls-out",
                removed: [],
                renamed: [{ from: "cam1-clean", to: "us-cnn" }],
              },
            ],
          },
          { headers: { ETag: 'W/"8"', "X-Multiview-Apply": "restart" } },
        );
      }),
    );
    renderAudio();
    const { focused } = await console();
    const trackField = within(focused).getByLabelText(/discrete track name/i);
    await userEvent.clear(trackField);
    await userEvent.type(trackField, "us-cnn");
    await userEvent.click(screen.getByTestId("audio-save"));
    const note = await screen.findByTestId("audio-outputs-updated-note");
    expect(note).toHaveTextContent(/hls-out/);
    expect(note).toHaveTextContent(/cam1-clean.*us-cnn/);
  });

  it("shows the program meters and loudness as the real telemetry sections, and no strip fabricates a level", async () => {
    renderAudio();
    const { focused } = await console();
    // The ACTUAL meters (engine telemetry), each exactly once.
    expect(screen.getByRole("region", { name: /program meters/i })).toBeInTheDocument();
    expect(screen.getByRole("region", { name: /program loudness/i })).toBeInTheDocument();
    // With no realtime telemetry in the test environment the strips show an
    // absent reading — never a number derived from gain, mute or the draft.
    expect(within(focused).getByTestId("audio-in-peak").textContent).toBe("—");
  });

  it("offers no fake PFL/listen key, and states plainly that it is unavailable", async () => {
    renderAudio();
    const { focused } = await console();
    expect(within(focused).queryByRole("button", { name: /listen|PFL|solo/i })).not.toBeInTheDocument();
    expect(screen.getByTestId("audio-pfl-note")).toHaveTextContent(/pre-fade listen.*not available/i);
  });
});
