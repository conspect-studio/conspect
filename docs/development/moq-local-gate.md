# MoQ component branch: proportional local gate

This task-specific `bin/check` does not replace CI or assert a full suite passed
on the final commit. Missing local historical evidence fails closed. CI must
continue its ordinary full workspace suite; fresh clones cannot use this local
evidence exception. No receipt, hook, global configuration, or test is bypassed.
The shipping owner authorized this gate to respect the two-full-suite budget.
An independent vendor must review the gate before its execution through the
normal local-gate wrapper. The script cannot certify its own review.

## Dated full-suite evidence

Baseline: `22661b9f479fabc097e9f70eea844964a49a39f3`.
The integration owner recovered the original tool history, job
`bash-4b73d7db3d3642db`, completion 2026-11-08T16:34:53Z, exit 0:

```sh
set -o pipefail
git rev-parse HEAD > target/moq-gates/workspace-test.sha
df -h .
cargo test --locked --workspace -j2 > target/moq-gates/workspace-test.log 2>&1
result=$?
printf '%s\n' "$result" > target/moq-gates/workspace-test.exit
tail -n 80 target/moq-gates/workspace-test.log
exit "$result"
```

This command provenance is owner-reported original tool history, not a new or
synthetic execution record. The log itself does not print the command. The gate
pins the actual complete log SHA256
`594918d1d9dbeb0e559f26bc8ee2d54a4007042e8bff658976a5359ff5720969`,
checks the SHA and zero exit sidecars, all result lines for success and zero
filtered tests, the formerly problematic tile budget test passing, and the
last xtask doctest completion. It does not infer final-HEAD full-suite coverage.
These local artifacts are not signed attestations; their exact bytes are pinned
in the independently reviewed gate. Do not reconstruct missing artifacts.

## Exact approved delta

Only this ordered immutable commit chain follows the baseline:

1. `45349ebf3b17bffc4c8cf788f1cbbc3efcad52d5`: dependency advisory/license repair,
   native TLS sources unchanged; wasm trust-anchor policy has one stricter `.tr`
   constraint. Targeted MoQ tests and root identity comparison are rerun.
2. `4c6c0613240b094ad32bfe98ed91d07fa8512141`: independently reviewed provenance
   clarification; no runtime changes.
3. `edc6daa12fed4cba5184e053befe5205bc33b095`: standalone fixture H264 parser;
   the fixture package tests are rerun, not presumed covered by the old suite.
4. `26dbbfd55b2edf2466e4f5c23b52d5c4dd21c6a6`: test generators and actual H264
   browser evidence. Existing live recordings remain dated evidence, not a new
   live run. Web lint/build recheck the affected frontend artifacts.

5. `fa4389cddeeeec80679fd88bbc5a51f174b43eee`: initial independently reviewed
   proportional gate. Its real wrapper run failed optional-feature Clippy in
   `moq_provider.rs` (documentation and match-style lints). This was NOT a gate
   pass; no successful receipt is claimed for that execution.
6. `c6654f1b5b3f7f6c4213b2004bf6020301660590`: behavior-equivalent, one-file
   optional-provider lint repair. Owner reports strict optional-feature Clippy
   and six focused tests passed. The gate still reruns its complete command list.

Only one gate-amendment commit may follow, with exactly the two policy paths
`bin/check` and this document changed, and a single parent equal to the exact
repair SHA. Both historical policy blobs are pinned; the provider must equal
blob `2db829b472b1ecfa749704118bab85f82e7d253a`. The repair itself must change
only that provider path. The complete baseline-to-repair commit chain is exact,
not a path wildcard. The whole working tree must be clean including untracked
files. Extra commits, merge parents, runtime changes or a different provider
blob fail closed. Further changes require a new reviewed gate/budget decision.
Independent review binds this gate amendment externally; the script cannot
self-approve its own changed bytes. Review amendments retain the same parent.

## Commands and limits

The executable `COMMANDS` list is authoritative: workspace fmt/check/strict
all-target Clippy, dependency audit, optional MoQ control feature check/Clippy,
MoQ/Cloudflare/vault/controller targeted tests, standalone fixture tests, root
bundle comparison, web lint/build, and probe syntax. Every command actually runs
and any failure stops the gate. The final HEAD and clean-tree checks run again
afterward. No full workspace test command runs a third time.

`bin/check --self-test` checks real evidence plus six evidence corruptions and
four policy-boundary failures (extra commit, merge, runtime path, provider blob);
`--validate-only` checks historical evidence/ancestry/cleanliness without builds.
Neither is a receipt-generating successful full gate: invoke the normal wrapper
without these flags only after independent review. The branch still lacks the
production shared-feed factory and paid/AV-sync acceptance. Green local checks
do not establish CI green, merge, or whole-feature delivery.
