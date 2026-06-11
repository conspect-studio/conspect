import { test, expect } from "@playwright/test";

// The grid layout editor in a REAL browser: open the seeded working layout (a
// `kind = "grid"` body, exactly what `multiview run` seeds), rename an area,
// and save — the PUT/POST body must carry the renamed area map with everything
// else preserved verbatim, and the renderer must not crash. The API is mocked
// via route interception; the read-only refusal this page used to show for
// grid bodies must never come back.
//
// NOTE: assertions use data-testids, not freshly-added UI strings — new
// strings are not in the compiled i18n catalog until the i18n lane runs
// `lingui extract`/`compile`, and the production build strips the source-text
// fallback.

const GRID_BODY = {
  canvas: {
    width: 1920,
    height: 1080,
    fps: "25/1",
    pixel_format: "nv12",
    background: "#101014",
    color: { profile: "sdr-bt709-limited" },
  },
  layout: {
    kind: "grid",
    columns: ["1fr", "1fr"],
    rows: ["1fr", "1fr"],
    gap: 4,
    areas: ["a b", "c d"],
  },
  cells: ["a", "b", "c", "d"].map((area) => ({
    id: `cell_${area}`,
    area,
    z: 0,
    fit: "contain",
    on_loss: { slate: "bars" },
    source: { input_id: `in_${area}` },
  })),
};

const LAYOUTS = [{ id: "working", name: "working", body: GRID_BODY }];

test.beforeEach(async ({ page }) => {
  await page.route("**/api/v1/**", (route) => route.fulfill({ json: [] }));
  await page.route("**/api/v1/auth/status*", (route) =>
    route.fulfill({ json: { auth_required: false, authenticated: true } }),
  );
  await page.route("**/api/v1/layouts", (route) => route.fulfill({ json: LAYOUTS }));
});

test("open the seeded grid layout, rename an area, and save the edited body", async ({
  page,
}) => {
  const crashed: string[] = [];
  page.on("crash", () => crashed.push("renderer crashed"));

  let savedBody: unknown;
  await page.route(/\/api\/v1\/layouts\/working$/, async (route) => {
    const method = route.request().method();
    if (method === "PUT" || method === "POST") {
      const payload = route.request().postDataJSON() as { body?: unknown };
      savedBody = payload.body;
      await route.fulfill({
        json: { id: "working", name: "working", body: payload.body },
        headers: { ETag: '"2"' },
      });
      return;
    }
    await route.fulfill({
      json: LAYOUTS[0],
      headers: { ETag: '"1"' },
    });
  });

  await page.goto("/layouts/working");

  // The grid editor renders (matrix + save), never a read-only refusal.
  await expect(page.getByTestId("area-matrix")).toBeVisible();
  await expect(page.getByText(/read-only/i)).toHaveCount(0);
  await expect(page.getByTestId("matrix-cell-0-0")).toHaveText("a");

  // Select area "a" and rename it to "hero".
  await page.getByTestId("area-chip-a").click();
  await page.getByTestId("rename-area-input").fill("hero");
  await page.getByTestId("rename-area").click();
  await expect(page.getByTestId("matrix-cell-0-0")).toHaveText("hero");

  // Save; the persisted body carries the rename and preserves everything else.
  await page.getByTestId("grid-save").click();
  await expect
    .poll(() => savedBody, { timeout: 5000 })
    .toEqual({
      ...GRID_BODY,
      layout: { ...GRID_BODY.layout, areas: ["hero b", "c d"] },
      cells: GRID_BODY.cells.map((cell) =>
        cell.area === "a" ? { ...cell, area: "hero" } : cell,
      ),
    });

  expect(crashed, crashed.join(",")).toHaveLength(0);
});

test("the keyboard matrix selection works in a real browser", async ({ page }) => {
  await page.goto("/layouts/working");
  await expect(page.getByTestId("area-matrix")).toBeVisible();

  // Arrow + Shift extends the gridcell selection.
  await page.getByTestId("matrix-cell-0-0").focus();
  await page.keyboard.press("Shift+ArrowRight");
  await expect(page.getByTestId("matrix-cell-0-0")).toHaveAttribute(
    "aria-selected",
    "true",
  );
  await expect(page.getByTestId("matrix-cell-0-1")).toHaveAttribute(
    "aria-selected",
    "true",
  );
  await expect(page.getByTestId("matrix-cell-1-1")).toHaveAttribute(
    "aria-selected",
    "false",
  );

  // The konva placement preview mounts without crashing.
  await page.getByTestId("grid-preview-tab").click();
  await expect(page.locator("canvas").first()).toBeVisible({ timeout: 10000 });
});
