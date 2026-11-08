import { describe, it, expect, vi } from "vitest";
import { render, screen, fireEvent, waitFor } from "@testing-library/react";
import { I18nProvider } from "../i18n/I18nProvider";
import { RelayManagementPage } from "./RelayManagementPage";
import { canCreateScopedToken, type RelayManagementService } from "./relayManagementModel";
function service(): RelayManagementService {
  return {
    supportsAutomaticMode: true,
    saveCredential: vi.fn(() => Promise.resolve({ reference: "vault:reference", accountId: "account", kind: "global_key" as const })),
    inspectTokens: vi.fn(() => Promise.resolve({ count: 49, allPagesLoaded: true })),
    createScopedToken: vi.fn(() => Promise.resolve({ reference: "vault:scoped", accountId: "account", kind: "scoped_token" as const })),
    list: vi.fn(() => Promise.resolve([{ id: "relay-1", name: "Relay one", mode: "manual" as const, requestedTransport: "moq", effectiveTransport: null, status: "failed", errorCode: "provider" }])),
    create: vi.fn(() => Promise.resolve()), importRelay: vi.fn(() => Promise.resolve()), setMode: vi.fn(() => Promise.resolve()), teardown: vi.fn(() => Promise.resolve()),
  };
}
async function onboard(api: RelayManagementService) {
  render(<I18nProvider><RelayManagementPage service={api} /></I18nProvider>);
  fireEvent.change(screen.getByLabelText("Account ID"), { target: { value: "account" } });
  fireEvent.change(screen.getByLabelText("API secret"), { target: { value: "super-secret" } });
  fireEvent.click(screen.getByRole("button", { name: "Store credential securely" }));
  await screen.findByText("vault:reference");
}
describe("relay management", () => {
  it("requires complete count below 50", () => {
    expect(canCreateScopedToken(null)).toBe(false);
    expect(canCreateScopedToken({ count: 1, allPagesLoaded: false })).toBe(false);
    expect(canCreateScopedToken({ count: 50, allPagesLoaded: true })).toBe(false);
    expect(canCreateScopedToken({ count: -1, allPagesLoaded: true })).toBe(false);
    expect(canCreateScopedToken({ count: 49, allPagesLoaded: true })).toBe(true);
  });
  it("clears secrets, creates scoped token only on explicit action, and separates transports", async () => {
    const api = service(); await onboard(api);
    expect(screen.getByLabelText("API secret")).toHaveValue("");
    expect(api.createScopedToken).not.toHaveBeenCalled();
    const create = screen.getByRole("button", { name: "Create account-only scoped token" });
    expect(create).toBeDisabled();
    fireEvent.click(screen.getByRole("button", { name: "Check all token pages" }));
    await waitFor(() => expect(create).toBeEnabled());
    expect(api.createScopedToken).not.toHaveBeenCalled();
    fireEvent.click(create);
    await waitFor(() => { expect(api.createScopedToken).toHaveBeenCalledOnce(); });
    expect(screen.getByText("Requested transport")).toBeInTheDocument();
    expect(screen.getByText("Unverified")).toBeInTheDocument();
  });
  it("accepts global keys and applies create, import, and mode changes", async () => {
    const api = service();
    render(<I18nProvider><RelayManagementPage service={api} /></I18nProvider>);
    fireEvent.change(screen.getByLabelText("Account ID"), { target: { value: "account" } });
    fireEvent.change(screen.getByLabelText("Credential type"), { target: { value: "global_key" } });
    fireEvent.change(screen.getByLabelText("Cloudflare email"), { target: { value: "owner@example.com" } });
    fireEvent.change(screen.getByLabelText("API secret"), { target: { value: "global-secret" } });
    fireEvent.click(screen.getByRole("button", { name: "Store credential securely" }));
    await screen.findByText("vault:reference");
    expect(api.saveCredential).toHaveBeenCalledWith("account", { kind: "global_key", email: "owner@example.com", key: "global-secret" });
    fireEvent.change(screen.getByLabelText("Relay name"), { target: { value: "New relay" } });
    fireEvent.click(screen.getByRole("button", { name: "Create relay" }));
    await waitFor(() => { expect(api.create).toHaveBeenCalledWith(expect.anything(), "New relay", "manual"); });
    await waitFor(() => { expect(screen.getByRole("button", { name: "Import relay" })).toBeEnabled(); });
    fireEvent.change(screen.getByLabelText("Existing relay ID"), { target: { value: "existing-id" } });
    fireEvent.click(screen.getByRole("button", { name: "Import relay" }));
    await waitFor(() => { expect(api.importRelay).toHaveBeenCalledWith(expect.anything(), "existing-id"); });
    await waitFor(() => { expect(screen.getByRole("button", { name: "Import relay" })).toBeEnabled(); });
    const relayMode = screen.getAllByLabelText("Management mode")[1];
    if (!relayMode) throw new Error("relay mode control missing");
    fireEvent.change(relayMode, { target: { value: "auto" } });
    await waitFor(() => { expect(api.setMode).toHaveBeenCalledWith(expect.anything(), "relay-1", "auto"); });
  });
  it("requires teardown confirmation", async () => {
    const api = service(); await onboard(api);
    fireEvent.click(await screen.findByRole("button", { name: "Teardown relay" }));
    expect(api.teardown).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: "Confirm teardown" }));
    await waitFor(() => { expect(api.teardown).toHaveBeenCalledWith(expect.anything(), "relay-1"); });
  });
  it("does not echo provider errors or secret input", async () => {
    const api = service(); api.saveCredential = vi.fn(() => Promise.reject(new Error("super-secret")));
    render(<I18nProvider><RelayManagementPage service={api} /></I18nProvider>);
    fireEvent.change(screen.getByLabelText("Account ID"), { target: { value: "account" } });
    fireEvent.change(screen.getByLabelText("API secret"), { target: { value: "super-secret" } });
    fireEvent.click(screen.getByRole("button", { name: "Store credential securely" }));
    expect(await screen.findByRole("alert")).not.toHaveTextContent("super-secret");
    expect(screen.getByLabelText("API secret")).toHaveValue("");
  });
});
