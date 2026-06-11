// The login gate: renders the app when auth is off or the token works, and the
// login page when auth is required and the browser has no valid token; a valid
// key entered on the login page unlocks the app (task #71).
import { afterAll, afterEach, beforeAll, describe, expect, it } from "vitest";
import { screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { http, HttpResponse } from "msw";
import { setupServer } from "msw/node";

import { RequireAuth } from "./RequireAuth";
import { renderWithProviders } from "../test/render";
import { clearStoredToken } from "../api/token";

let mode: { required: boolean; goodKey?: string } = { required: false };

const server = setupServer(
  http.get("http://localhost:3000/api/v1/auth/status", ({ request }) => {
    const auth = request.headers.get("Authorization");
    const authenticated =
      !mode.required ||
      (mode.goodKey !== undefined && auth === `Bearer ${mode.goodKey}`);
    return HttpResponse.json({
      auth_required: mode.required,
      authenticated,
    });
  }),
);

beforeAll(() => {
  server.listen();
});
afterEach(() => {
  server.resetHandlers();
  clearStoredToken();
});
afterAll(() => {
  server.close();
});

describe("RequireAuth", () => {
  it("renders the app when auth is not required", async () => {
    mode = { required: false };
    renderWithProviders(
      <RequireAuth>
        <div>app-content</div>
      </RequireAuth>,
    );
    expect(await screen.findByText("app-content")).toBeInTheDocument();
  });

  it("shows the login page when auth is required and no token is stored", async () => {
    mode = { required: true, goodKey: "admin.secret" };
    renderWithProviders(
      <RequireAuth>
        <div>app-content</div>
      </RequireAuth>,
    );
    // The login page (its submit button) appears; the app does not.
    expect(
      await screen.findByRole("button", { name: "Sign in" }),
    ).toBeInTheDocument();
    expect(screen.queryByText("app-content")).not.toBeInTheDocument();
  });

  it("unlocks the app when a valid key is entered", async () => {
    mode = { required: true, goodKey: "admin.secret" };
    renderWithProviders(
      <RequireAuth>
        <div>app-content</div>
      </RequireAuth>,
    );
    const input = await screen.findByLabelText("API key");
    await userEvent.type(input, "admin.secret");
    await userEvent.click(screen.getByRole("button", { name: "Sign in" }));
    expect(await screen.findByText("app-content")).toBeInTheDocument();
  });

  it("rejects a wrong key and stays on the login page", async () => {
    mode = { required: true, goodKey: "admin.secret" };
    renderWithProviders(
      <RequireAuth>
        <div>app-content</div>
      </RequireAuth>,
    );
    const input = await screen.findByLabelText("API key");
    await userEvent.type(input, "wrong-key");
    await userEvent.click(screen.getByRole("button", { name: "Sign in" }));
    expect(
      await screen.findByText(/was not accepted/i),
    ).toBeInTheDocument();
    expect(screen.queryByText("app-content")).not.toBeInTheDocument();
  });
});
