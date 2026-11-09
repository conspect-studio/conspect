import { afterAll, afterEach, beforeAll, describe, expect, it } from "vitest";
import { http, HttpResponse } from "msw";
import { setupServer } from "msw/node";
import { createApiClient } from "./client";
import { createRelayManagementService } from "./relayManagementService";
const server = setupServer(http.post("*/api/v1/cloudflare/capabilities", () => HttpResponse.json({ data: { automatic_reconciliation: false } })));
beforeAll(() => { server.listen({ onUnhandledRequest: "error" }); });
afterEach(() => { server.resetHandlers(); });
afterAll(() => { server.close(); });
const credential = { accountId: "acct", reference: "vault:test", kind: "global_key" as const };
const service = () => createRelayManagementService(createApiClient({ baseUrl: "http://localhost" }));
describe("generated relay management adapter", () => {
  it.each([{ records: [], pending: "recovery" }, { records: null, pending: null }])("fails closed after stale successful inventory then invalid refresh %j", async (invalid) => {
    let broken = false;
    let mutations = 0;
    server.use(http.post("*/api/v1/cloudflare/capabilities", () => HttpResponse.json({ data: { automatic_reconciliation: true } })), http.post("*/api/v1/cloudflare/relays", () => HttpResponse.json({ data: { mode: "auto", desired_transport: "moq", ...(broken ? invalid : { records: [], pending: null }) } })), http.post("*/api/v1/cloudflare/set-mode", () => { mutations += 1; return HttpResponse.json({ data: {} }); }), http.post("*/api/v1/cloudflare/reconcile", () => { mutations += 1; return HttpResponse.json({ data: {} }); }));
    const api = service();
    await api.list(credential);
    expect(api.supportsAutomaticMode).toBe(true);
    broken = true;
    await expect(api.list(credential)).rejects.toThrow("pending");
    expect(api.supportsAutomaticMode).toBe(false);
    await expect(api.create(credential, "relay", "auto")).rejects.toThrow("unavailable");
    await expect(api.setMode(credential, "relay", "auto")).rejects.toThrow("unavailable");
    expect(mutations).toBe(0);
  });
  it("does not reuse validated automatic capability for another account or reference", async () => {
    server.use(http.post("*/api/v1/cloudflare/capabilities", () => HttpResponse.json({ data: { automatic_reconciliation: true } })), http.post("*/api/v1/cloudflare/relays", () => HttpResponse.json({ data: { mode: "auto", desired_transport: "moq", records: [], pending: null } })));
    const api = service();
    for (const changed of [{ ...credential, accountId: "other" }, { ...credential, reference: "vault:other" }]) {
      await api.list(credential);
      await expect(api.create(changed, "relay", "auto")).rejects.toThrow("unavailable");
      expect(api.supportsAutomaticMode).toBe(false);
    }
  });
  it("sends typed update config and standalone token revocation", async () => {
    server.use(http.post("*/api/v1/cloudflare/update", async ({ request }) => {
      expect(await request.json()).toEqual({ account_id: "acct", secret_ref: "vault:test", relay_id: "relay-id", name: "Changed", config: { upstreams: { enabled: true, upstreams: [{ url: "https://relay.example.com" }] } } });
      return HttpResponse.json({ data: {} });
    }), http.post("*/api/v1/cloudflare/revoke-token", async ({ request }) => {
      expect(await request.json()).toEqual({ account_id: "acct", secret_ref: "vault:test", relay_id: "relay-id", token_jti: "token-id" });
      return HttpResponse.json({ data: { revoked: true, connected_sessions_terminated: false } });
    }));
    await service().update(credential, "relay-id", { name: "Changed", enabled: true, urls: ["https://relay.example.com"] });
    await service().revokeToken(credential, "relay-id", "token-id");
  });
  it.each([403, 503])("sanitizes unauthorized and unavailable responses (%s)", async (status) => {
    server.use(http.post("*/api/v1/cloudflare/revoke-token", () => HttpResponse.json({ detail: "DO-NOT-ECHO-SECRET" }, { status })));
    await expect(service().revokeToken(credential, "relay-id", "token-id")).rejects.toThrow("Cloudflare operation unavailable");
  });
  it("rejects oversized or credential-bearing upstream requests before HTTP", async () => {
    for (const urls of [Array<string>(17).fill("https://relay.example.com"), ["https://user:pass@relay.example.com"], ["https://relay.example.com?token=secret"], ["https://relay.example.com/" + "x".repeat(2048)]]) {
      await expect(service().update(credential, "relay-id", { name: "Changed", enabled: true, urls })).rejects.toThrow();
    }
  });
  it("stores credentials through generated route and returns references only", async () => {
    server.use(http.post("*/api/v1/cloudflare/credentials", async ({ request }) => {
      expect(await request.json()).toEqual({ account_id: "acct", kind: "scoped_token", token: "input-secret" });
      return HttpResponse.json({ data: { secret_ref: "vault:saved" } });
    }));
    expect(await service().saveCredential("acct", { kind: "scoped_token", token: "input-secret" })).toEqual({ accountId: "acct", kind: "scoped_token", reference: "vault:saved" });
  });
  it("rejects incomplete recovery and never infers effective media from relay health", async () => {
    server.use(http.post("*/api/v1/cloudflare/relays", () => HttpResponse.json({ data: { mode: "manual", desired_transport: "moq", records: [{ relay: { uid: "id", name: "relay" }, health: "Reachable" }], pending: null } })));
    const api = service();
    expect(api.supportsAutomaticMode).toBe(false);
    expect((await api.list(credential))[0]?.effectiveTransport).toBeNull();
    await expect(api.setMode(credential, "id", "auto")).rejects.toThrow("unavailable");
    server.use(http.post("*/api/v1/cloudflare/relays", () => HttpResponse.json({ data: { mode: "manual", desired_transport: "moq", records: [], pending: "provision" } })));
    await expect(api.list(credential)).rejects.toThrow("pending");
  });
  it("enables automatic policy only after runtime capability detection", async () => {
    let changed = false;
    server.use(http.post("*/api/v1/cloudflare/capabilities", () => HttpResponse.json({ data: { automatic_reconciliation: true } })), http.post("*/api/v1/cloudflare/relays", () => HttpResponse.json({ data: { mode: "manual", desired_transport: "moq", records: [], pending: null } })), http.post("*/api/v1/cloudflare/set-mode", async ({ request }) => {
      expect(await request.json()).toEqual({ account_id: "acct", secret_ref: "vault:test", relay_id: "id", mode: "auto" });
      changed = true;
      return HttpResponse.json({ data: {} });
    }));
    const api = service();
    expect(api.supportsAutomaticMode).toBe(false);
    await api.list(credential);
    expect(api.supportsAutomaticMode).toBe(true);
    await api.setMode(credential, "id", "auto");
    expect(changed).toBe(true);
  });
  it("selects only selectable account-scoped MoQ permission", async () => {
    server.use(http.post("*/api/v1/cloudflare/permissions", () => HttpResponse.json({ data: [{ id: "wrong", name: "MoQ Write", scopes: ["com.cloudflare.api.account"], is_selectable: false }] })));
    await expect(service().createScopedToken(credential)).rejects.toThrow("permission unavailable");
    server.use(http.post("*/api/v1/cloudflare/permissions", () => HttpResponse.json({ data: [{ id: "right", name: "MoQ Write", scopes: ["com.cloudflare.api.account"], is_selectable: true }] })), http.post("*/api/v1/cloudflare/scoped-token", async ({ request }) => {
      expect(await request.json()).toEqual({ name: "Conspect MoQ", permission_group_id: "right", account_id: "acct", secret_ref: "vault:test" });
      return HttpResponse.json({ data: { secret_ref: "vault:scoped" } });
    }));
    expect((await service().createScopedToken(credential)).reference).toBe("vault:scoped");
  });
});
