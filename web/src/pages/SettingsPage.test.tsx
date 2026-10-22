// SettingsPage "Configuration file" card (ADR-W020): shows whether the boot
// config file is being watched, the watched path, the last applied/rejected
// loads, and any restart-pending sections — read from
// GET /api/v1/config/watch-status.
import { afterAll, afterEach, beforeAll, describe, expect, it } from "vitest";
import { screen } from "@testing-library/react";
import { http, HttpResponse } from "msw";
import { setupServer } from "msw/node";
import { MemoryRouter } from "react-router-dom";

import { SettingsPage } from "./SettingsPage";
import { I18nProvider as AppI18nProvider } from "../i18n/I18nProvider";
import { ThemeProvider } from "../theme/ThemeProvider";
import { renderWithProviders } from "../test/render";

const ACTIVE_STATUS = {
  active: true,
  path: "/etc/multiview/multiview.toml",
  applied_count: 3,
  last_applied: { at_ms: 1718000000123, detail: "sources: in_a changed" },
  last_rejected: { at_ms: 1718000050456, detail: "TOML parse error at line 3" },
  restart_pending: ["canvas", "outputs"],
};

const server = setupServer(
  http.get("*/api/v1/config/watch-status", () => HttpResponse.json(ACTIVE_STATUS)),
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

describe("SettingsPage configuration-file card", () => {
  it("shows the watched path and the watch-active state", async () => {
    renderSettings();
    // The card exists…
    expect(
      await screen.findByRole("heading", { name: /configuration file/i }),
    ).toBeInTheDocument();
    // …and reflects the live watch status from the API.
    expect(
      await screen.findByText("/etc/multiview/multiview.toml"),
    ).toBeInTheDocument();
    expect(await screen.findByText(/watching/i)).toBeInTheDocument();
  });

  it("surfaces the last applied and last rejected loads", async () => {
    renderSettings();
    expect(
      await screen.findByText(/sources: in_a changed/),
    ).toBeInTheDocument();
    expect(
      await screen.findByText(/TOML parse error at line 3/),
    ).toBeInTheDocument();
  });

  it("names the restart-pending sections", async () => {
    renderSettings();
    expect(await screen.findByText(/restart pending/i)).toBeInTheDocument();
    expect(await screen.findByText("canvas, outputs")).toBeInTheDocument();
  });

  it("shows the not-watched state when the watcher is inactive", async () => {
    server.use(
      http.get("*/api/v1/config/watch-status", () =>
        HttpResponse.json({
          active: false,
          path: null,
          applied_count: 0,
          last_applied: null,
          last_rejected: null,
          restart_pending: [],
        }),
      ),
    );
    renderSettings();
    expect(
      await screen.findByRole("heading", { name: /configuration file/i }),
    ).toBeInTheDocument();
    expect(await screen.findByText(/not watched/i)).toBeInTheDocument();
  });
});
