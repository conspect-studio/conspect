// MSW tests for the Mesh screen (/settings/mesh): the discovery LOCKED row
// (always-on, no toggle, the disclosure + exhaustive announce list); the relay
// toggle (real, wired to PUT /api/v1/mesh/relay) + relaying-for status; the
// piggyback/leaf status (role/via); the read-only nearby-peers panel; and the
// isolated-network state linking to /settings/licence#challenge.
import { afterAll, afterEach, beforeAll, describe, expect, it } from "vitest";
import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { http, HttpResponse } from "msw";
import { setupServer } from "msw/node";
import { MemoryRouter } from "react-router-dom";

import { MeshPage } from "./MeshPage";
import type { MeshPeerDoc, MeshStatusDoc } from "../api/conspect";
import { renderWithProviders } from "../test/render";

let statusBody: MeshStatusDoc = {
  discovery: "always_on",
  relay_enabled: false,
  role: { kind: "direct" },
  peers_count: 1,
};
let lastRelayPut: boolean | undefined;

const PEERS: MeshPeerDoc[] = [
  { key: "a".repeat(64), claimed: true, last_seen: 1000, relaying_for_us: false, name: null },
  {
    key: "b".repeat(64),
    claimed: false,
    last_seen: 1500,
    relaying_for_us: true,
    name: "Studio relay",
  },
];

const server = setupServer(
  http.get("*/api/v1/mesh/status", () => HttpResponse.json(statusBody)),
  http.get("*/api/v1/mesh/peers", () => HttpResponse.json(PEERS)),
  http.put("*/api/v1/mesh/relay", async ({ request }) => {
    const body = (await request.json()) as { enabled: boolean };
    lastRelayPut = body.enabled;
    statusBody = {
      discovery: "always_on",
      relay_enabled: body.enabled,
      role: body.enabled ? { kind: "relay" } : { kind: "direct" },
      peers_count: 1,
    };
    return HttpResponse.json(statusBody);
  }),
);

beforeAll(() => {
  server.listen();
});
afterEach(() => {
  statusBody = {
    discovery: "always_on",
    relay_enabled: false,
    role: { kind: "direct" },
    peers_count: 1,
  };
  lastRelayPut = undefined;
  server.resetHandlers();
});
afterAll(() => {
  server.close();
});

function renderMesh(): void {
  renderWithProviders(
    <MemoryRouter>
      <MeshPage />
    </MemoryRouter>,
  );
}

describe("MeshPage — discovery (LOCKED)", () => {
  it("has NO toggle in the discovery panel (always-on, locked)", async () => {
    renderMesh();
    const panel = await screen.findByTestId("discovery-panel");
    expect(within(panel).queryByRole("switch")).toBeNull();
    expect(within(panel).queryByRole("checkbox")).toBeNull();
  });

  it("states always-on and the disclosure", async () => {
    renderMesh();
    const panel = await screen.findByTestId("discovery-panel");
    expect(within(panel).getByText(/always on/i)).toBeInTheDocument();
    expect(within(panel).getByTestId("discovery-announce-list")).toBeInTheDocument();
  });
});

describe("MeshPage — relay (real toggle)", () => {
  it("renders the relay switch reflecting status and PUTs on toggle", async () => {
    renderMesh();
    const toggle = await screen.findByRole("switch", { name: /relay/i });
    expect(toggle).toHaveAttribute("aria-checked", "false");
    await userEvent.click(toggle);
    await waitFor(() => {
      expect(lastRelayPut).toBe(true);
    });
    await waitFor(() => {
      expect(screen.getByRole("switch", { name: /relay/i })).toHaveAttribute(
        "aria-checked",
        "true",
      );
    });
  });
});

describe("MeshPage — role / piggyback status", () => {
  it("shows the leaf role with its via peer", async () => {
    statusBody = {
      discovery: "always_on",
      relay_enabled: false,
      role: { kind: "leaf", via: "c".repeat(64) },
      via: "c".repeat(64),
      peers_count: 2,
    };
    renderMesh();
    const role = await screen.findByTestId("mesh-role");
    expect(role).toHaveTextContent(/leaf/i);
    expect(role).toHaveTextContent(/cccc/);
  });
});

describe("MeshPage — nearby peers (read-only)", () => {
  it("lists discovered peers read-only with mono keys and no adopt control", async () => {
    renderMesh();
    const panel = await screen.findByTestId("peers-panel");
    expect(await within(panel).findByText(/aaaa/)).toBeInTheDocument();
    expect(within(panel).getByText("Studio relay")).toBeInTheDocument();
    // Read-only inventory: no adopt/forget buttons on this surface.
    expect(within(panel).queryByRole("button", { name: /adopt/i })).toBeNull();
  });
});

describe("MeshPage — isolated network", () => {
  it("links the isolated-network guidance to the challenge anchor", async () => {
    renderMesh();
    const link = await screen.findByRole("link", { name: /export a challenge/i });
    expect(link).toHaveAttribute("href", "/settings/licence#challenge");
  });
});
