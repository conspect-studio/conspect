# Canonical source-registration contract — WEB CONFIRMATION PENDING

Status: fail closed; NO LIVE HARNESS until web DTO mapping closes and an independent review names the new exact commit SHA. Paid delivery remains OFF. MC02/MG02 wire bytes do not change.

Source pins (repository-relative):

- `tools/media-crypto/src/bootstrap.rs`: `Bootstrap::parse`, `mapping`, `validate_expected`, `PinnedRootTransport`. The trusted bootstrap and fresh fragment must carry the same five raw shared identities: `contributorOrg`, `feed`, `version`, `publisherMachine`, `publisherInstance`. Do not derive raw identities from opaque HTTP strings. Do not infer machine/instance equality.
- `tools/media-crypto/src/authority_http.rs`: `PublisherLease`, `HttpAuthorityRootProvider::current_epoch`, `adopt_generation`. One actual lease owns acquisition/renewal/release. Its generation must enter the provider high-water before root fetch. Root DTO requires `contributorOrgCryptoId`, `versionCryptoId`, `publisherMachineCryptoId`, `feedCryptoId`, `epochStartMs`, `epochEndMs`, plus canonical generation/key/lease fields. These are REQUIRED candidate fields, NOT evidence the deployed web DTO emits them.
- `tools/native-exchange-runtime/src/main.rs`: one retained `AuthorityRootGuard` with the same lease fence spans publisher rebuilds; no fabricated permit or second lease actor.
- `tools/native-exchange-runtime/src/lib.rs`: verified epoch transaction precedes new receiver installation/decrypt; authenticated object counter transaction precedes plaintext return. Failed transactions poison reconnect.
- `tools/media-crypto/src/spool.rs`: RS05 local journal adds durable server-accounting high-water to exact receipt ID/sequence/payload and media cursor. No network wire change. Old local formats fail closed. ACK validation and exact pending removal share one fsync/rename transaction. Server receipt deduplication remains mandatory; local durability does not replace it.

The command stores durable journals beside the bootstrap manifest in its `.runtime-state` sibling directory (owner-only, never a symlink), not in the temporary FFmpeg directory. Preserve these journals on restart. A different bootstrap mapping/run fails closed against existing state. Never delete state as reconnect recovery. FFmpeg faults return static errors without command/stderr/token logs. Coordinator shutdown precedes RAII lease release; verified HTTP transport bounds each request.

Web closure must confirm the actual trusted registration provenance, all exact field names and raw byte mappings, machine versus publisher-instance semantics, epoch window fields, and server stable-ID/sequence deduplication. Synthetic local TLS DTOs do not establish deployed compatibility.
