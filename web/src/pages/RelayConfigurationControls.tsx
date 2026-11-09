import { useState } from "react";
import { Trans, useLingui } from "@lingui/react/macro";
import { Button } from "../components/ui/button";
import { Input } from "../components/ui/input";
import { validateRelayConfig, type CredentialView, type RelayManagementService, type RelayView } from "./relayManagementModel";

/** No provider URLs or token secrets are prefilled into these request controls. */
export function RelayConfigurationControls({ relay, credential, service, disabled, run, refresh }: {
  relay: RelayView; credential: CredentialView; service: RelayManagementService; disabled: boolean;
  run: (action: () => Promise<void>) => Promise<void>; refresh: () => Promise<void>;
}) {
  const { t } = useLingui();
  const [name, setName] = useState(relay.name);
  const [enabled, setEnabled] = useState(false);
  const [upstreams, setUpstreams] = useState("");
  const [tokenId, setTokenId] = useState("");
  const [confirmedId, setConfirmedId] = useState<string | null>(null);
  const [revoked, setRevoked] = useState(false);
  return <section aria-label={t`Relay configuration and tokens`} className="space-y-3">
    <p><Trans>Requested configuration only. Saving replaces all upstreams; it does not prove media health. Use at most 16 HTTPS or MoQT URLs, without credentials, query strings, or fragments.</Trans></p>
    <form onSubmit={(event) => { event.preventDefault(); void run(async () => {
      const config = { name, enabled, urls: upstreams.split("\n").map((url) => url.trim()).filter(Boolean) };
      validateRelayConfig(config);
      await service.update(credential, relay.id, config); await refresh();
    }); }}>
      <label htmlFor={`rename-${relay.id}`}><Trans>Requested relay name</Trans></label>
      <Input id={`rename-${relay.id}`} value={name} required maxLength={128} disabled={disabled} onChange={(event) => { setName(event.target.value); }} />
      <label><input type="checkbox" checked={enabled} disabled={disabled} onChange={(event) => { setEnabled(event.target.checked); }} /><Trans>Enable requested upstreams</Trans></label>
      <label htmlFor={`upstreams-${relay.id}`}><Trans>Requested upstream URLs, one per line</Trans></label>
      <textarea id={`upstreams-${relay.id}`} value={upstreams} maxLength={32783} rows={4} disabled={disabled} autoComplete="off" onChange={(event) => { setUpstreams(event.target.value); }} />
      <Button type="submit" disabled={disabled}><Trans>Apply requested configuration</Trans></Button>
    </form>
    <label htmlFor={`token-${relay.id}`}><Trans>Relay token ID to revoke (not the secret)</Trans></label>
    <Input id={`token-${relay.id}`} value={tokenId} maxLength={256} autoComplete="off" disabled={disabled || confirmedId !== null} onChange={(event) => { setTokenId(event.target.value); setRevoked(false); }} />
    {confirmedId === null ? <Button disabled={disabled || !/^[A-Za-z0-9_-]{1,256}$/.test(tokenId)} onClick={() => { setConfirmedId(tokenId); }}><Trans>Revoke relay token</Trans></Button> : <div>
      <p><Trans>Revoke this token for this relay? New connections lose access. Existing connected sessions are not terminated.</Trans></p>
      <p>{relay.id} / {confirmedId}</p>
      <Button disabled={disabled} onClick={() => void run(async () => { await service.revokeToken(credential, relay.id, confirmedId); setConfirmedId(null); setTokenId(""); setRevoked(true); })}><Trans>Confirm token revocation</Trans></Button>
      <Button disabled={disabled} onClick={() => { setConfirmedId(null); }}><Trans>Cancel token revocation</Trans></Button>
    </div>}
    {revoked && <p role="status"><Trans>Token revoked. Existing connected sessions are not terminated.</Trans></p>}
  </section>;
}
