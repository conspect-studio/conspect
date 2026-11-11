import { z } from "zod";
import { createApiClient, type MultiviewApiClient } from "./client";
import type { components } from "./types";
import { validateRelayConfig, type CredentialView, type RelayManagementService } from "../pages/relayManagementModel";

// The generated envelope deliberately exposes unknown JSON. Validate data at
// this boundary; never assume management health proves media connectivity.
const reference = z.object({ secret_ref: z.string().startsWith("vault:") });
const inventory = z.object({ tokens: z.array(z.unknown()), all_pages_loaded: z.boolean() });
const permission = z.object({ id: z.string(), name: z.string(), scopes: z.array(z.string()), is_selectable: z.boolean().nullable().optional() });
const capabilities = z.object({ automatic_reconciliation: z.boolean() });
const records = z.object({ mode: z.enum(["auto", "manual"]), desired_transport: z.string(), records: z.array(z.object({
  relay: z.object({ uid: z.string(), name: z.string(), status: z.unknown().optional() }),
  health: z.string(),
})).nullable(), pending: z.string().nullable() });
export function createRelayManagementService(client: MultiviewApiClient = createApiClient()): RelayManagementService {
  let automatic = false;
  let validatedIdentity: string | null = null;
  let generation = 0;
  const identity = (credential: CredentialView) => JSON.stringify([credential.accountId, credential.reference]);
  function invalidate() { automatic = false; validatedIdentity = null; generation += 1; }
  function requireAutomatic(credential: CredentialView) {
    if (!automatic || validatedIdentity !== identity(credential)) {
      invalidate();
      throw new Error("Automatic mode unavailable");
    }
  }
  async function action(credential: CredentialView, name: string, extra: Partial<components["schemas"]["CloudflareRequest"]> = {}) {
    if (validatedIdentity !== null && validatedIdentity !== identity(credential)) invalidate();
    const response = await client.POST("/api/v1/cloudflare/{action}", {
      params: { path: { action: name } },
      body: { ...extra, account_id: credential.accountId, secret_ref: credential.reference },
    });
    if (!response.response.ok) throw new Error("Cloudflare operation unavailable");
    if (!response.data) throw new Error("Cloudflare response unavailable");
    return response.data.data;
  }
  return {
    get supportsAutomaticMode() { return automatic; },
    async saveCredential(accountId, input) {
      invalidate();
      const response = await client.POST("/api/v1/cloudflare/credentials", { body: { account_id: accountId, ...input } });
      if (!response.response.ok || !response.data) throw new Error("Credential storage unavailable");
      return { accountId, kind: input.kind, reference: reference.parse(response.data.data).secret_ref };
    },
    async inspectTokens(credential) {
      const result = inventory.parse(await action(credential, "audit"));
      return { count: result.tokens.length, allPagesLoaded: result.all_pages_loaded };
    },
    async createScopedToken(credential) {
      invalidate();
      const groups = z.array(permission).parse(await action(credential, "permissions"));
      const selected = groups.find((group) => (group.name === "MoQ Write" || group.name === "MoQ Edit") && group.is_selectable === true && group.scopes.includes("com.cloudflare.api.account"));
      if (!selected) throw new Error("Required account-only permission unavailable");
      const result = reference.parse(await action(credential, "scoped-token", { name: "Conspect MoQ", permission_group_id: selected.id }));
      return { ...credential, reference: result.secret_ref, kind: "scoped_token" };
    },
    async list(credential) {
      invalidate();
      const refreshGeneration = generation;
      const supported = capabilities.parse(await action(credential, "capabilities")).automatic_reconciliation;
      const result = records.parse(await action(credential, "relays"));
      if (result.pending !== null || result.records === null) throw new Error("Relay recovery pending");
      if (refreshGeneration !== generation) throw new Error("Relay inventory superseded");
      validatedIdentity = identity(credential);
      automatic = supported;
      return result.records.map((record) => ({
        id: record.relay.uid, name: record.relay.name, mode: result.mode,
        requestedTransport: result.desired_transport, effectiveTransport: null,
        status: record.health, ...(record.health === "Unavailable" ? { errorCode: "unavailable" } : {}),
      }));
    },
    async create(credential, name, mode) {
      if (mode === "auto") {
        requireAutomatic(credential);
        await action(credential, "set-mode", { mode, name });
        requireAutomatic(credential);
        await action(credential, "reconcile");
      } else {
        await action(credential, "provision", { name });
      }
    },
    async importRelay(credential, id) { await action(credential, "import", { relay_id: id }); },
    async setMode(credential, id, mode) {
      if (mode === "auto") requireAutomatic(credential);
      await action(credential, "set-mode", { relay_id: id, mode });
    },
    async update(credential, id, config) {
      validateRelayConfig(config);
      await action(credential, "update", { relay_id: id, name: config.name, config: { upstreams: { enabled: config.enabled, upstreams: config.urls.map((url) => ({ url })) } } });
    },
    async revokeToken(credential, id, tokenId) {
      if (!/^[A-Za-z0-9_-]{1,256}$/.test(tokenId)) throw new Error("Invalid token ID");
      z.object({ revoked: z.literal(true), connected_sessions_terminated: z.literal(false) }).parse(await action(credential, "revoke-token", { relay_id: id, token_jti: tokenId }));
    },
    async teardown(credential, id) { await action(credential, "teardown", { relay_id: id }); },
  };
}
