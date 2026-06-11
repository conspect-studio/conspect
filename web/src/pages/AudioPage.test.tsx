// MSW test for the Audio routing page: loads the singleton document
// (GET /api/v1/audio-routing), renders one labelled table row per route, lists
// the selectable tracks ("prog" + declared), validates inline, and saves the
// whole document with PUT + If-Match.
import { afterAll, afterEach, beforeAll, describe, expect, it } from "vitest";
import { screen, waitFor, within } from "@testing-library/react";
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

/** The last PUT body + If-Match header the mock server saw. */
let lastPut: { body: unknown; ifMatch: string | null } | undefined;

const server = setupServer(
  http.get("*/api/v1/sources", () => HttpResponse.json(SOURCES)),
  http.get("*/api/v1/audio-routing", () =>
    HttpResponse.json(ROUTING, { headers: { ETag: 'W/"7"' } }),
  ),
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

function renderAudio(): void {
  renderWithProviders(
    <MemoryRouter>
      <AudioPage />
    </MemoryRouter>,
  );
}

describe("AudioPage", () => {
  it("loads the document into the form and lists the selectable tracks", async () => {
    renderAudio();
    // The sample rate field prefills from the document.
    expect(await screen.findByLabelText(/sample rate/i)).toHaveValue(48000);
    // One row per route, inside a labelled table.
    const table = screen.getByRole("table", { name: /audio route/i });
    expect(within(table).getByDisplayValue("cam1-clean")).toBeInTheDocument();
    // The resulting selectable tracks are listed: the program bus + declared.
    const tracks = screen.getByTestId("audio-tracks-list");
    expect(within(tracks).getByText("prog")).toBeInTheDocument();
    expect(within(tracks).getByText("cam1-clean")).toBeInTheDocument();
  });

  it("shows the apply-semantics callout", async () => {
    renderAudio();
    expect(await screen.findByRole("note")).toHaveTextContent(
      /config export.*restart|exporting the configuration/i,
    );
  });

  it("adds and removes route rows from the keyboard", async () => {
    renderAudio();
    await screen.findByLabelText(/sample rate/i);
    const addButton = screen.getByTestId("audio-add-route");
    await userEvent.click(addButton);
    expect(screen.getAllByTestId("audio-route-row")).toHaveLength(2);
    // Each row carries an accessible remove control; removing the new row
    // returns to one.
    const rows = screen.getAllByTestId("audio-route-row");
    const lastRow = rows[1];
    expect(lastRow).toBeDefined();
    if (lastRow === undefined) {
      throw new Error("expected a second route row");
    }
    await userEvent.click(within(lastRow).getByRole("button", { name: /remove/i }));
    expect(screen.getAllByTestId("audio-route-row")).toHaveLength(1);
  });

  it("renders an inline error instead of saving an invalid form", async () => {
    renderAudio();
    const rate = await screen.findByLabelText(/sample rate/i);
    await userEvent.clear(rate);
    await userEvent.click(screen.getByTestId("audio-save"));
    expect(await screen.findByText(/required/i)).toBeInTheDocument();
    expect(rate).toHaveAttribute("aria-invalid", "true");
    expect(lastPut).toBeUndefined();
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
      expect(status).toHaveTextContent(/stored\. it goes live via config export/i);
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
});
