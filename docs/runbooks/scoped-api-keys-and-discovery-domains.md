# Runbook — scoped API keys + discovery domains (ADR-W026)

Operational "how" for the discovery-scope authorization axis: minting a
**scoped, non-admin API key** and labelling a node's **discovery domain**. The
"why" is [ADR-W026](../decisions/ADR-W026.md).

## Label a node's discovery domain

Each node stamps its own operator-declared domain onto every discovery row it
observes — the sole source of the authz label (never the responder's payload):

```toml
[discovery]
domain = "site-a"        # DNS-label-like: non-empty, ≤64 chars, [a-z0-9-]
```

- **Absent** ⇒ this node's discovery rows are **unlabelled**; a discovery-scoped
  principal is **denied** them (fail-closed). Unscoped principals (every existing
  key) still see everything.
- One node = one domain (per-NIC/per-VLAN split is out of scope).
- Reachable via config-as-code: `GET/PUT /api/v1/config` (import/export/apply).

## Mint a scoped API key

Config-declared keys register at startup beside the bootstrap admin. The secret
is referenced by an **environment-variable name** — never inlined (rule 34):

```toml
[[api.keys]]
key_id = "site-a-operator"
secret_env = "MULTIVIEW_KEY_SITE_A"        # the env var holding the secret
role = "operator"                          # read_only | viewer | operator (NO admin)
scoped_object_ids = ["cam-3"]              # optional; absent = unscoped on axis
scoped_output_ids = ["out-1", "program:main"]
scoped_discovery_domains = ["site-a"]
```

Then set the secret and start:

```bash
export MULTIVIEW_KEY_SITE_A="$(op read op://Multiview/site-a-operator/secret)"
multiview run --config multiview.toml
# The bearer token the key presents is  <key_id>.<secret>
#   Authorization: Bearer site-a-operator.<secret>
```

- A key whose `secret_env` is **unset or empty** is a **hard startup error** (an
  un-authenticatable scoped key is a latent misconfiguration) — the daemon
  refuses to start and names the missing variable.
- `key_id`s must be unique; a bare `program:` grant (no program id) is rejected;
  an **empty** entry in any scope allowlist is rejected.
- There is **no `admin` role** for config keys — admin authentication is
  environment-only (the bootstrap `MULTIVIEW_CONTROL_TOKEN`, always unscoped), so
  config-as-code can never mint an administrator. Reusing the reserved `key_id =
  "admin"` (or any already-registered id) is a **hard startup error**, never a
  silent overwrite of the bootstrap admin.

## The three scope axes (what each confines)

| Axis | Config field | Confines |
|------|--------------|----------|
| Object | `scoped_object_ids` | device / cast-session / media-player / input-bound-tile events + their REST reads (BOLA) |
| Output | `scoped_output_ids` (plain entries) | output-sink events (`rist.link.stats`) + per-output REST |
| Program | `scoped_output_ids` `program:<id>` entries | `timing.status` for that program |
| Discovery | `scoped_discovery_domains` | `device.discovered` + `GET /discovery/devices` + `POST /scan` |

`program:main` and a plain output `main` are **distinct** grants (the `program:`
prefix is reserved) — grant timing explicitly with `program:main`.

## Common remediations (post-upgrade "it broke" symptoms)

- **A scoped key sees empty discovery.** The node has no `[discovery] domain`, or
  its domain is not in the key's `scoped_discovery_domains`. Add
  `[discovery] domain = "<site>"` and/or the domain to the key (fail-closed by
  design — an unlabelled row is denied to a discovery-scoped key).
- **An output-scoped key stopped receiving `timing.status`.** Add `program:main`
  (the program stream id, `ProgramId::MAIN`) to its `scoped_output_ids`.
- **A key can't scan.** `POST /discovery/devices/scan` is gated on the same
  domain axis: a principal that could not see the results may not spend the
  single-flight scan budget.

Scope changes on a store-managed key take effect on live WS/SSE sessions within
one delta / ≤5 s (RT010) once re-registered; today a config `[[api.keys]]` edit
re-registers on restart (live file-watch re-registration is a tracked follow-up).
