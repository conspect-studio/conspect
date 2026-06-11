import { test, expect } from "@playwright/test";

// Regression guard for the renderer-OOM crash fixed in `ResourceTable`: opening a
// create / edit / delete Dialog over a POPULATED resource table must not drive
// `useReactTable` into an unbounded re-render loop. This only reproduces in a real
// browser, so it lives here (not in the jsdom suite). API is mocked via route
// interception; auth is reported "off" so the app renders without a login gate.

const SOURCES = [
  { id: "cam-1", name: "Camera 1", body: { id: "cam-1", kind: "rtsp", url: "rtsp://host/one" } },
  { id: "cam-2", name: "Camera 2", body: { id: "cam-2", kind: "bars" } },
];

const OUTPUTS = [
  {
    id: "out-hls",
    name: "Program HLS",
    body: { id: "out-hls", kind: "hls", path: "/hls/m", codec: "h264" },
  },
  {
    id: "out-rtsp",
    name: "RTSP mount",
    body: { id: "out-rtsp", kind: "rtsp_server", mount: "/multiview", codec: "h264" },
  },
];

const PROBES = [
  {
    id: "black-1",
    name: "Black on PGM",
    body: {
      id: "black-1",
      cell: "cell-a",
      kind: "black",
      luma_threshold: 16,
      dwell: { up_ms: 2000, down_ms: 1000 },
      severity: "Major",
      latched: false,
    },
  },
];

const LAYOUTS = [
  {
    id: "working",
    name: "working",
    body: {
      canvas: { width: 1920, height: 1080 },
      layout: { kind: "absolute" },
      cells: [{ id: "cell-a" }, { id: "cell-b" }],
    },
  },
];

test.beforeEach(async ({ page }) => {
  // Catch-all FIRST so the specific routes registered after it take precedence
  // (Playwright matches most-recently-added routes first).
  await page.route("**/api/v1/**", (route) => route.fulfill({ json: [] }));
  await page.route("**/api/v1/auth/status*", (route) =>
    route.fulfill({ json: { auth_required: false, authenticated: true } }),
  );
  await page.route("**/api/v1/sources", (route) => route.fulfill({ json: SOURCES }));
  await page.route(/\/api\/v1\/sources\/[^/?]+$/, (route) =>
    route.fulfill({ json: SOURCES[0], headers: { ETag: '"1"' } }),
  );
  await page.route("**/api/v1/outputs", (route) => route.fulfill({ json: OUTPUTS }));
  await page.route(/\/api\/v1\/outputs\/[^/?]+$/, (route) =>
    route.fulfill({ json: OUTPUTS[0], headers: { ETag: '"1"' } }),
  );
  await page.route("**/api/v1/probes", (route) => route.fulfill({ json: PROBES }));
  await page.route(/\/api\/v1\/probes\/[^/?]+$/, (route) =>
    route.fulfill({ json: PROBES[0], headers: { ETag: '"1"' } }),
  );
  await page.route("**/api/v1/layouts", (route) => route.fulfill({ json: LAYOUTS }));
});

test("create / edit / delete dialogs open without crashing the renderer", async ({ page }) => {
  const crashed: string[] = [];
  page.on("crash", () => crashed.push("renderer crashed"));

  await page.goto("/sources");
  await expect(page.getByText("Camera 1")).toBeVisible();

  // Create — opening this Dialog re-renders the page with the table mounted; the
  // old `data: [...rows]` drove an unbounded re-render loop that OOM-ed here.
  await page.getByRole("button", { name: "New source" }).click();
  await expect(page.getByRole("dialog")).toBeVisible({ timeout: 5000 });
  await page.keyboard.press("Escape");

  // Edit (loads the record via GET, prefills, opens the same Dialog).
  await page.getByRole("button", { name: "Edit source: Camera 1" }).click();
  await expect(page.getByRole("dialog")).toBeVisible({ timeout: 5000 });
  await page.keyboard.press("Escape");

  // Delete confirmation (a Dialog with no Select — also triggered the loop).
  await page.getByRole("button", { name: "Delete source: Camera 1" }).click();
  await expect(page.getByText("Delete this resource?")).toBeVisible({ timeout: 5000 });

  expect(crashed, crashed.join(",")).toHaveLength(0);
});

test("the kind-specific outputs dialog (selects + advanced disclosure) opens cleanly", async ({
  page,
}) => {
  const crashed: string[] = [];
  page.on("crash", () => crashed.push("renderer crashed"));

  await page.goto("/outputs");
  await expect(page.getByText("Program HLS")).toBeVisible();
  // Honesty markers render in the table (asserted structurally via testid:
  // freshly-added strings are not in the compiled catalog until the i18n lane
  // runs `lingui extract`/`compile`, so text matching would be brittle here).
  await expect(
    page.getByTestId("output-runnability").and(page.locator('[data-runnable="false"]')),
  ).toBeVisible();
  await expect(
    page.getByTestId("output-runnability").and(page.locator('[data-runnable="true"]')),
  ).toBeVisible();

  await page.getByRole("button", { name: "New output" }).click();
  await expect(page.getByRole("dialog")).toBeVisible({ timeout: 5000 });
  // The Advanced disclosure expands without looping the renderer.
  await page.getByTestId("advanced-toggle").click();
  await expect(page.getByTestId("advanced-section")).toHaveAttribute("open", "");
  await page.keyboard.press("Escape");

  // Edit prefills the kind-specific destination field.
  await page.getByRole("button", { name: "Edit output: Program HLS" }).click();
  await expect(page.getByRole("dialog")).toBeVisible({ timeout: 5000 });
  await expect(page.getByLabel("Output path")).toHaveValue("/hls/m");

  expect(crashed, crashed.join(",")).toHaveLength(0);
});

test("the probes dialog (cell select + zone + policy disclosure) opens cleanly", async ({
  page,
}) => {
  // Structural selectors (testids / element ids / row data) throughout:
  // freshly-added localized strings render as catalog hash ids in the built
  // bundle until the i18n lane runs `lingui extract`/`compile`, so text
  // matching would be brittle here (same note as the runnability markers).
  const crashed: string[] = [];
  page.on("crash", () => crashed.push("renderer crashed"));

  await page.goto("/probes");
  await expect(page.getByText("Black on PGM")).toBeVisible();

  // Create — Selects (cell/kind/severity) + the policy disclosure render over
  // the populated table without looping the renderer.
  await page.getByTestId("crud-new").click();
  await expect(page.getByRole("dialog")).toBeVisible({ timeout: 5000 });
  // The cell picker is fed from the mocked working layout's cells.
  await page.getByTestId("probe-cell-select").click();
  await expect(page.getByRole("option", { name: "cell-a" })).toBeVisible();
  await page.keyboard.press("Escape"); // close the select
  await page.getByTestId("advanced-toggle").click();
  await expect(page.getByTestId("advanced-section")).toHaveAttribute("open", "");
  await page.keyboard.press("Escape");

  // Edit prefills the kind-specific threshold field (the single mocked row).
  await page.getByTestId("row-edit").click();
  await expect(page.getByRole("dialog")).toBeVisible({ timeout: 5000 });
  await expect(page.locator("#probe-luma")).toHaveValue("16");
  await page.keyboard.press("Escape");

  // Delete confirmation ("Delete this resource?" is in the compiled catalog).
  await page.getByTestId("row-delete").click();
  await expect(page.getByText("Delete this resource?")).toBeVisible({ timeout: 5000 });

  expect(crashed, crashed.join(",")).toHaveLength(0);
});
