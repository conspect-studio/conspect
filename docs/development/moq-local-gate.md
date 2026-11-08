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

The only following commit may add exactly `bin/check` and this document. Its
parent must be the last SHA above, both policy paths must be new, and the whole
working tree must be clean including untracked files. Thus no runtime wildcard,
unknown commit, modified source, or later policy revision is silently allowed.
Any further change requires a new reviewed gate and suite-budget decision.
Independent review binds the gate commit externally; no self-signed approval is
created. Amendments during initial independent review retain the same parent.

## Commands and limits

The executable `COMMANDS` list is authoritative: workspace fmt/check/strict
all-target Clippy, dependency audit, optional MoQ control feature check/Clippy,
MoQ/Cloudflare/vault/controller targeted tests, standalone fixture tests, root
bundle comparison, web lint/build, and probe syntax. Every command actually runs
and any failure stops the gate. The final HEAD and clean-tree checks run again
afterward. No full workspace test command runs a third time.

`bin/check --self-test` checks real evidence plus six safe in-memory corruptions;
`--validate-only` checks historical evidence/ancestry/cleanliness without builds.
Neither is a receipt-generating successful full gate: invoke the normal wrapper
without these flags only after independent review. The branch still lacks the
production shared-feed factory and paid/AV-sync acceptance. Green local checks
do not establish CI green, merge, or whole-feature delivery.
