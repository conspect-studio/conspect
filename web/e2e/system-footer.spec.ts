import { test, expect } from "@playwright/test";

// Drives the REAL built SPA and asserts the live system-metrics footer renders
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

test("the footer shows a live CPU ours-vs-total value and a sparkline", async ({
  page,
}) => {
  await page.goto("/");

  // The desktop footer is a button labelled for the system page.
  const footer = page.getByRole("button", {
    name: "Open the system metrics page",
  });
  await expect(footer).toBeVisible();

  // The hook conflates to the LATEST sample; the CPU cell shows OURS / TOTAL as
  // explicit text (self_cpu_util 0.18 -> 18%, cpu_util 0.55 -> 55%). The slash
  // makes the relation legible WITHOUT relying on colour (WCAG 1.4.1).
  await expect(footer.getByText("18% / 55%")).toBeVisible();

  // The CPU sparkline announces the ours-over-total relation in its label.
  await expect(
    footer.getByRole("img", {
      name: "CPU utilisation trend: ours over host total",
    }),
  ).toBeVisible();

  // The live dot reports the open connection.
  await expect(footer.getByText("Live")).toBeVisible();
});

test("the footer shows EVERY GPU with an ours-vs-total compute + NVENC breakdown", async ({
  page,
}) => {
  await page.goto("/");

  const footer = page.getByRole("button", {
    name: "Open the system metrics page",
  });
  await expect(footer).toBeVisible();

  // Both GPUs get their own labelled mini-group (not just gpus[0]).
  await expect(footer.getByText("GPU0", { exact: true })).toBeVisible();
  await expect(footer.getByText("GPU1", { exact: true })).toBeVisible();

  // GPU0 compute as OURS / TOTAL (self 0.25 -> 25%, total 0.6 -> 60%).
  await expect(footer.getByText("25% / 60%")).toBeVisible();
  // GPU1 compute as OURS / TOTAL (self 0.3 -> 30%, total 0.4 -> 40%).
  await expect(footer.getByText("30% / 40%")).toBeVisible();

  // NVENC sessions are shown ours/device-total per GPU (GPU0 2/6, GPU1 3/4).
  await expect(footer.getByText("2 / 6")).toBeVisible();
  await expect(footer.getByText("3 / 4")).toBeVisible();

  // Each GPU compute trend is its OWN sparkline (the dashed "ours" overlay rides
  // the same graphic, distinguished by line style not colour).
  await expect(
    footer.getByRole("img", {
      name: /GPU0 compute utilisation: ours 25% of 60% device total/,
    }),
  ).toBeVisible();
  await expect(
    footer.getByRole("img", {
      name: /GPU1 compute utilisation: ours 30% of 40% device total/,
    }),
  ).toBeVisible();
});

test("clicking the footer navigates to the System page", async ({ page }) => {
  await page.goto("/");

  await page
    .getByRole("button", { name: "Open the system metrics page" })
    .click();

  await expect(page).toHaveURL(/\/system$/);
  // The System page heading renders.
  await expect(
    page.getByRole("heading", { level: 1, name: "System" }),
  ).toBeVisible();
  // Both GPU cards surface their device names.
  await expect(page.getByText("NVIDIA RTX 4060")).toBeVisible();
  await expect(page.getByText("NVIDIA RTX 4090")).toBeVisible();
});
