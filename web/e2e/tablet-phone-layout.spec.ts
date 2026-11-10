import { test, expect } from "@playwright/test";

// Layout guards in a REAL browser (vitest runs with css:false, so stylesheet
// regressions are invisible there). Two defects were found by looking at real
// screenshots, and both must stay fixed:
//   1. Desk on a phone hid the only "meters unavailable" statement.
//   2. Audio on a tablet overlapped the fader scale labels with the nudge
//      column and clipped the strip bank.
// Assertions are structural (testids/roles) and measured, never copy.

const SOURCES = ["CAM 1", "CAM 2", "CAM 3", "CAM 4", "VT 1", "VT 2", "GFX", "BARS"].map(
  (name, i) => ({
    id: `s${String(i)}`,
    name,
    body: { id: `s${String(i)}`, kind: "bars" },
  }),
);

const ROUTING = {
  configured: true,
  routing: {
    sample_rate_hz: 48000,
    routes: SOURCES.map((s) => ({
      input_id: s.id,
      channels: { kind: "stereo" },
      include_in_program_bus: true,
      gain_db: -6,
      mute: false,
    })),
  },
  selectable_tracks: ["prog"],
};

test.beforeEach(async ({ page }) => {
  // Catch-all FIRST so the specific routes registered after it take precedence.
  await page.route("**/api/v1/**", (route) => route.fulfill({ json: [] }));
  await page.route("**/api/v1/auth/status*", (route) =>
    route.fulfill({ json: { auth_required: false, authenticated: true } }),
  );
  await page.route("**/api/v1/sources", (route) => route.fulfill({ json: SOURCES }));
  await page.route("**/api/v1/audio-routing", (route) =>
    route.fulfill({ json: ROUTING, headers: { ETag: 'W/"1"' } }),
  );
});

for (const width of [390, 360]) {
  test(`desk shows the meters-unavailable statement on a ${String(width)}px phone`, async ({
    page,
  }) => {
    await page.setViewportSize({ width, height: 844 });
    await page.goto("/");
    const note = page.getByTestId("desk-meters-unavailable");
    await expect(note).toBeVisible();
    const box = await note.boundingBox();
    expect(box).not.toBeNull();
    expect(box?.height ?? 0).toBeGreaterThan(0);
    expect(await note.evaluate((el) => getComputedStyle(el).display)).not.toBe("none");
  });
}

for (const [width, height] of [
  [1024, 768],
  [1280, 800],
] as const) {
  test(`audio ${String(width)}px: nudges never overlap the fader scale and the bank does not clip`, async ({
    page,
  }) => {
    await page.setViewportSize({ width, height });
    await page.goto("/audio");
    const strips = page.getByTestId("audio-strip");
    await expect(strips.first()).toBeVisible();
    const count = await strips.count();
    expect(count).toBeGreaterThan(0);

    const result = await strips.evaluateAll((els) => {
      const inter = (a: DOMRect, b: DOMRect): boolean =>
        a.left < b.right && a.right > b.left && a.top < b.bottom && a.bottom > b.top;
      const overlaps: number[] = [];
      const clipped: number[] = [];
      els.forEach((strip, i) => {
        const scale = strip.querySelector(".audio-fader-box[aria-hidden='true']");
        const nudges = strip.querySelector("[data-testid='audio-nudges']");
        if (scale && nudges && inter(scale.getBoundingClientRect(), nudges.getBoundingClientRect())) {
          overlaps.push(i);
        }
        const name = strip.querySelector("[data-testid='audio-strip-name']");
        if (name && name.scrollWidth > name.clientWidth) clipped.push(i);
      });
      const bank = els[0]?.parentElement;
      return {
        overlaps,
        clipped,
        bankFits: bank ? bank.scrollWidth <= bank.clientWidth : true,
        pageFits: document.documentElement.scrollWidth <= window.innerWidth,
      };
    });

    expect(result.overlaps, "nudge column intersects the fader scale").toEqual([]);
    expect(result.clipped, "strip name is clipped").toEqual([]);
    expect(result.bankFits, "strip bank scrolls horizontally").toBe(true);
    expect(result.pageFits, "page scrolls horizontally").toBe(true);
  });
}
