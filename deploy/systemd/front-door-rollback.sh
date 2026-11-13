#!/usr/bin/env bash
# deploy/systemd/front-door-rollback.sh: undo the front-door cutover
# (docs/runbooks/front-door-cutover.md, issue #292) in one command:
#
#   sudo bash deploy/systemd/front-door-rollback.sh
#
# 1. Builds the rolled-back state in a scratch copy and runs `multiview
#    validate` on it FIRST; nothing is stopped unless it prints
#    "OK   start: accepted state" (RULES: validate before every restart).
#    Rolled back = conspect.toml from conspect.toml.pre-cutover, and in
#    desired.json ONLY the `network` and `control` sections restored from
#    desired.json.pre-cutover (every other operator edit made since is kept).
# 2. Stops the engine and conspect.socket, removes the socket-activation
#    drop-in, installs the rolled-back files, restores certbot's renewal
#    config (nginx authenticator), starts nginx, starts the engine (which
#    then binds only UDP 443 itself, as before the cutover).
#
# The engine binary is not changed (the new binary runs the pre-cutover state
# exactly as before). Environment overrides: MV (config dir), BIN (binary),
# SVC_USER (service user).
set -euo pipefail

MV="${MV:-/home/operations/mv}"
BIN="${BIN:-/home/operations/conspect/target/release/multiview}"
SVC_USER="${SVC_USER:-operations}"
STATE="$MV/.multiview/desired.json"

[ "$(id -u)" -eq 0 ] || { echo "run with sudo" >&2; exit 1; }
[[ "$SVC_USER" =~ ^[a-z_][a-z0-9_-]*$ ]] || { echo "SVC_USER is not a plain user name" >&2; exit 1; }
for p in "$MV" "$BIN"; do
    [[ "$p" =~ ^/[A-Za-z0-9._/@+-]*$ && "$p" != *..* ]] || { echo "unsafe path: $p" >&2; exit 1; }
done
for f in "$MV/conspect.toml.pre-cutover" "$STATE.pre-cutover" "$STATE" "$BIN"; do
    [ -e "$f" ] || { echo "missing $f (preflight step 1 of the runbook makes the copies)" >&2; exit 1; }
done

scratch="$(sudo -u "$SVC_USER" mktemp -d "$MV/.rollback.XXXXXX")"
trap 'rm -rf -- "${scratch:?}"' EXIT
sudo -u "$SVC_USER" mkdir -m 0700 "$scratch/.multiview"
sudo -u "$SVC_USER" cp "$MV/conspect.toml.pre-cutover" "$scratch/conspect.toml"
sudo -u "$SVC_USER" python3 - "$STATE" "$STATE.pre-cutover" "$scratch/.multiview/desired.json" <<'PY'
import json, os, sys
current, before, out = sys.argv[1:4]
with open(current) as f:
    state = json.load(f)
with open(before) as f:
    old = json.load(f)
for section in ("network", "control"):
    if section in old["config"]:
        state["config"][section] = old["config"][section]
    else:
        state["config"].pop(section, None)
fd = os.open(out, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
with os.fdopen(fd, "w") as f:
    json.dump(state, f, indent=2)
PY

echo "== validate the rolled-back state (nothing stopped yet)"
report="$(sudo -u "$SVC_USER" "$BIN" validate "$scratch/conspect.toml" 2>&1)" || true
printf '%s\n' "$report"
# The OK line must be for the accepted state the engine will start from:
# exactly the scratch desired.json built above (the line after it names it).
want="     $scratch/.multiview/desired.json"
if ! printf '%s\n' "$report" | grep -A1 '^OK   start: accepted state' | grep -qxF -- "$want" \
   || printf '%s\n' "$report" | grep -q '^FAIL'; then
    echo "ROLLBACK ABORTED: the rolled-back state does not validate; nothing was changed" >&2
    exit 1
fi

echo "== stop the engine and the socket unit"
systemctl stop conspect.service
systemctl stop conspect.socket || true
systemctl disable conspect.socket || true
rm -f /etc/systemd/system/conspect.service.d/10-socket-activation.conf
rm -f /etc/systemd/system/conspect.service.d/20-nft-floor.conf
systemctl daemon-reload

echo "== install the rolled-back files"
sudo -u "$SVC_USER" cp "$STATE" "$STATE.rolled-back-from"
sudo -u "$SVC_USER" install -m 0600 "$scratch/.multiview/desired.json" "$STATE"
sudo -u "$SVC_USER" cp "$scratch/conspect.toml" "$MV/conspect.toml"

echo "== certbot back to its pre-cutover authenticator (nginx)"
for conf in /etc/letsencrypt/renewal/*.conf.pre-cutover; do
    [ -e "$conf" ] || continue
    cp -p "$conf" "${conf%.pre-cutover}"
done

echo "== nginx back on TCP 443/80, then the engine"
systemctl enable --now nginx
systemctl start conspect.service
sleep 3
systemctl --no-pager --lines=0 status nginx conspect.service | grep -E 'Active:|●'
echo "rolled back; the cutover state is kept in $STATE.rolled-back-from"
