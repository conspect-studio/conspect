// SMOKE test for the Probes management page: the table renders the stored
// probes, the create / edit / delete dialogs open over a populated table, the
// cell picker offers the working layout's cells, the apply-semantics callout is
// present, and an unknown-kind row is preserved (rendered + deletable, never
// editable through a fold). Mirrors SimplePages.test.tsx.
import { afterAll, afterEach, beforeAll, describe, expect, it } from "vitest";
import { screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { http, HttpResponse } from "msw";
import { setupServer } from "msw/node";
import { MemoryRouter } from "react-router-dom";

import { ProbesPage } from "./ProbesPage";
import { renderWithProviders } from "../test/render";

const PROBES = [
  {
    id: "black-1",
    name: "Black on PGM",
    body: {
      id: "black-1",
      cell: "cell-a",
      kind: "black",
      luma_threshold: 16,
      dwell: { up_ms: 2000, down_ms: 1000 },
      severity: "Major",
      latched: false,
    },
  },
  {
    id: "loud-1",
    name: "Loudness compliance",
    body: {
      id: "loud-1",
      cell: "cell-b",
      kind: "loudness",
      target: { kind: "r128", target_lufs: -23.0, max_true_peak_dbtp: -1.0 },
    },
  },
  // A kind this UI has no form for: rendered + deletable, but NOT editable.
  {
    id: "psnr-1",
    name: "Future PSNR probe",
    body: { id: "psnr-1", cell: "cell-a", kind: "psnr", threshold: 30 },
  },
];

const LAYOUTS = [
  {
    id: "working",
    name: "working",
    body: {
      canvas: { width: 1920, height: 1080 },
      layout: { kind: "absolute" },
      cells: [{ id: "cell-a" }, { id: "cell-b" }],
    },
  },
];

const server = setupServer(
  http.get("*/api/v1/probes", () => HttpResponse.json(PROBES)),
  http.get("*/api/v1/probes/:id", ({ params }) => {
    const found = PROBES.find((p) => p.id === String(params.id));
    return found
      ? HttpResponse.json(found, { headers: { ETag: '"1"' } })
      : new HttpResponse(null, { status: 404 });
  }),
  http.get("*/api/v1/layouts", () => HttpResponse.json(LAYOUTS)),
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

function renderProbes(): void {
  renderWithProviders(
    <MemoryRouter>
      <ProbesPage />
    </MemoryRouter>,
  );
}

describe("ProbesPage", () => {
  it("lists the stored probes with kind, cell, and severity", async () => {
    renderProbes();
    expect(await screen.findByText("Black on PGM")).toBeInTheDocument();
    expect(screen.getByText("Loudness compliance")).toBeInTheDocument();
    expect(screen.getByText("black")).toBeInTheDocument();
    expect(screen.getAllByText("cell-a").length).toBeGreaterThanOrEqual(1);
  });

  it("shows the apply-semantics callout", async () => {
    renderProbes();
    expect(await screen.findByRole("note")).toHaveTextContent(
      /config export.*restart|exporting the configuration/i,
    );
  });

  it("opens the create dialog with the cell picker fed from the working layout", async () => {
    renderProbes();
    expect(await screen.findByText("Black on PGM")).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "New probe" }));
    expect(await screen.findByRole("dialog")).toBeInTheDocument();
    expect(await screen.findByLabelText("Identifier")).toBeInTheDocument();
    // The cell picker is a combobox (Select) once the layout cells loaded.
    expect(await screen.findByRole("combobox", { name: "Cell" })).toBeInTheDocument();
  });

  it("opens the edit dialog and prefills the kind-specific threshold", async () => {
    renderProbes();
    expect(await screen.findByText("Black on PGM")).toBeInTheDocument();
    await userEvent.click(
      screen.getByRole("button", { name: "Edit probe: Black on PGM" }),
    );
    expect(await screen.findByRole("dialog")).toBeInTheDocument();
    expect(await screen.findByDisplayValue("16")).toBeInTheDocument();
  });

  it("opens the delete confirmation", async () => {
    renderProbes();
    expect(await screen.findByText("Black on PGM")).toBeInTheDocument();
    await userEvent.click(
      screen.getByRole("button", { name: "Delete probe: Black on PGM" }),
    );
    expect(await screen.findByText("Delete this resource?")).toBeInTheDocument();
  });

  it("renders an unknown-kind probe as authored but refuses to edit it", async () => {
    renderProbes();
    expect(await screen.findByText("Future PSNR probe")).toBeInTheDocument();
    // The raw authored kind is displayed, never folded.
    expect(screen.getByText("psnr")).toBeInTheDocument();
    const edit = screen.getByRole("button", {
      name: /Edit probe: Future PSNR probe —/,
    });
    expect(edit).toHaveAttribute("aria-disabled", "true");
  });
});
