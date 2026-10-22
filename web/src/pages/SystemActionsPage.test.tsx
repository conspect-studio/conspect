// MSW tests for the System Actions screen (/system/actions): the pending-actions
// strip (list + local Cancel wired to POST /api/v1/actions/{id}/cancel) and the
// append-only account audit log (cursor-paginated, filterable, with kind/actor/
// at/detail columns and mono ids).
import { afterAll, afterEach, beforeAll, describe, expect, it } from "vitest";
import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { http, HttpResponse } from "msw";
import { setupServer } from "msw/node";
import { MemoryRouter } from "react-router-dom";

import { SystemActionsPage } from "./SystemActionsPage";
import { renderWithProviders } from "../test/render";

let pending = [
  {
    action_id: "act-1",
    kind: "restart",
    requested_by: "portal:ops",
    requested_at_nanos: 1_000_000_000,
    state: "pending",
    detail: null,
  },
];
let cancelled: string | undefined;

const PAGE_1 = {
  entries: [
    { seq: 1, at_nanos: 1_000_000_000, actor: "local", kind: "consent-change", detail: null },
    { seq: 2, at_nanos: 2_000_000_000, actor: "portal:ops", kind: "relay-toggle", detail: null },
  ],
  next_cursor: 2,
};
const PAGE_2 = {
  entries: [
    { seq: 3, at_nanos: 3_000_000_000, actor: "local", kind: "lease-install", detail: "LS-0002" },
  ],
  next_cursor: null,
};

const server = setupServer(
  http.get("*/api/v1/actions/pending", () => HttpResponse.json(pending)),
  http.post("*/api/v1/actions/:id/cancel", ({ params }) => {
    cancelled = params.id as string;
    pending = [];
    return HttpResponse.json({ cancelled: true });
  }),
  http.get("*/api/v1/account/audit", ({ request }) => {
    const url = new URL(request.url);
    const cursor = url.searchParams.get("cursor");
    const filter = url.searchParams.get("filter");
    if (filter === "lease-install") {
      return HttpResponse.json({ entries: [PAGE_2.entries[0]], next_cursor: null });
    }
    return HttpResponse.json(cursor === "2" ? PAGE_2 : PAGE_1);
  }),
);

beforeAll(() => {
  server.listen();
});
afterEach(() => {
  pending = [
    {
      action_id: "act-1",
      kind: "restart",
      requested_by: "portal:ops",
      requested_at_nanos: 1_000_000_000,
      state: "pending",
      detail: null,
    },
  ];
  cancelled = undefined;
  server.resetHandlers();
});
afterAll(() => {
  server.close();
});

function renderActions(): void {
  renderWithProviders(
    <MemoryRouter>
      <SystemActionsPage />
    </MemoryRouter>,
  );
}

describe("SystemActionsPage — pending strip", () => {
  it("lists pending actions with mono ids and a Cancel control", async () => {
    renderActions();
    const strip = await screen.findByTestId("pending-strip");
    expect(await within(strip).findByText("act-1")).toBeInTheDocument();
    expect(within(strip).getByRole("button", { name: /cancel/i })).toBeInTheDocument();
  });

  it("POSTs the cancel and refreshes the strip", async () => {
    renderActions();
    const strip = await screen.findByTestId("pending-strip");
    await within(strip).findByText("act-1");
    await userEvent.click(within(strip).getByRole("button", { name: /cancel/i }));
    await waitFor(() => {
      expect(cancelled).toBe("act-1");
    });
    await waitFor(() => {
      expect(screen.getByText(/no pending actions/i)).toBeInTheDocument();
    });
  });
});

describe("SystemActionsPage — audit log", () => {
  it("renders the audit table with kind/actor/at/detail columns", async () => {
    renderActions();
    const table = await screen.findByTestId("account-audit-table");
    expect(within(table).getByText("consent-change")).toBeInTheDocument();
    expect(within(table).getByText("relay-toggle")).toBeInTheDocument();
    // Actor is shown.
    expect(within(table).getAllByText("local").length).toBeGreaterThanOrEqual(1);
  });

  it("paginates to the next page via the cursor", async () => {
    renderActions();
    const table = await screen.findByTestId("account-audit-table");
    await within(table).findByText("consent-change");
    await userEvent.click(screen.getByRole("button", { name: /next page/i }));
    await waitFor(() => {
      expect(within(screen.getByTestId("account-audit-table")).getByText("lease-install")).toBeInTheDocument();
    });
    // The detail of the next page is shown (mono).
    expect(within(screen.getByTestId("account-audit-table")).getByText("LS-0002")).toBeInTheDocument();
  });

  it("filters by kind", async () => {
    renderActions();
    const table = await screen.findByTestId("account-audit-table");
    await within(table).findByText("consent-change");
    const select = screen.getByLabelText(/filter by kind/i);
    await userEvent.selectOptions(select, "lease-install");
    await waitFor(() => {
      expect(within(screen.getByTestId("account-audit-table")).getByText("lease-install")).toBeInTheDocument();
    });
    expect(
      within(screen.getByTestId("account-audit-table")).queryByText("consent-change"),
    ).toBeNull();
  });
});
