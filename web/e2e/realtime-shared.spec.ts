import { test, expect } from "@playwright/test";

// SW-19 (#45): useEngineEvents + useHealth + useSystemMetrics share ONE realtime
// socket per session (was 3+). The WS is mocked via `page.routeWebSocket` (for which
// `page.on("websocket")` never fires); each socket the page opens hits the handler.

test("all realtime hooks share exactly one WebSocket", async ({ page }) => {
  // Count every WebSocket the page opens (routeWebSocket replaces the page's
  // WebSocket, so `page.on("websocket")` does not fire for mocked sockets).
  let routed = 0;
  await page.route("**/api/v1/**", (route) => route.fulfill({ json: [] }));
  await page.route("**/api/v1/auth/status*", (route) =>
    route.fulfill({ json: { auth_required: false, authenticated: true } }),
  );
  await page.routeWebSocket("**", (ws) => {
    routed += 1;
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
  expect(routed).toBe(1);
});
