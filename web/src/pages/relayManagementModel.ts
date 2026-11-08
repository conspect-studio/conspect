/** Presentation contract only. Integration supplies generated-client adapters.
 * Never persist onboarding inputs in browser storage or query caches.
 */
export type CredentialInput =
  | { kind: "global_key"; email: string; key: string }
  | { kind: "scoped_token"; token: string };
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
  teardown: (credential: CredentialView, id: string) => Promise<void>;
}
