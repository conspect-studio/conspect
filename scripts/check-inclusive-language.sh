#!/usr/bin/env bash
# Starter inclusive-language check (see CODE_OF_CONDUCT.md and
# docs/architecture/conventions.md §9). It flags only terms that have clean,
# unambiguous inclusive replacements in this codebase and NO legitimate
# domain collision here — currently the allowlist/blocklist pair.
#
# It deliberately does NOT flag terms that carry precise technical meaning in
# this project, e.g. FFmpeg "slave outputs", a "master clock", or HDR
# "mastering display". For richer, configurable, context-aware enforcement,
# adopt `woke` (https://github.com/get-woke/woke) with a project rules file.
set -euo pipefail

pattern='\b(whitelist|blacklist)\b'

matches="$(grep -rInE -i "$pattern" . \
  --exclude-dir=.git \
  --exclude-dir=target \
  --exclude-dir=node_modules \
  --exclude-dir=dist \
  --exclude-dir=.multiview-build \
  --exclude-dir=.claude \
  --exclude='check-inclusive-language.sh' || true)"

if [ -n "$matches" ]; then
  echo "::error::Non-inclusive terminology found. Prefer 'allowlist'/'blocklist'."
  echo "$matches"
  exit 1
fi

echo "inclusive-language: OK"
