# multiview-mesh — agent notes

Conspect local-mesh discovery + relay plane (ADR-0051): mDNS announce/browse of salted, signed summaries, untrusted peer inventory, mesh role determination, end-to-end-signed relay carrier.
Inv #10: control-plane actor only, no `multiview-engine` dependency; bounded drop-oldest queues; announce/browse never blocks on the network.
Data minimisation — PINNED BY TEST (`tests/announce_payload.rs`): announce payload carries only `{protocol_version, digests, claim_state, entitlement, signature}` — never a serial/MAC/URL/hostname/config. Never weaken that test.
`DiscoveryMode` has exactly one value (`AlwaysOn`) — do not add a discovery off-switch. Only relay opt-in toggles.
Untrusted inventory: a discovered peer is never auto-trusted/auto-relayed; relayer is a dumb carrier, verification is against the pinned server key, never the relayer's.
IPv6-first (ADR-0042): mDNS multicast `ff02::fb` primary, IPv4 `224.0.0.251` legacy interop only.
Read first: [ADR-0051](../../docs/decisions/ADR-0051.md).
