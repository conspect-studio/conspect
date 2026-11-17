// Settings → Integrations → Windy (issue #470): write-only credential, Test
// login result, and the admin-only state for a non-admin.
import { afterAll, afterEach, beforeAll, describe, expect, it } from "vitest";
import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { http, HttpResponse } from "msw";
import { setupServer } from "msw/node";
import { MemoryRouter } from "react-router-dom";

import { SettingsPage } from "./SettingsPage";
import { I18nProvider as AppI18nProvider } from "../i18n/I18nProvider";
import { ThemeProvider } from "../theme/ThemeProvider";
import { renderWithProviders } from "../test/render";

const UNSET = { set: false, last_verified_at: null, result: null, premium_available: false, verifying: false };
const SET = { ...UNSET, set: true };

const server = setupServer(
  http.get("*/api/v1/config/watch-status", () =>
    HttpResponse.json({ active: false, path: null, applied_count: 0, last_applied: null, last_rejected: null, restart_pending: [] }),
  ),
);

beforeAll(() => {
  server.listen();
});
afterEach(() => {
  server.resetHandlers();
});
afterAll(() => {
  server.close();
});

function renderSettings(): void {
  renderWithProviders(
    <AppI18nProvider>
      <ThemeProvider>
        <MemoryRouter>
          <SettingsPage />
        </MemoryRouter>
      </ThemeProvider>
    </AppI18nProvider>,
  );
}

describe("SettingsPage Windy integration card", () => {
  it("saves the credential write-only and clears the password field", async () => {
    let sent: unknown;
    server.use(
      http.get("*/api/v1/integrations/windy", () => HttpResponse.json(UNSET)),
      http.put("*/api/v1/integrations/windy", async ({ request }) => {
        sent = await request.json();
        return HttpResponse.json(SET);
      }),
    );
    renderSettings();
    expect(await screen.findByText(/No account/)).toBeInTheDocument();
    await userEvent.type(screen.getByLabelText(/Windy email/i), "ops@example.com");
    const password = screen.getByLabelText(/Windy password/i);
    expect(password).toHaveAttribute("type", "password");
    await userEvent.type(password, "hunter2-secret");
    await userEvent.click(screen.getByRole("button", { name: "Save Windy account" }));
    await waitFor(() => {
      expect(sent).toEqual({ email: "ops@example.com", password: "hunter2-secret" });
    });
    expect(await screen.findByText(/Account set/)).toBeInTheDocument();
    expect(screen.getByLabelText(/Windy password/i)).toHaveValue("");
    expect(screen.queryByDisplayValue("hunter2-secret")).not.toBeInTheDocument();
  });

  it("Test login reports a wrong password and a Premium pass", async () => {
    let verdict = "rejected";
    server.use(
      http.get("*/api/v1/integrations/windy", () => HttpResponse.json(SET)),
      http.post("*/api/v1/integrations/windy/verify", () =>
        HttpResponse.json({
          result: verdict,
          account: {
            ...SET,
            result: verdict,
            last_verified_at: "2026-11-14T23:00:00Z",
            premium_available: verdict === "premium",
          },
        }),
      ),
    );
    renderSettings();
    const test = await screen.findByRole("button", { name: "Test login" });
    await waitFor(() => {
      expect(test).toBeEnabled();
    });
    await userEvent.click(test);
    expect(
      (await screen.findAllByText(/Login failed: wrong email or password/)).length,
    ).toBeGreaterThan(0);
    verdict = "premium";
    await userEvent.click(screen.getByRole("button", { name: "Test login" }));
    expect(await screen.findByText(/Premium available/)).toBeInTheDocument();
  });

  it("tells a non-admin the account is admin-only", async () => {
    server.use(
      http.get("*/api/v1/integrations/windy", () =>
        HttpResponse.json({ type: "/problems/forbidden", title: "Access denied", status: 403 }, { status: 403 }),
      ),
    );
    renderSettings();
    expect(
      await screen.findByText(/Only an administrator can view or change the Windy account/),
    ).toBeInTheDocument();
    expect(screen.queryByLabelText(/Windy password/i)).not.toBeInTheDocument();
  });
});
