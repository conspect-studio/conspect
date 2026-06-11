import { test, expect } from "@playwright/test";

// Drives the REAL built SPA and asserts the SA-0 global health banner (ADR-0035)
// renders an actionable warning — severity + message + the `graphics`/libvulkan
// remediation — once a `health.warning.raised` event streams over the WebSocket,
// and renders NOTHING on a clean host (no false alarm). The WS is mocked with
// `page.routeWebSocket` (the handler never connects to a server, so the socket is
// fully mocked); auth is reported "off" so the app renders without a login gate;
// REST is stubbed empty so unrelated queries resolve.

const WARNING = {
  code: "gpu-present-no-vulkan-adapter",
  severity: "warning",
  subsystem: "compositor",
  message:
    "NVIDIA GeForce RTX 4060 detected, but GPU compositing is UNAVAILABLE (no Vulkan adapter); compositing fell back to the CPU reference (high CPU, GPU idle).",
  remediation:
    "Grant the container the `graphics` driver capability (set NVIDIA_DRIVER_CAPABILITIES to include `graphics`, or `all`) and install the Vulkan loader (`libvulkan1`) + the GPU's ICD.",
  since: 1_700_000_000_000_000_000,
  active: true,
};

/** Build one `health.warning.raised` Envelope frame on the `alerts` lane. */
function warningFrame(seq: number): string {
  return JSON.stringify({
    v: 1,
    t: "health.warning.raised",
    topic: "alerts",
    seq,
    ts: seq * 1_000_000,
    data: WARNING,
  });
}

test.beforeEach(async ({ page }) => {
  // Auth off; everything else returns an empty JSON body.
  await page.route("**/api/v1/**", (route) => route.fulfill({ json: [] }));
  await page.route("**/api/v1/auth/status*", (route) =>
    route.fulfill({ json: { auth_required: false, authenticated: true } }),
  );
});

test("the banner surfaces a GPU-fallback warning with its remediation", async ({
  page,
}) => {
  await page.routeWebSocket("**/api/v1/ws*", (ws) => {
    ws.onMessage(() => {
      // Ignore client→server subscribe/heartbeat traffic; keep the socket open.
    });
    ws.send(warningFrame(1));
  });

  await page.goto("/");

  // The banner is an announced alert region carrying the warning.
  const banner = page.getByRole("alert");
  await expect(banner).toBeVisible();

  // Severity is conveyed as TEXT (not colour alone — WCAG 1.4.1).
  await expect(banner.getByText("Warning")).toBeVisible();
  // The message names the detected GPU and the CPU fallback.
  await expect(
    banner.getByText(/GPU compositing is UNAVAILABLE/),
  ).toBeVisible();
  // The actionable remediation carries the concrete fix.
  await expect(banner.getByText(/NVIDIA_DRIVER_CAPABILITIES/)).toBeVisible();
  await expect(banner.getByText(/libvulkan1/)).toBeVisible();
  // The stable code is shown for reference/search.
  await expect(banner.getByText("gpu-present-no-vulkan-adapter")).toBeVisible();
});

test("the banner renders nothing on a clean host (no warnings)", async ({
  page,
}) => {
  await page.routeWebSocket("**/api/v1/ws*", (ws) => {
    ws.onMessage(() => {
      // No health-warning frames are pushed → a clean host.
    });
  });

  await page.goto("/");

  // The app shell is up (the nav rail renders) but no alert banner exists.
  await expect(page.getByRole("alert")).toHaveCount(0);
});
