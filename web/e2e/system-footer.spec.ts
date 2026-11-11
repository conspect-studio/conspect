import { test, expect } from "@playwright/test";

// NOTE: the approved six-screen console has no persistent footer, so the same
// live ours-vs-total telemetry is asserted on the System page (/system).
// Drives the REAL built SPA and asserts the live system metrics render
// ALL GPUs with an OURS-vs-TOTAL breakdown once `system.metrics` events stream
// over the WebSocket. The box has MULTIPLE GPUs shared with co-tenant processes,
// so the device-wide totals are not all ours; the wire now carries our-process
// `self_*` shares. The WS is mocked with `page.routeWebSocket` (Playwright
// 1.48+): the route handler never calls `connectToServer`, so the page's
// WebSocket is fully mocked, and we push Envelope frames the `useSystemMetrics`
// hook folds into the footer. Auth is reported "off" so the app renders without
// a login gate. REST is stubbed empty so unrelated queries resolve.

const SAMPLE = {
  cpu_util: 0.55,
  self_cpu_util: 0.18,
  mem_used_bytes: 8_000_000_000,
  self_mem_used_bytes: 2_000_000_000,
  mem_total_bytes: 32_000_000_000,
  gpus: [
    {
      id: "gpu-0",
      vendor: "nvidia",
      name: "NVIDIA RTX 4060",
      compute_util: 0.6,
      self_compute_util: 0.25,
      mem_used_bytes: 4_000_000_000,
      self_mem_used_bytes: 1_500_000_000,
      mem_total_bytes: 8_000_000_000,
      encoder_util: 0.3,
      self_encoder_util: 0.1,
      decoder_util: 0.2,
      self_decoder_util: 0.05,
      encoder_sessions: 6,
      self_encoder_sessions: 2,
      encoder_session_ceiling: 8,
    },
    {
      id: "gpu-1",
      vendor: "nvidia",
      name: "NVIDIA RTX 4090",
      compute_util: 0.4,
      self_compute_util: 0.3,
      mem_used_bytes: 12_000_000_000,
      self_mem_used_bytes: 6_000_000_000,
      mem_total_bytes: 24_000_000_000,
      encoder_util: 0.5,
      self_encoder_util: 0.2,
      decoder_util: 0.1,
      self_decoder_util: 0.05,
      encoder_sessions: 4,
      self_encoder_sessions: 3,
      encoder_session_ceiling: 8,
    },
  ],
  program_fps: 50,
  sampled_hz: 2,
};

/** Build one `system.metrics` Envelope frame at the given sequence. */
function frame(seq: number, cpuUtil: number): string {
  return JSON.stringify({
    v: 1,
    t: "system.metrics",
    topic: "system",
    seq,
    ts: seq * 1_000_000,
    data: { ...SAMPLE, cpu_util: cpuUtil },
  });
}

test.beforeEach(async ({ page }) => {
  // Mock the realtime WS BEFORE navigation: accept the (mocked) connection and,
  // after the client opens it, push a few metrics frames. Not calling
  // connectToServer keeps the socket fully mocked (no real backend needed).
  await page.routeWebSocket("**/api/v1/ws*", (ws) => {
    ws.onMessage(() => {
      // The client may send a subscribe/heartbeat frame; we don't require it —
      // just keep the socket open and ignore client→server traffic.
    });
    // Push three samples with a rising CPU value so the footer + sparkline fill.
    ws.send(frame(1, 0.2));
    ws.send(frame(2, 0.42));
    ws.send(frame(3, 0.55));
  });

  // Auth off; everything else returns an empty JSON body.
  await page.route("**/api/v1/**", (route) => route.fulfill({ json: [] }));
  await page.route("**/api/v1/auth/status*", (route) =>
    route.fulfill({ json: { auth_required: false, authenticated: true } }),
  );
});

test("the System page shows a live CPU ours-vs-total value", async ({ page }) => {
  await page.goto("/system");
  await expect(page.getByRole("heading", { level: 1, name: "System" })).toBeVisible();
  await expect(page.getByText("18% / 55%").first()).toBeVisible();
});

test("the System page shows EVERY GPU with an ours-vs-total breakdown", async ({ page }) => {
  await page.goto("/system");
  await expect(page.getByText("NVIDIA RTX 4060")).toBeVisible();
  await expect(page.getByText("NVIDIA RTX 4090")).toBeVisible();
  await expect(page.getByText("25% / 60%").first()).toBeVisible();
  await expect(page.getByText("30% / 40%").first()).toBeVisible();
});

test("the System page is reachable from the navigation drawer", async ({ page }) => {
  await page.goto("/");
  await page.getByRole("button", { name: "Open navigation" }).click();
  await page.getByRole("dialog").getByRole("link", { name: "System", exact: true }).click();
  await expect(page).toHaveURL(/\/system$/);
  await expect(page.getByRole("heading", { level: 1, name: "System" })).toBeVisible();
});
