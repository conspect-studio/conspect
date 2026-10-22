// MSW tests for the first-run Welcome screen (/welcome): it greets an unclaimed
// machine, points at the claim/offline path on the Account + Licence screens,
// and reflects the licence state (claimed → a "you're set" affirmation).
import { afterAll, afterEach, beforeAll, describe, expect, it } from "vitest";
import { screen } from "@testing-library/react";
import { http, HttpResponse } from "msw";
import { setupServer } from "msw/node";
import { MemoryRouter } from "react-router-dom";

import { WelcomePage } from "./WelcomePage";
import type { LicenceResource } from "../api/conspect";
import { renderWithProviders } from "../test/render";

let licenceBody: LicenceResource = { licensed: false, status: null };

const server = setupServer(
  http.get("*/api/v1/licence", () => HttpResponse.json(licenceBody)),
);

beforeAll(() => {
  server.listen();
});
afterEach(() => {
  licenceBody = { licensed: false, status: null };
  server.resetHandlers();
});
afterAll(() => {
  server.close();
});

function renderWelcome(): void {
  renderWithProviders(
    <MemoryRouter>
      <WelcomePage />
    </MemoryRouter>,
  );
}

describe("WelcomePage", () => {
  it("greets a first-run unclaimed machine and links to claim + offline paths", async () => {
    renderWelcome();
    expect(await screen.findByRole("heading", { name: /welcome/i })).toBeInTheDocument();
    expect(screen.getByRole("link", { name: /claim/i })).toHaveAttribute(
      "href",
      "/settings/account",
    );
    expect(screen.getByRole("link", { name: /offline/i })).toHaveAttribute(
      "href",
      "/settings/licence#challenge",
    );
  });

  it("affirms when the machine is already claimed", async () => {
    licenceBody = {
      licensed: true,
      status: {
        tier: "studio",
        state: "compliant",
        enforcement: "active",
        hardware_class: { licensed: "standard", detected: "standard" },
        gpu_limit: { kind: "unlimited" },
        gpus_in_use: 0,
        config_locked: false,
        watermark: false,
        blocks_new_instances: false,
        lease: {
          serial: "LS-1",
          source: "online",
          granted_at: "2999-01-01T00:00:00Z",
          expires_at: "2999-02-05T00:00:00Z",
          grace_days: 14,
          grace_until: "2999-02-19T00:00:00Z",
          hard_at: "2999-03-30T00:00:00Z",
          next_contact_due: "2999-02-01T00:00:00Z",
        },
        reasons: [],
      },
    };
    renderWelcome();
    expect(await screen.findByText(/this machine is licensed/i)).toBeInTheDocument();
  });
});
