import { test, expect } from "@playwright/test";

// SW-19 (#45): useEngineEvents + useHealth + useSystemMetrics share ONE realtime
// socket per session (was 3+). The WS is mocked via `page.routeWebSocket`; every
// `websocket` event the page opens is counted.

test("all realtime hooks share exactly one WebSocket", async ({ page }) => {
  let opened = 0;
  page.on("websocket", () => {
    opened += 1;
  });
  await page.route("**/api/v1/**", (route) => route.fulfill({ json: [] }));
  await page.route("**/api/v1/auth/status*", (route) =>
    route.fulfill({ json: { auth_required: false, authenticated: true } }),
  );
  await page.routeWebSocket("**/api/v1/ws*", (ws) => {
    ws.onMessage(() => {
      // keep open
    });
    ws.send(
      JSON.stringify({ v: 1, t: "health.warning.raised", topic: "alerts", seq: 1, ts: 1,
        data: { code: "c", severity: "warning", subsystem: "s", message: "SHARED-OK", remediation: "r", since: 1, active: true } }),
    );
  });

  await page.goto("/");
  await expect(page.getByRole("alert").getByText("SHARED-OK")).toBeVisible();
  await page.waitForTimeout(1500);
  expect(opened).toBe(1);
});
