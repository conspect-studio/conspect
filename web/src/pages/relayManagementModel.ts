/** Presentation contract only. Integration supplies generated-client adapters.
 * Never persist onboarding inputs in browser storage or query caches.
 */
export type CredentialInput =
  | { kind: "global_key"; email: string; key: string }
  | { kind: "scoped_token"; token: string };
export interface RelayConfigInput { name: string; enabled: boolean; urls: string[] }
export function validateRelayConfig(input: RelayConfigInput): void {
  if (!input.name.trim() || input.name.length > 128 || input.urls.length > 16) throw new Error("Invalid relay configuration");
  for (const value of input.urls) {
    if (value.length > 2048) throw new Error("Invalid upstream URL");
    const url = new URL(value);
    if (!["https:", "moqt:"].includes(url.protocol) || !url.hostname || url.username || url.password || url.search || url.hash) throw new Error("Invalid upstream URL");
  }
  if (input.enabled && input.urls.length === 0) throw new Error("Upstreams required");
}
export interface RelayView {
  id: string;
  name: string;
  mode: "auto" | "manual";
  requestedTransport: string;
  effectiveTransport: string | null;
  status: string;
  errorCode?: string;
}
export interface CredentialView { reference: string; accountId: string; kind: CredentialInput["kind"] }
export interface TokenInventory { count: number; allPagesLoaded: boolean }
export function canCreateScopedToken(inventory: TokenInventory | null): boolean {
  return inventory !== null && inventory.allPagesLoaded && Number.isSafeInteger(inventory.count) && inventory.count >= 0 && inventory.count < 50;
}
export interface RelayManagementService {
  /** True only when the server actually runs automatic reconciliation. */
  supportsAutomaticMode?: boolean;
  saveCredential: (accountId: string, input: CredentialInput) => Promise<CredentialView>;
  inspectTokens: (credential: CredentialView) => Promise<TokenInventory>;
  /** Must recheck all pages at mutation time and enforce this account only. */
  createScopedToken: (credential: CredentialView) => Promise<CredentialView>;
  list: (credential: CredentialView) => Promise<RelayView[]>;
  create: (credential: CredentialView, name: string, mode: RelayView["mode"]) => Promise<void>;
  importRelay: (credential: CredentialView, id: string) => Promise<void>;
  setMode: (credential: CredentialView, id: string, mode: RelayView["mode"]) => Promise<void>;
  update: (credential: CredentialView, id: string, config: RelayConfigInput) => Promise<void>;
  revokeToken: (credential: CredentialView, id: string, tokenId: string) => Promise<void>;
  teardown: (credential: CredentialView, id: string) => Promise<void>;
}
