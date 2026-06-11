import { test, expect } from "@playwright/test";

// Guard for the Audio routing page in a REAL browser: the route renders the
// document into the table, rows add/remove without crashing the renderer
// (Radix Selects per row — the same effect class that OOM-ed ResourceTable),
// and Save issues a conditional PUT carrying the document `If-Match`.
// Assertions are structural (testids/roles), never freshly-added copy: new
// strings are not in the compiled catalog until the i18n lane runs
// `lingui extract`/`compile`.

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
        include_in_program_bus: true,
        gain_db: -3,
        mute: false,
      },
    ],
  },
  selectable_tracks: ["prog", "cam1-clean"],
};

test.beforeEach(async ({ page }) => {
  // Catch-all FIRST so the specific routes registered after it take precedence
  // (Playwright matches most-recently-added routes first).
  await page.route("**/api/v1/**", (route) => route.fulfill({ json: [] }));
  await page.route("**/api/v1/auth/status*", (route) =>
    route.fulfill({ json: { auth_required: false, authenticated: true } }),
  );
  await page.route("**/api/v1/sources", (route) => route.fulfill({ json: SOURCES }));
  await page.route("**/api/v1/audio-routing", (route) => {
    if (route.request().method() === "PUT") {
      return route.fulfill({
        json: ROUTING,
        headers: { ETag: 'W/"8"', "X-Multiview-Apply": "restart" },
      });
    }
    return route.fulfill({ json: ROUTING, headers: { ETag: 'W/"7"' } });
  });
});

test("the audio routing page edits rows and saves with If-Match without crashing", async ({
  page,
}) => {
  const crashed: string[] = [];
  page.on("crash", () => crashed.push("renderer crashed"));

  await page.goto("/audio");

  // The document loads into the form: one route row + the tracks list with
  // the program bus and the declared discrete track.
  await expect(page.getByTestId("audio-route-row")).toHaveCount(1);
  const tracks = page.getByTestId("audio-tracks-list");
  await expect(tracks).toContainText("prog");
  await expect(tracks).toContainText("cam1-clean");

  // Add a row (renders another set of Radix Selects over the table), then
  // remove it from the keyboard-reachable per-row control.
  await page.getByTestId("audio-add-route").click();
  await expect(page.getByTestId("audio-route-row")).toHaveCount(2);
  await page
    .getByTestId("audio-route-row")
    .nth(1)
    .getByTestId("audio-remove-route")
    .click();
  await expect(page.getByTestId("audio-route-row")).toHaveCount(1);

  // Save replaces the whole document conditionally.
  const putRequest = page.waitForRequest(
    (request) =>
      request.url().includes("/api/v1/audio-routing") && request.method() === "PUT",
  );
  await page.getByTestId("audio-save").click();
  const put = await putRequest;
  expect(put.headers()["if-match"]).toBe('W/"7"');
  const body: unknown = put.postDataJSON();
  expect(body).toMatchObject({ sample_rate_hz: 48000 });

  expect(crashed, crashed.join(",")).toHaveLength(0);
});
