#!/usr/bin/env sh
# .devcontainer/initialize.sh
#
# Runs on the HOST (not in the container) before container creation, and again
# on every rebuild/reopen/restart. MUST be idempotent and MUST NOT print secrets.
#
# Responsibilities:
#   1. GUARANTEE a repo-root .env exists. devcontainer.json uses
#      `runArgs: ["--env-file", ".env"]`, and `docker run --env-file <missing>`
#      FAILS with "no such file or directory" (verified on current Docker). So we
#      always create .env (empty if there is no token), making container start safe.
#   2. SEED OP_SERVICE_ACCOUNT_TOKEN into .env from ~/.onepassword_token when the
#      token is not already present in .env and the host file exists. The token is
#      written via redirection (never echoed), with a trailing newline stripped,
#      and the file is chmod 600.
#   3. Emit a friendly note for VAAPI: if /dev/dri exists on this host, print the
#      render-group GID and how to enable VAAPI passthrough via a local override
#      (we do NOT auto-add --device /dev/dri to the committed config, because an
#      unconditional device mount breaks startup on macOS and NVIDIA-only Linux).
#
# POSIX sh only — no bashisms.

set -eu

# Resolve the repo root from this script's location (.devcontainer/..).
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
ENV_FILE="$REPO_ROOT/.env"
TOKEN_FILE="${HOME}/.onepassword_token"

# --- 1 & 2: ensure .env exists and seed the token if needed ------------------
umask 077  # any file we create is owner-only

if [ ! -f "$ENV_FILE" ]; then
  : > "$ENV_FILE"
  echo "[initialize] created empty .env at repo root"
fi

# Does .env already define OP_SERVICE_ACCOUNT_TOKEN (any non-empty value)?
has_token=0
if grep -Eq '^OP_SERVICE_ACCOUNT_TOKEN=.+' "$ENV_FILE" 2>/dev/null; then
  has_token=1
fi

if [ "$has_token" -eq 0 ]; then
  if [ -r "$TOKEN_FILE" ]; then
    # Strip CR/LF so the token is a clean single line.
    TOKEN=$(tr -d '\r\n' < "$TOKEN_FILE")
    if [ -n "$TOKEN" ]; then
      # Remove any stale empty/blank assignment, then append the real one.
      # (Use a temp file; never echo the token to stdout.)
      tmp=$(mktemp "${ENV_FILE}.XXXXXX")
      grep -Ev '^OP_SERVICE_ACCOUNT_TOKEN=' "$ENV_FILE" > "$tmp" 2>/dev/null || true
      printf 'OP_SERVICE_ACCOUNT_TOKEN=%s\n' "$TOKEN" >> "$tmp"
      mv "$tmp" "$ENV_FILE"
      echo "[initialize] seeded OP_SERVICE_ACCOUNT_TOKEN from ~/.onepassword_token"
    else
      echo "[initialize] WARN: ~/.onepassword_token is empty; op secret access disabled"
    fi
    unset TOKEN
  else
    echo "[initialize] NOTE: ~/.onepassword_token not found; .env has no token (op disabled, build/test still work)"
  fi
fi

chmod 600 "$ENV_FILE"

# --- 3: VAAPI guidance (informational only; never fatal) ---------------------
if [ -e /dev/dri/renderD128 ]; then
  # `stat -c` is GNU/Linux; guard for macOS (BSD stat) where /dev/dri won't exist anyway.
  if RGID=$(stat -c '%g' /dev/dri/renderD128 2>/dev/null) && [ -n "$RGID" ]; then
    echo "[initialize] VAAPI host render node detected (/dev/dri/renderD128, gid=$RGID)."
    echo "[initialize] To enable VAAPI passthrough, add a local override (NOT committed):"
    echo "[initialize]   create .devcontainer/devcontainer.local.json with runArgs:"
    echo "[initialize]     [\"--device\",\"/dev/dri\",\"--group-add\",\"$RGID\"]"
    echo "[initialize]   then 'Dev Containers: Rebuild Container'. See docs/operations/devcontainer.md."
  fi
else
  echo "[initialize] No /dev/dri on host (macOS or NVIDIA-only/GPU-less). VAAPI is unavailable; software paths only."
fi

exit 0
