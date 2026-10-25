import { test, expect } from "@playwright/test";
import type { WebSocketRoute } from "@playwright/test";

// SW-19 (#45): a command's 202 only toasts "accepted"; the authoritative outcome
// (envelope `corr` == operation id) is then surfaced to the operator.

test("salvo take shows acceptance, then the failure outcome with its reason", async ({ page }) => {
  await page.route("**/api/v1/**", (route) => route.fulfill({ json: [] }));
  await page.route("**/api/v1/auth/status*", (route) =>
    route.fulfill({ json: { auth_required: false, authenticated: true } }),
  );
  await page.route("**/api/v1/salvos", (route) =>
    route.fulfill({ json: [{ id: "s1", display_name: "Wide" }] }),
  );
  let socket: WebSocketRoute | undefined;
  await page.route("**/api/v1/salvos/s1/take", async (route) => {
    await route.fulfill({ status: 202, json: { operation_id: "op-e2e-1", kind: "take" } });
    // The outcome envelope follows the 202 on the (mocked) realtime socket.
    setTimeout(() => {
      socket?.send(
        JSON.stringify({ v: 1, t: "job.failed", topic: "jobs", seq: 1, ts: 1, corr: "op-e2e-1",
          data: { message: "layout missing" } }),
      );
    }, 300);
  });
  await page.routeWebSocket("**", (ws) => {
    socket = ws;
    ws.onMessage(() => {
      // keep open
    });
  });

  await page.goto("/salvos");
  await page.getByRole("button", { name: /Take salvo: s1/ }).click();
  await expect(page.getByText("Command accepted")).toBeVisible();
  await expect(page.getByText("layout missing", { exact: true })).toBeVisible();
  await expect(page.getByText("Salvo take: s1 failed", { exact: true })).toBeVisible();
});
