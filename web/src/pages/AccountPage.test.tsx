// Tests for the Account screen (/settings/account): claim/transfer/deactivate
// per §2. The claim REDEMPTION endpoint (POST /api/v1/account/claim) is NOT in
// the OpenAPI yet (O1-blocked, server-side), so this screen renders the spec'd
// UNCLAIMED state with an HONEST, DISABLED 6-char code form and an explicit
// "requires licence-server connectivity (not yet wired in this build)" note,
// plus the offline-claim export path (the challenge export on the Licence
// screen). The 6-char code field validates the ambiguity-free charset
// client-side. This is the honest rendering of the as-built backend, not a stub.
import { afterAll, afterEach, beforeAll, describe, expect, it } from "vitest";
import { screen, within } from "@testing-library/react";
import { http, HttpResponse } from "msw";
import { setupServer } from "msw/node";
import { MemoryRouter } from "react-router-dom";

import { AccountPage } from "./AccountPage";
import {
  CLAIM_CODE_CHARSET,
  CLAIM_CODE_LEN,
  isClaimCodeChar,
} from "./account-constants";
import { renderWithProviders } from "../test/render";

// The licence resource drives the claimed/unclaimed framing. Here: unclaimed.
const server = setupServer(
  http.get("*/api/v1/licence", () => HttpResponse.json({ licensed: false, status: null })),
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

function renderAccount(): void {
  renderWithProviders(
    <MemoryRouter>
      <AccountPage />
    </MemoryRouter>,
  );
}

describe("AccountPage — claim-code constants", () => {
  it("pins the 6-character length", () => {
    expect(CLAIM_CODE_LEN).toBe(6);
  });

  it("uses an ambiguity-free charset (excludes 0/O/1/I/L)", () => {
    for (const glyph of ["0", "O", "1", "I", "L"]) {
      expect(CLAIM_CODE_CHARSET).not.toContain(glyph);
    }
  });

  it("validates charset membership case-insensitively", () => {
    // A known good glyph in either case is accepted; an ambiguous one is not.
    const good = CLAIM_CODE_CHARSET[0] ?? "";
    expect(isClaimCodeChar(good)).toBe(true);
    expect(isClaimCodeChar(good.toLowerCase())).toBe(true);
    expect(isClaimCodeChar("0")).toBe(false);
  });
});

describe("AccountPage — unclaimed state (claim endpoint absent / O1-blocked)", () => {
  it("renders the unclaimed state", async () => {
    renderAccount();
    expect(await screen.findByText(/not claimed/i)).toBeInTheDocument();
  });

  it("shows the 6-char code form DISABLED with an honest 'not yet wired' note", async () => {
    renderAccount();
    const input = await screen.findByLabelText(/claim code/i);
    expect(input).toBeDisabled();
    const submit = screen.getByRole("button", { name: /claim this machine/i });
    expect(submit).toBeDisabled();
    expect(
      screen.getByText(/requires licence-server connectivity \(not yet wired in this build\)/i),
    ).toBeInTheDocument();
  });

  it("offers the offline-claim export path linking to the challenge anchor", async () => {
    renderAccount();
    const link = await screen.findByRole("link", { name: /export a challenge/i });
    expect(link).toHaveAttribute("href", "/settings/licence#challenge");
  });

  it("documents the 6-character ambiguity-free rule next to the field", async () => {
    renderAccount();
    await screen.findByLabelText(/claim code/i);
    // The field is disabled (no endpoint), but the rule is documented inline; the
    // charset validator itself is pinned in the constants tests above.
    expect(screen.getByText(/6 characters/i)).toBeInTheDocument();
  });
});

// The claimed-state branch: transfer and deactivate are SEPARATE, discrete
// honestly-disabled sections (their endpoints are O1-blocked, same as claim
// redemption), not a single shared sentence. Each names what it would do and
// states plainly that it is not yet wired.
const CLAIMED_SERVER = setupServer(
  http.get("*/api/v1/licence", () =>
    HttpResponse.json({
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
          serial: "LS-0001",
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
    }),
  ),
);

describe("AccountPage — claimed state (transfer/deactivate endpoints absent / O1-blocked)", () => {
  beforeAll(() => {
    CLAIMED_SERVER.listen();
  });
  afterEach(() => {
    CLAIMED_SERVER.resetHandlers();
  });
  afterAll(() => {
    CLAIMED_SERVER.close();
  });

  it("renders the claimed state", async () => {
    renderAccount();
    expect(await screen.findByText(/this machine is claimed/i)).toBeInTheDocument();
  });

  it("renders a DISABLED transfer section with an honest 'not yet wired' note", async () => {
    renderAccount();
    const section = await screen.findByTestId("transfer-section");
    const button = within(section).getByRole("button", { name: /transfer/i });
    expect(button).toBeDisabled();
    expect(section).toHaveTextContent(/not yet wired in this build/i);
  });

  it("renders a DISABLED deactivate section with an honest 'not yet wired' note", async () => {
    renderAccount();
    const section = await screen.findByTestId("deactivate-section");
    const button = within(section).getByRole("button", { name: /deactivate/i });
    expect(button).toBeDisabled();
    expect(section).toHaveTextContent(/not yet wired in this build/i);
  });

  it("does not show the claim form when already claimed", async () => {
    renderAccount();
    await screen.findByText(/this machine is claimed/i);
    expect(screen.queryByLabelText(/claim code/i)).toBeNull();
  });
});
