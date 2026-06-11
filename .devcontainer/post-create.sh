#!/usr/bin/env bash
# .devcontainer/post-create.sh
#
# Runs IN the container, once, after creation. Fixes cache ownership, verifies the
# toolchain, warms the cargo cache, and prints a capability summary. Resilient:
# individual probes never abort the whole script (so the container is usable even
# if e.g. cargo fetch is offline).

set -eu

echo "==> Fixing ownership of cached volumes (empty named volumes mount as root)"
sudo chown -R vscode:vscode /usr/local/cargo/registry /usr/local/cargo/git 2>/dev/null || true
if [ -d "${PWD}/target" ]; then
  sudo chown -R vscode:vscode "${PWD}/target" 2>/dev/null || true
fi

echo "==> Toolchain versions"
rustc --version || true
cargo --version || true
echo -n "ffmpeg: "; ffmpeg -hide_banner -version 2>/dev/null | head -1 || echo "not found"
echo -n "libavcodec (pkg-config): "; pkg-config --modversion libavcodec 2>/dev/null || echo "not found"
echo -n "node: "; node --version 2>/dev/null || echo "not found"
echo -n "op: "; op --version 2>/dev/null || echo "not found"

echo "==> Warming cargo cache (cargo fetch)"
cargo fetch --locked 2>/dev/null || cargo fetch 2>/dev/null || echo "   (cargo fetch skipped/failed — non-fatal)"

# ---------------------------------------------------------------------------
# Agent CLIs + dev tools. npm comes from the Node feature (installed AFTER the
# Dockerfile), so these are installed here, not in the image. Non-fatal: a
# registry hiccup must never break container creation. The npm packages are
# architecture-independent (JavaScript), so this works on amd64 and arm64.
# ---------------------------------------------------------------------------
echo "==> Installing agent CLIs (Claude Code, Codex)"
if command -v npm >/dev/null 2>&1; then
  npm install -g @anthropic-ai/claude-code @openai/codex 2>/dev/null \
    || echo "   (npm global install failed — non-fatal; rerun: npm i -g @anthropic-ai/claude-code @openai/codex)"
else
  echo "   (npm not found — Node feature missing? skipping agent CLIs)"
fi
echo -n "claude : "; claude --version 2>/dev/null || echo "not found"
echo -n "codex  : "; codex --version 2>/dev/null || echo "not found"
echo -n "rg     : "; rg --version 2>/dev/null | head -1 || echo "not found"
echo -n "fd     : "; fd --version 2>/dev/null || echo "not found"
echo -n "bat    : "; bat --version 2>/dev/null || echo "not found"

# ---------------------------------------------------------------------------
# Capability summary — what hardware/secret paths are actually available here.
# ---------------------------------------------------------------------------
echo ""
echo "================ Multiview dev container capability summary ================"

# NVIDIA NVDEC/NVENC
if command -v nvidia-smi >/dev/null 2>&1 && nvidia-smi -L >/dev/null 2>&1; then
  GPU_NAME=$(nvidia-smi --query-gpu=name --format=csv,noheader 2>/dev/null | head -1)
  echo "NVIDIA GPU : YES -> ${GPU_NAME:-detected}"
  if ffmpeg -hide_banner -encoders 2>/dev/null | grep -q nvenc; then
    echo "  NVENC    : ffmpeg exposes *_nvenc encoders"
  else
    echo "  NVENC    : ffmpeg has no nvenc encoders (check NVIDIA_DRIVER_CAPABILITIES includes 'video')"
  fi
else
  echo "NVIDIA GPU : no (CPU/software encode/decode only)"
fi

# Intel/AMD VAAPI
if [ -e /dev/dri/renderD128 ]; then
  if vainfo >/dev/null 2>&1; then
    echo "VAAPI      : YES -> /dev/dri/renderD128 usable (vainfo OK)"
  else
    echo "VAAPI      : /dev/dri present but vainfo failed (likely render-group GID mismatch;"
    echo "             add --group-add <host renderD128 gid> via a local override)"
  fi
else
  echo "VAAPI      : no /dev/dri (macOS Apple Silicon has NO passthrough; software paths only)"
fi

# 1Password
if [ -n "${OP_SERVICE_ACCOUNT_TOKEN:-}" ]; then
  if op whoami >/dev/null 2>&1; then
    echo "1Password  : token set and authenticated (op read \"op://Vault/Item/field\" works)"
  else
    echo "1Password  : token present but auth failed (check token validity / vault grants)"
  fi
else
  echo "1Password  : no OP_SERVICE_ACCOUNT_TOKEN (op disabled; build/test still work)"
fi

echo "========================================================================"
echo "macOS note: on Apple Silicon + Docker Desktop the host GPU is NOT visible"
echo "to this Linux container. Test Metal/VideoToolbox with a NATIVE macOS build."
echo "========================================================================"
