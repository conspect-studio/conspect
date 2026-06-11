#!/usr/bin/env bash
# Lightweight, dependency-free relative-link sanity check for committed Markdown.
#
# Why: docs-only PRs skip the heavy CI matrix (see .github/workflows/ci.yml), so
# they need a real green signal of their own. The most common docs-PR breakage is
# a relative link to a brief/ADR/doc that was renamed or moved. This script walks
# every tracked *.md file, extracts inline `[text](target)` links, and fails if a
# RELATIVE target points at a path that does not exist in the repo.
#
# Deliberately conservative — it only flags links it can prove are broken:
#   * external links (http://, https://, mailto:, tel:, //…)  → skipped
#   * in-page anchors (#section)                              → skipped
#   * a target with a #fragment is checked WITHOUT the fragment (we verify the
#     file exists; we do not validate heading anchors)
#   * reference-style links and bare autolinks                → not parsed
#   * `< >`-wrapped autolinks                                 → skipped
# When in doubt it does NOT flag — this is a sanity net, not an exhaustive linter.
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"

# Collect tracked markdown files (NUL-delimited; handles spaces). Restrict to
# tracked files so transient/build artefacts are never scanned.
mapfile -d '' md_files < <(git ls-files -z -- '*.md')

broken=0

for md in "${md_files[@]}"; do
  dir="$(dirname "$md")"

  # Extract inline-link targets: [text](target). Strip any title and #fragment.
  # grep -oE yields the whole "](target...)" match; sed peels the target out.
  while IFS= read -r target; do
    [ -n "$target" ] || continue

    # Skip external schemes, protocol-relative, pure anchors, and template
    # placeholders ([..](<name>.md), {{var}}, $VAR) that appear inside docs
    # examples and are never real paths.
    case "$target" in
      http://*|https://*|mailto:*|tel:*|//*|\#*) continue ;;
      *'<'*|*'{'*|*'$'*) continue ;;
    esac

    # Drop a #fragment and any ?query, then trim surrounding whitespace.
    path="${target%%#*}"
    path="${path%%\?*}"
    path="${path#"${path%%[![:space:]]*}"}"
    path="${path%"${path##*[![:space:]]}"}"
    [ -n "$path" ] || continue

    # Resolve the link relative to the containing file (absolute "/x" → repo root).
    case "$path" in
      /*) resolved=".${path}" ;;
      *)  resolved="${dir}/${path}" ;;
    esac

    if [ ! -e "$resolved" ]; then
      echo "::error file=${md}::broken relative link → ${target} (resolved: ${resolved#./})"
      broken=$((broken + 1))
    fi
  done < <(
    grep -oE '\]\([^)]+\)' "$md" 2>/dev/null \
      | sed -E 's/^\]\(//; s/\)$//' \
      | sed -E 's/[[:space:]]+"[^"]*"$//' \
      | sed -E "s/[[:space:]]+'[^']*'\$//" \
      || true
  )
done

if [ "$broken" -gt 0 ]; then
  echo "docs link sanity: ${broken} broken relative link(s) found." >&2
  exit 1
fi

echo "docs link sanity: OK"
