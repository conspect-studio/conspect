import { useState } from "react";
import { Trans, useLingui } from "@lingui/react/macro";
import { Button } from "../components/ui/button";
import { Input } from "../components/ui/input";
import { PageHeader } from "../components/PageHeader";
import { createRelayManagementService } from "../api/relayManagementService";
import { RelayConfigurationControls } from "./RelayConfigurationControls";
import { canCreateScopedToken, type CredentialView, type RelayManagementService, type RelayView, type TokenInventory } from "./relayManagementModel";

/** All mutations use an injected generated-client adapter. Secrets only live in
 * the form until submission; API errors are deliberately not echoed (providers
 * can include submitted secrets in error bodies).
 */
export function RelayManagementPage({ service: injectedService }: { service?: RelayManagementService }) {
  const [defaultService] = useState(() => createRelayManagementService());
  const service = injectedService ?? defaultService;
  const { t } = useLingui();
  const [kind, setKind] = useState<"global_key" | "scoped_token">("scoped_token");
  const [account, setAccount] = useState("");
  const [email, setEmail] = useState("");
  const [secret, setSecret] = useState("");
  const [credential, setCredential] = useState<CredentialView | null>(null);
  const [inventory, setInventory] = useState<TokenInventory | null>(null);
  const [relays, setRelays] = useState<RelayView[]>([]);
  const [name, setName] = useState("");
  const [importId, setImportId] = useState("");
  const [mode, setMode] = useState<RelayView["mode"]>("manual");
  const [busy, setBusy] = useState(false);
  const [failed, setFailed] = useState(false);
  const [confirmDelete, setConfirmDelete] = useState<string | null>(null);
  async function run(action: () => Promise<void>) {
    if (busy) return;
    setBusy(true); setFailed(false);
    try { await action(); } catch { setFailed(true); } finally { setBusy(false); }
  }
  async function refresh(current: CredentialView) { setRelays(await service.list(current)); }
  return <main className="space-y-6 p-6">
    <PageHeader title={t`Cloudflare media relays`} />
    <p><Trans>Relay credentials use encrypted server storage. The server needs an external master key; no plaintext configuration fallback exists.</Trans></p>
    <p><Trans>Relay changes are control-plane operations. A transport change can reset media connections. A shared relay does not provide per-source privacy.</Trans></p>
    {failed && <p role="alert"><Trans>The operation failed. Check server diagnostics and permissions, then retry. No successful change is assumed.</Trans></p>}
    <form className="space-y-3" onSubmit={(event) => { event.preventDefault(); const submitted = secret; setSecret(""); void run(async () => {
      const saved = await service.saveCredential(account, kind === "global_key" ? { kind, email, key: submitted } : { kind, token: submitted });
      setCredential(saved); setInventory(null); setRelays([]); await refresh(saved);
    }); }}>
      <h2><Trans>Credential onboarding</Trans></h2>
      <label htmlFor="cf-account"><Trans>Account ID</Trans><Input id="cf-account" value={account} onChange={(e) => { setAccount(e.target.value); }} required disabled={busy} /></label>
      <label><Trans>Credential type</Trans><select value={kind} disabled={busy} onChange={(e) => { setKind(e.target.value === "global_key" ? "global_key" : "scoped_token"); setSecret(""); }}>
        <option value="scoped_token">{t`Scoped API token`}</option><option value="global_key">{t`Global API key and email`}</option>
      </select></label>
      {kind === "global_key" && <label htmlFor="cf-email"><Trans>Cloudflare email</Trans><Input id="cf-email" type="email" autoComplete="off" value={email} onChange={(e) => { setEmail(e.target.value); }} required disabled={busy} /></label>}
      <label htmlFor="cf-secret"><Trans>API secret</Trans><Input id="cf-secret" type="password" autoComplete="new-password" value={secret} onChange={(e) => { setSecret(e.target.value); }} required disabled={busy} /></label>
      <Button disabled={busy} type="submit"><Trans>Store credential securely</Trans></Button>
    </form>
    {credential && <section className="space-y-4" aria-label={t`Relay management`}>
      <p><Trans>Stored credential reference:</Trans> <code>{credential.reference}</code></p>
      {credential.kind === "global_key" && <div className="space-y-2">
        <p><Trans>A scoped token is optional. Creation requires a complete inventory below 50 tokens and explicit consent. Permissions apply only to the selected account.</Trans></p>
        <Button disabled={busy} onClick={() => { setInventory(null); void run(async () => { setInventory(await service.inspectTokens(credential)); }); }}><Trans>Check all token pages</Trans></Button>
        {inventory && <p>{inventory.allPagesLoaded ? t`Complete token count: ${inventory.count}` : t`Token inventory incomplete; creation disabled`}</p>}
        <Button disabled={busy || !canCreateScopedToken(inventory)} onClick={() => { setInventory(null); void run(async () => { const scoped = await service.createScopedToken(credential); setCredential(scoped); await refresh(scoped); }); }}><Trans>Create account-only scoped token</Trans></Button>
      </div>}
      {!service.supportsAutomaticMode && <p><Trans>Automatic reconciliation is unavailable. Relay creation and deletion require explicit actions.</Trans></p>}
      <form onSubmit={(e) => { e.preventDefault(); void run(async () => { await service.create(credential, name, mode); setName(""); await refresh(credential); }); }}>
        <label htmlFor="cf-name"><Trans>Relay name</Trans><Input id="cf-name" value={name} onChange={(e) => { setName(e.target.value); }} required disabled={busy} /></label>
        <label><Trans>Management mode</Trans><select disabled={busy || !service.supportsAutomaticMode} value={mode} onChange={(e) => { setMode(e.target.value === "auto" ? "auto" : "manual"); }}><option value="manual">{t`Manual`}</option><option value="auto">{t`Automatic`}</option></select></label>
        <Button type="submit" disabled={busy}><Trans>Create relay</Trans></Button>
      </form>
      <form onSubmit={(e) => { e.preventDefault(); void run(async () => { await service.importRelay(credential, importId); setImportId(""); await refresh(credential); }); }}>
        <label htmlFor="cf-import"><Trans>Existing relay ID</Trans><Input id="cf-import" value={importId} onChange={(e) => { setImportId(e.target.value); }} required disabled={busy} /></label>
        <Button type="submit" disabled={busy}><Trans>Import relay</Trans></Button>
      </form>
      <Button disabled={busy} onClick={() => void run(() => refresh(credential))}><Trans>Refresh relay status</Trans></Button>
      <ul className="space-y-4">{relays.map((relay) => <li key={relay.id} className="rounded border p-4">
        <h3>{relay.name}</h3><p>{relay.id}</p>
        <dl><dt><Trans>Status</Trans></dt><dd>{relay.status}</dd><dt><Trans>Requested transport</Trans></dt><dd>{relay.requestedTransport}</dd><dt><Trans>Effective transport</Trans></dt><dd>{relay.effectiveTransport ?? t`Unverified`}</dd></dl>
        {relay.errorCode && <p role="alert"><Trans>Relay reports an error. Check server diagnostics.</Trans></p>}
        <label><Trans>Management mode</Trans><select value={relay.mode} disabled={busy || !service.supportsAutomaticMode} onChange={(e) => { const next = e.target.value === "auto" ? "auto" : "manual"; void run(async () => { await service.setMode(credential, relay.id, next); await refresh(credential); }); }}><option value="manual">{t`Manual`}</option><option value="auto">{t`Automatic`}</option></select></label>
        <RelayConfigurationControls key={`${credential.reference}:${relay.id}`} relay={relay} credential={credential} service={service} disabled={busy} run={run} refresh={() => refresh(credential)} />
        {confirmDelete === relay.id ? <div><p><Trans>Delete this relay? Active clients can disconnect. Imported resources also require explicit deletion.</Trans></p><Button disabled={busy} onClick={() => void run(async () => { await service.teardown(credential, relay.id); setConfirmDelete(null); await refresh(credential); })}><Trans>Confirm teardown</Trans></Button><Button disabled={busy} onClick={() => { setConfirmDelete(null); }}><Trans>Cancel</Trans></Button></div> : <Button disabled={busy} onClick={() => { setConfirmDelete(relay.id); }}><Trans>Teardown relay</Trans></Button>}
      </li>)}</ul>
    </section>}
    {busy && <p role="status"><Trans>Operation in progress</Trans></p>}
  </main>;
}
