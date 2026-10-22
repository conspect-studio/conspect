// SMOKE test for the Overlays management page apply-semantics copy (ADR-W022):
// the page renders the stored overlays, and the callout + saved/deleted copy
// tell the truth about live apply — analog clock overlays apply to the running
// engine immediately (X-Multiview-Apply: live); other kinds are stored and go
// live via config export + restart. Mirrors ProbesPage.test.tsx.
import { afterAll, afterEach, beforeAll, describe, expect, it } from "vitest";
import { screen } from "@testing-library/react";
import { http, HttpResponse } from "msw";
import { setupServer } from "msw/node";
import { MemoryRouter } from "react-router-dom";

import { OverlaysPage } from "./OverlaysPage";
import { renderWithProviders } from "../test/render";

const OVERLAYS = [
  {
    id: "wall-clock",
    name: "Wall clock",
    body: {
      id: "wall-clock",
      kind: "clock",
      target: "canvas",
      z: 10,
      face: "analog",
      x: 200,
      y: 120,
      radius: 40,
    },
  },
  {
    id: "cam-label",
    name: "Camera label",
    body: { id: "cam-label", kind: "label", target: "cell_a", z: 5 },
  },
];

const server = setupServer(
  http.get("*/api/v1/overlays", () => HttpResponse.json(OVERLAYS)),
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

function renderOverlays(): void {
  renderWithProviders(
    <MemoryRouter>
      <OverlaysPage />
    </MemoryRouter>,
  );
}

describe("OverlaysPage", () => {
  it("lists the stored overlays", async () => {
    renderOverlays();
    expect(await screen.findByText("Wall clock")).toBeInTheDocument();
    expect(screen.getByText("Camera label")).toBeInTheDocument();
  });

  it("tells the live-apply truth in the apply-semantics callout (ADR-W022)", async () => {
    renderOverlays();
    const note = await screen.findByRole("note");
    // Analog clock overlays apply live on a rendering build…
    expect(note).toHaveTextContent(/analog.*clock.*(immediately|running engine)/i);
    // …and every other kind is stored until config export + restart.
    expect(note).toHaveTextContent(/config export.*restart/i);
    // The response header carries the per-mutation truth.
    expect(note).toHaveTextContent(/X-Multiview-Apply/i);
  });
});
