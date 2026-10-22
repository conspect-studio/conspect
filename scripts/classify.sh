#!/usr/bin/env bash
# Classify a change by blast radius: R0 / R1 / R2 / R3.
#
# Why: gates should scale with blast radius, not with change count. This script is the
# mechanical half of docs/standards/engineering.md Part B — it reads the diff, applies a
# path map plus an R0 content test, and prints the class and the gates that class owes.
#
# The class is a FLOOR. Raise it when you know something the paths cannot show (above all,
# a change that risks invariant #1 or #10 is R3 whatever its path). You may never lower it.
#
# Design notes:
#   * PATH-KEYED ONLY. The commit type is non-authoritative: this repo has many
#     `docs(...)`-typed commits whose entire diff is doc comments inside crates/**/*.rs, and
#     every one of them correctly received the full CI matrix.
#   * The R0 prose test mirrors the allowlist in .github/workflows/ci.yml's `changes` job
#     (ADR-0046) so the classifier and CI can never disagree. Both fail UPWARD: a path that
#     matches nothing is code, not prose.
#   * `case` patterns are used deliberately — in a bash `case`, `*` matches `/` too, so
#     `crates/multiview-engine/*` covers the whole subtree.
#
# Usage:
#   scripts/classify.sh [BASE_REF]   # default: origin/main, else main, else HEAD~1
#   scripts/classify.sh --quiet      # print just the class (R0|R1|R2|R3)
#   git diff --name-only A B | scripts/classify.sh --files   # classify an explicit path list
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"

quiet=0
from_stdin=0
base=""
for arg in "$@"; do
  case "$arg" in
    --quiet | -q) quiet=1 ;;
    --files) from_stdin=1 ;;
    -h | --help)
      sed -n '2,23p' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *) base="$arg" ;;
  esac
done

# ── collect the changed paths ────────────────────────────────────────────────
files_tmp="$(mktemp)"
trap 'rm -f "$files_tmp"' EXIT

if [ "$from_stdin" -eq 1 ]; then
  base="(stdin)"
  sort -u | grep -v '^$' >"$files_tmp" || true
else
  if [ -z "$base" ]; then
    if git rev-parse --verify -q origin/main >/dev/null 2>&1; then
      base="origin/main"
    elif git rev-parse --verify -q main >/dev/null 2>&1; then
      base="main"
    else
      base="HEAD~1"
    fi
  fi

  merge_base="$(git merge-base "$base" HEAD 2>/dev/null || echo "")"

  # Changes since the merge base, plus anything staged, unstaged or untracked, so the
  # script is useful mid-work and not only at PR time.
  {
    [ -n "$merge_base" ] && git diff --name-only "$merge_base" HEAD
    git diff --name-only HEAD
    git diff --name-only --cached
    git ls-files --others --exclude-standard
  } 2>/dev/null | sort -u | grep -v '^$' >"$files_tmp" || true
fi

# ── per-file classification ──────────────────────────────────────────────────
# Highest class wins. Order matters: R3, then R2, then the R0 prose test, else R1.
classify_one() {
  case "$1" in
    # ── Agent policy, wherever it lives (checked first) ──────────────────────
    # A per-directory CLAUDE.md is agent instruction, not the code it annotates,
    # so it is R2 even inside an R3 crate. Without this arm, editing
    # crates/multiview-licence/CLAUDE.md would inherit that crate's R3 and put
    # operator approval on a docs tweak — disproportionate, and exactly the
    # ceremony this matrix exists to remove.
    AGENTS.md | CLAUDE.md | */CLAUDE.md | docs/standards/* | .claude/* | scripts/classify.sh)
      echo R2
      return
      ;;

    # ── R3: keys, licence enforcement, legal/public action, publishing, and
    # the trust boundaries — authn/authz, credential and token handling,
    # outbound-egress guards, TLS. A defect on those is a security incident,
    # not a bug: PR #246 (outbound-dial SSRF, ADR-M013) took four cross-vendor
    # passes, and R2's single pass would have under-gated it.
    LICENSE | LICENSE-* | NOTICE | THIRD-PARTY-NOTICES.md | \
      CONTRIBUTOR-LICENSE-AGREEMENT.md | CODE_OF_CONDUCT.md | SECURITY.md | \
      docs/licensing/* | .gitleaks.toml | \
      .github/workflows/release.yml | .github/workflows/release-plz.yml | \
      .github/workflows/docker.yml | .github/workflows/ffmpeg-base.yml | \
      release-plz.toml | cliff.toml | \
      crates/multiview-licence/* | \
      crates/multiview-control/src/auth.rs | \
      crates/multiview-control/src/devices/* | \
      crates/multiview-control/src/routes/cast_sessions.rs | \
      crates/multiview-config/src/device/net_guard.rs | \
      crates/multiview-config/src/tls.rs | \
      crates/multiview-preview/src/token.rs | \
      crates/multiview-webrtc/src/turn/auth.rs)
      echo R3
      return
      ;;

    # ── R2: data plane, security surface, deps, CI/CD, agent policy, publish ─
    # Data plane — conventions.md invariants #1-#10 live in these crates.
    crates/multiview-core/* | crates/multiview-hal/* | crates/multiview-ffmpeg/* | \
      crates/multiview-compositor/* | crates/multiview-framestore/* | \
      crates/multiview-audio/* | crates/multiview-overlay/* | crates/multiview-input/* | \
      crates/multiview-output/* | crates/multiview-engine/* | \
      crates/multiview-ndi-sys/* | crates/multiview-rist-sys/* | \
      crates/multiview-ntpsys/* | crates/multiview-i915pmu/* | \
      crates/multiview-webrtc/* | \
      crates/multiview-control/* | crates/multiview-config/* | \
      crates/multiview-preview/* | crates/multiview-mesh/* | crates/multiview-cli/* | \
      xtask/* | \
      Cargo.toml | Cargo.lock | */Cargo.toml | web/package.json | web/package-lock.json | \
      deny.toml | renovate.json | .github/dependabot.yml | \
      rust-toolchain.toml | clippy.toml | rustfmt.toml | lefthook.yml | \
      .github/workflows/* | .github/CODEOWNERS | \
      .github/PULL_REQUEST_TEMPLATE.md | .github/ISSUE_TEMPLATE/* | \
      deploy/* | scripts/* | \
      .mcp.json | .devcontainer/* | docs/api/*)
      echo R2
      return
      ;;
  esac

  # ── R0: prose with no executable or spec effect ───────────────────────────
  # Mirrors ci.yml's `changes` prose allowlist (ADR-0046): Markdown, plain text and
  # images are prose. Everything the allowlist deliberately excludes has already been
  # caught above and returned -- docs/api/** (the served contract, include_str!'d into
  # multiview-control), LICENSE*/NOTICE (R3), and every *.md under an R2/R3 path such
  # as .claude/**, crates/**, AGENTS.md or docs/standards/**. So a bare extension test
  # is exact here, not loose. Note `*` matches `/` inside a `case`, so `*.md` already
  # covers every depth -- per-directory variants would be dead patterns.
  case "$1" in
    *.md | *.txt | *.png | *.jpg | *.jpeg | *.gif | *.svg | *.webp)
      echo R0
      return
      ;;
  esac

  echo R1
}

rank() { case "$1" in R0) echo 0 ;; R1) echo 1 ;; R2) echo 2 ;; R3) echo 3 ;; esac; }

class="R0"
count=0
while IFS= read -r f; do
  [ -n "$f" ] || continue
  count=$((count + 1))
  c="$(classify_one "$f")"
  if [ "$(rank "$c")" -gt "$(rank "$class")" ]; then class="$c"; fi
done <"$files_tmp"

if [ "$count" -eq 0 ]; then class="R0"; fi

if [ "$quiet" -eq 1 ]; then
  echo "$class"
  exit 0
fi

echo "class: $class   (base $base, $count changed path(s))"
echo
case "$class" in
  R0)
    cat <<'EOF'
R0 — prose only, no executable or spec effect.
  tests      none        full suite  CI only
  review     none        docs/ADR    n/a
  merge      auto on green           batching  many per PR
EOF
    ;;
  R1)
    cat <<'EOF'
R1 — contained change outside the data plane and the security surface.
  tests      focused crate: cargo test -p multiview-<crate>
  full suite CI only
  review     diff-only, single cross-vendor pass
  test-first behavioural changes only        docs  only if the change makes docs wrong
  merge      agent merges on green           batching  related changes per PR
EOF
    ;;
  R2)
    cat <<'EOF'
R2 — data plane, security surface, deps, CI/CD, agent policy, or a publish trigger.
  issue      reference the request or a backlog item
  test-first required                        tests  focused crate while working
  full suite before the PR, and CI:
             cargo fmt --all -- --check
             cargo clippy --locked --workspace --all-targets -- -D warnings
             cargo test --locked --workspace
             cargo deny check                (if dependencies changed)
  review     scoped-context cross-vendor pass (never self-performed by the author's vendor)
  docs       required for a behaviour change  ADR  only if it constrains future changes
  merge      agent merges on green; operator override retained (ADR-G005)
  batching   one logical change per PR
EOF
    ;;
  R3)
    cat <<'EOF'
R3 — keys, licence enforcement, legal or public action, release publishing,
     or any change that risks invariant #1 (output-clock) or #10 (isolation).
  everything R2 owes, plus:
  plan       required                        ADR  required
  review     3-lens cross-vendor panel
  gates      chaos/soak for an invariant #1/#10 change; rehearsed recovery path
  approval   explicit operator approval before the outward-facing action
  merge      operator confirms, then merge
EOF
    ;;
esac
echo
echo "Contract: docs/standards/engineering.md   (this class is a floor, never a ceiling)"
