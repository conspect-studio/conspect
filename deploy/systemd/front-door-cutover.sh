#!/usr/bin/env bash
# deploy/systemd/front-door-cutover.sh: move TCP 443/80 from nginx to the
# engine on a host like mda001 (docs/runbooks/front-door-cutover.md, #292).
#
#   sudo HOST=mda001.public-servers.sy4.aperim.net bash deploy/systemd/front-door-cutover.sh
#
# Order (every step before "stop" is read-only for the running service):
#  1. Copies the certbot certificate where the engine (user operations) can
#     read it, and installs a certbot deploy hook that repeats the copy on
#     every renewal (the engine reloads the files without a restart).
#  2. Builds the new state in a scratch directory from the CURRENT
#     desired.json (only `network` keys and `control.ui_dir` change; every
#     operator edit is kept) and conspect.toml (the same [network] and
#     ui_dir, so a later edit of the file keeps them), and runs `multiview
#     validate` on it. Nothing is stopped unless it prints
#     "OK   start: accepted state".
#  3. Keeps rollback copies (*.pre-cutover), installs conspect.socket and the
#     service drop-in, stops the engine, stops+disables nginx, installs the
#     new state, starts the socket and the engine.
#  4. Moves certbot from the nginx authenticator to the engine's port-80
#     webroot (HTTP-01) and proves it with `certbot renew --dry-run`.
# Rollback: sudo bash deploy/systemd/front-door-rollback.sh
#
# Inputs (environment): HOST (required, the public name = certificate name),
# UI_DIR (default: the conspect-ui root nginx serves), ADVERTISE (optional,
# space-separated IP literals; default: keep the current network.advertise),
# MV, BIN, SVC_USER, UNITS (the deploy/systemd directory of the checkout).
set -euo pipefail

HOST="${HOST:?set HOST to the public host name (the certbot certificate name)}"
MV="${MV:-/home/operations/mv}"
BIN="${BIN:-/home/operations/conspect/target/release/multiview}"
SVC_USER="${SVC_USER:-operations}"
UNITS="${UNITS:-$(cd "$(dirname "$0")" && pwd)}"
STATE="$MV/.multiview/desired.json"
TLS_DIR=/etc/conspect/tls
WEBROOT=/var/lib/conspect/acme-webroot
LE="/etc/letsencrypt/live/$HOST"
HOOK=/etc/letsencrypt/renewal-hooks/deploy/conspect-copy.sh

[ "$(id -u)" -eq 0 ] || { echo "run with sudo" >&2; exit 1; }
for f in "$BIN" "$STATE" "$MV/conspect.toml" "$LE/fullchain.pem" "$LE/privkey.pem" \
         "$UNITS/conspect.socket" "$UNITS/conspect.service.d/10-socket-activation.conf"; do
    [ -e "$f" ] || { echo "missing $f" >&2; exit 1; }
done
if [ -e "$STATE.pre-cutover" ] && [ "${FORCE:-0}" != 1 ]; then
    echo "$STATE.pre-cutover exists: a cutover already ran (roll back first, or FORCE=1)" >&2
    exit 1
fi
if [ -z "${UI_DIR:-}" ]; then
    UI_DIR="$(nginx -T 2>/dev/null | sed -n 's/^[[:space:]]*root[[:space:]]\+\(\/var\/www\/conspect-ui\/[^;[:space:]]*\);.*/\1/p' | head -n 1)"
fi
[ -n "${UI_DIR:-}" ] && [ -f "$UI_DIR/index.html" ] || { echo "UI_DIR not found (set UI_DIR=/var/www/conspect-ui/<sha>)" >&2; exit 1; }
UI_DIR="${UI_DIR%/}"
echo "host $HOST, UI $UI_DIR"

echo "== 1. certificate readable by $SVC_USER, renewed copies follow"
install -d -m 0750 -o root -g "$SVC_USER" "$TLS_DIR"
cat > "$HOOK" <<EOF
#!/bin/sh
# Installed by deploy/systemd/front-door-cutover.sh: copy the renewed
# certificate for the conspect engine (it reloads the files within 30 s).
set -e
[ "\${RENEWED_LINEAGE:-$LE}" = "$LE" ] || exit 0
install -m 0644 -o root -g $SVC_USER "$LE/fullchain.pem" "$TLS_DIR/fullchain.pem.new"
install -m 0640 -o root -g $SVC_USER "$LE/privkey.pem" "$TLS_DIR/privkey.pem.new"
mv -f "$TLS_DIR/privkey.pem.new" "$TLS_DIR/privkey.pem"
mv -f "$TLS_DIR/fullchain.pem.new" "$TLS_DIR/fullchain.pem"
EOF
chmod 0755 "$HOOK"
RENEWED_LINEAGE="$LE" "$HOOK"
install -d -m 0755 "$WEBROOT/.well-known/acme-challenge"
sudo -u "$SVC_USER" test -r "$TLS_DIR/privkey.pem"

echo "== 2. build and validate the new state (nothing stopped yet)"
scratch="$(sudo -u "$SVC_USER" mktemp -d "$MV/.cutover.XXXXXX")"
trap 'rm -rf -- "${scratch:?}"' EXIT
sudo -u "$SVC_USER" mkdir -m 0700 "$scratch/.multiview"
sudo -u "$SVC_USER" env HOST="$HOST" TLS_DIR="$TLS_DIR" WEBROOT="$WEBROOT" UI_DIR="$UI_DIR" \
    ADVERTISE="${ADVERTISE:-}" python3 - "$STATE" "$MV/conspect.toml" "$scratch" <<'PY'
import json, os, re, sys
state_path, toml_path, scratch = sys.argv[1:4]
env = os.environ
with open(state_path) as f:
    state = json.load(f)
cfg = state["config"]
net = cfg.setdefault("network", {})
net.update({
    "https_port": 443,
    "mode": "direct",
    "public_host": env["HOST"],
    "cert_file": f'{env["TLS_DIR"]}/fullchain.pem',
    "key_file": f'{env["TLS_DIR"]}/privkey.pem',
    "acme_webroot": env["WEBROOT"],
})
for stale in ("tcp_port", "http_redirect_port", "bind", "proxy_listen", "trusted_proxies"):
    net.pop(stale, None)
if env.get("ADVERTISE"):
    net["advertise"] = env["ADVERTISE"].split()
net.setdefault("ice_tcp", {"enabled": True})
cfg.setdefault("control", {})["ui_dir"] = env["UI_DIR"]
fd = os.open(f"{scratch}/.multiview/desired.json", os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
with os.fdopen(fd, "w") as f:
    json.dump(state, f, indent=2)
# conspect.toml: [control] is a restart-only section spliced from the boot
# file, so ui_dir must be there too. Replace or add the key in [control].
with open(toml_path) as f:
    text = f.read()
line = f'ui_dir = "{env["UI_DIR"]}"'
m = re.search(r"(?m)^\[control\][ \t]*$", text)
if m is None:
    raise SystemExit("conspect.toml has no [control] section")
end = re.search(r"(?m)^\[", text[m.end():])
body_end = m.end() + end.start() if end else len(text)
body = text[m.end():body_end]
if re.search(r"(?m)^ui_dir[ \t]*=", body):
    body = re.sub(r"(?m)^ui_dir[ \t]*=.*$", line, body)
else:
    body = "\n" + line + body
text = text[:m.end()] + body + text[body_end:]
# [network] is NOT spliced from the boot file (desired.json carries it), but
# the config-file watcher commits the WHOLE file on any later edit of
# conspect.toml: the file must carry the same [network] or that edit would
# drop the cutover keys. Replace every [network] / [network.*] table.
if "tls" in net:
    raise SystemExit("[network.tls] is set (ACME): this script is for the certbot cutover")
lines, out, skipping = text.splitlines(keepends=True), [], False
for ln in lines:
    header = re.match(r"^\[([^\[\]]+)\]\s*$", ln)
    if header:
        skipping = header.group(1).strip() == "network" or header.group(1).strip().startswith("network.")
    if not skipping:
        out.append(ln)
def value(v):
    if isinstance(v, bool):
        return "true" if v else "false"
    if isinstance(v, int):
        return str(v)
    if isinstance(v, str):
        return json.dumps(v)
    if isinstance(v, list) and all(isinstance(x, str) for x in v):
        return "[" + ", ".join(json.dumps(x) for x in v) + "]"
    raise SystemExit(f"cannot write network value {v!r} to TOML")
block = ["\n[network]\n"]
tables = []
for k, v in net.items():
    if isinstance(v, dict):
        tables.append((k, v))
    else:
        block.append(f"{k} = {value(v)}\n")
for k, v in tables:
    block.append(f"\n[network.{k}]\n")
    block.extend(f"{kk} = {value(vv)}\n" for kk, vv in v.items())
text = "".join(out).rstrip("\n") + "\n" + "".join(block)
with open(f"{scratch}/conspect.toml", "w") as f:
    f.write(text)
PY
report="$(sudo -u "$SVC_USER" "$BIN" validate "$scratch/conspect.toml" 2>&1)" || true
printf '%s\n' "$report"
if ! printf '%s\n' "$report" | grep -q '^OK   start: accepted state'; then
    echo "CUTOVER ABORTED: the new state does not validate; nothing was changed" >&2
    exit 1
fi
python3 -c 'import json,sys; n=json.load(open(sys.argv[1]))["config"]["network"]; print("network:", json.dumps(n))' \
    "$scratch/.multiview/desired.json"

echo "== 3. cut over (rollback: sudo bash $UNITS/front-door-rollback.sh)"
sudo -u "$SVC_USER" cp -p "$STATE" "$STATE.pre-cutover"
sudo -u "$SVC_USER" cp -p "$MV/conspect.toml" "$MV/conspect.toml.pre-cutover"
nginx -T > "$MV/nginx-T.pre-cutover.txt" 2>&1 || true
install -m 0644 "$UNITS/conspect.socket" /etc/systemd/system/conspect.socket
install -D -m 0644 "$UNITS/conspect.service.d/10-socket-activation.conf" \
    /etc/systemd/system/conspect.service.d/10-socket-activation.conf
systemctl daemon-reload
t0=$(date +%s.%N)
systemctl stop conspect.service
systemctl disable --now nginx
sudo -u "$SVC_USER" install -m 0600 "$scratch/.multiview/desired.json" "$STATE"
sudo -u "$SVC_USER" cp "$scratch/conspect.toml" "$MV/conspect.toml"
systemctl enable --now conspect.socket
systemctl start conspect.service
for _ in $(seq 1 60); do
    curl -fsS -o /dev/null --max-time 2 --resolve "$HOST:443:[::1]" "https://$HOST/" && break
    sleep 0.5
done
t1=$(date +%s.%N)
echo "site answered over the engine's TLS after $(python3 -c "print(round($t1-$t0,1))") s"

echo "== 4. certbot renews through the engine (HTTP-01 webroot on port 80)"
conf="/etc/letsencrypt/renewal/$HOST.conf"
cp -p "$conf" "$conf.pre-cutover"
python3 - "$conf" "$HOST" "$WEBROOT" <<'PY'
import re, sys
path, host, webroot = sys.argv[1:4]
text = open(path).read()
text = re.sub(r"(?m)^authenticator = .*$", "authenticator = webroot", text)
text = re.sub(r"(?m)^installer = .*\n", "", text)
text = re.sub(r"(?m)^webroot_path = .*\n", "", text)
text = re.sub(r"(?ms)^\[\[webroot_map\]\].*?(?=^\[|\Z)", "", text)
text = text.replace("[renewalparams]\n", f"[renewalparams]\nwebroot_path = {webroot},\n", 1)
text = text.rstrip("\n") + f"\n[[webroot_map]]\n{host} = {webroot}\n"
open(path, "w").write(text)
PY
if certbot renew --cert-name "$HOST" --dry-run; then
    echo "certbot dry-run renewal through the engine: OK"
else
    echo "WARNING: certbot dry-run failed; the certificate still serves (see the runbook, 'Certificates')" >&2
fi
echo "cutover done; run the checks in docs/runbooks/front-door-cutover.md"
