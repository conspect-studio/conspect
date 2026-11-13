# Runbook: front-door cutover, the engine takes TCP 443 and 80 from nginx (mda001)

Front-door slice 6 ([ADR-0107](../decisions/ADR-0107-single-front-door-443.md), design note
[section 9 and "Slice 6 as built"](../architecture/network-front-door.md#slice-6-as-built-issue-292),
issue #292). After this runbook the engine serves the UI, the API, HLS, the port-80 redirect,
WebRTC over ICE-TCP and (already) MoQ/WebRTC on UDP 443. nginx is stopped and disabled.
systemd owns the three ports (`conspect.socket`) and hands them to the engine, so a restart
of the engine never refuses a connection.

## What changes, and what does not

| | Before (nginx) | After (engine) |
| --- | --- | --- |
| TCP 443 | nginx: TLS, UI files from `/var/www/conspect-ui/<sha>/`, proxy to `[::1]:8080` | engine TCP door: TLS (same certbot certificate), UI from the same directory (`[control] ui_dir`), API, HLS, ICE-TCP |
| TCP 80 | nginx: 301 to https, certbot HTTP-01 through nginx | engine: 301 to `https://<public_host><path>`, certbot HTTP-01 through `acme_webroot` |
| UDP 443 | engine (MoQ + WebRTC) | engine, the socket now from systemd (a `ReusePort=yes` pair) |
| `[::1]:8080` | engine admin listener (plain HTTP, loopback) | unchanged: the CLI, the lead's curl and the token checks keep working |
| Certificate | certbot, `/etc/letsencrypt/live/<host>/`, renewed by `certbot.timer` via nginx | the same certbot lineage, renewed by `certbot.timer` through the engine's port 80; a deploy hook copies it to `/etc/conspect/tls/` and the engine reloads it within 30 s, no restart |
| Firewall | TCP 80/443 + UDP 443 open | **nothing to change**: TCP 80/443 and UDP 443 are already open (TCP 8443 is not, and is not used) |

Caching and headers are the ones nginx sent: hashed `/assets/*` `public, max-age=31536000,
immutable`, `index.html` `no-cache`, strong `ETag` + `304` on both; HLS per
`deploy/hls-nginx.conf` (playlists `max-age=1, must-revalidate`, segments immutable,
reflected CORS with `Vary: Origin`, `204` preflight, `Range`/`206`/`416`, `ETag`,
`Last-Modified`, `If-None-Match`/`If-Modified-Since` -> `304`, `If-Range`). Text assets are
gzipped when the browser accepts it (nginx did this for `text/html` only). The tests that pin
this are listed under "Evidence" below.

## Requirements

* The engine binary from `main` at or after the merge of #292 with the `tls` and `web`
  features (the presets include both). Build it as usual (RULES: `PATH=$HOME/.cargo/bin:$PATH`,
  `web/dist` copied into the source tree first).
* `python3`, `curl`, `certbot` on the host (all present on mda001).
* The checkout on the host that contains `deploy/systemd/` of the same commit (the scripts and
  units are used from there): below it is `~/conspect`.

## 0. Preflight (read-only)

```bash
cd ~/mv
NEW=/home/operations/conspect/target/release/multiview      # the binary you will run
$NEW --version
# The usual state check, with the binary you will run (RULES):
$NEW validate conspect.toml            # require exit 0 and "OK   start: accepted state"
# The certificate and the UI build nginx serves today:
sudo nginx -T 2>/dev/null | grep -E '^\s*(root|ssl_certificate|listen)\b' | sort -u
sudo certbot certificates
ls /var/www/conspect-ui/
# Nothing else may want 443/80 after nginx is gone:
sudo ss -ltnup '( sport = :443 or sport = :80 )'
```

Deploy the new engine binary first (the normal procedure, its own restart, the same validate
preflight). It changes nothing on TCP: `[network] mode` is not set yet, nginx keeps 443/80.

## 1. Cut over (one command)

```bash
cd ~/conspect
sudo HOST=mda001.public-servers.sy4.aperim.net bash deploy/systemd/front-door-cutover.sh
# optional: UI_DIR=/var/www/conspect-ui/<sha>   (default: the root nginx serves)
# optional: ADVERTISE="2404:... 203.0.113.10"  (default: keep the current network.advertise)
```

What it does, in order (the script header has the detail):

1. Copies the certbot pair to `/etc/conspect/tls/` (`root:operations`, key `0640`) and installs
   `/etc/letsencrypt/renewal-hooks/deploy/conspect-copy.sh` (static text; its paths and group
   are in `/etc/conspect/certbot-hook.env`), which repeats the copy on every renewal. `HOST`,
   `UI_DIR` and the service user are allow-listed before anything is written.
2. Builds the new state **in a scratch copy** from the current `~/mv/.multiview/desired.json`
   (all operator edits kept) and `conspect.toml`, and runs `multiview validate` on it. It stops
   here, changing nothing, unless the output has `OK   start: accepted state`. The keys it sets:

   ```toml
   [control]
   ui_dir = "/var/www/conspect-ui/<sha>"     # the build nginx served

   [network]
   https_port = 443                          # unchanged: UDP 443
   mode = "direct"                           # TCP door on 443 (tcp_port removed)
   public_host = "mda001.public-servers.sy4.aperim.net"
   cert_file = "/etc/conspect/tls/fullchain.pem"
   key_file = "/etc/conspect/tls/privkey.pem"
   acme_webroot = "/var/lib/conspect/acme-webroot"
   # http_redirect_port defaults to 80; advertise is kept
   [network.ice_tcp]
   enabled = true
   ```

   Both `desired.json` and `conspect.toml` get them: the engine starts from `desired.json`, and
   the config-file watcher commits the whole `conspect.toml` on any later edit of it.
   `[network] cert_file`/`key_file` give the door its own certificate and leave the loopback
   `[::1]:8080` listener plain HTTP (`[control.tls]` would switch that one to TLS too).
3. Keeps rollback copies (`desired.json.pre-cutover`, `conspect.toml.pre-cutover`,
   `nginx-T.pre-cutover.txt` in `~/mv`), installs `/etc/systemd/system/conspect.socket` and
   `/etc/systemd/system/conspect.service.d/10-socket-activation.conf`, then:
   `systemctl stop conspect` -> `systemctl disable --now nginx` -> install the new state ->
   `systemctl enable --now conspect.socket` -> `systemctl start conspect`. It prints how long the
   site was down (expect the engine's start-up time, a few seconds).
4. Switches certbot's renewal for the lineage from the nginx authenticator to
   `authenticator = webroot` with `webroot_path = /var/lib/conspect/acme-webroot` (the old file
   is kept as `<host>.conf.pre-cutover`) and runs `certbot renew --dry-run` against Let's
   Encrypt staging, through the engine's port 80.

## 2. Checks (paste as they are)

```bash
H=mda001.public-servers.sy4.aperim.net
# Sockets: systemd holds them, the engine has them.
systemctl --no-pager status conspect.socket conspect | grep -E 'Active|Listen'
sudo ss -ltnup '( sport = :443 or sport = :80 )'     # users: multiview (and systemd)
journalctl -u conspect --since -5min | grep -E 'socket activation|inherited|TCP door|serves the site|serving the web UI|front door: UDP'
#   expect: "inherited sockets ... tcp=[[::]:443, [::]:80] udp=[[::]:443, [::]:443]",
#   "front-door TCP door serves the site", "serving the web UI from control.ui_dir",
#   and "validated flows steered" (or the warning with the reason, see Troubleshoot).

# TLS h1 on 443, the certificate, the UI:
curl -sS -o /dev/null -w '%{http_code} %{http_version} %{ssl_verify_result}\n' https://$H/
echo | openssl s_client -connect $H:443 -servername $H -alpn http/1.1 2>/dev/null \
  | grep -E 'subject=|issuer=|ALPN|Verify return'
curl -sSI https://$H/ | grep -iE '^(HTTP|cache-control|etag|content-type)'
#   200, cache-control: no-cache, etag: "..."
# Every asset the index references answers 200:
curl -s https://$H/ | grep -o '/assets/[^"]*' | sort -u | while read -r a; do
  printf '%s %s\n' "$(curl -s -o /dev/null -w '%{http_code}' "https://$H$a")" "$a"; done | sort | uniq -c -w3
#   all lines start with 200
curl -sSI -H 'Accept-Encoding: gzip' "https://$H$(curl -s https://$H/ | grep -o '/assets/index-[^"]*\.js' | head -1)" \
  | grep -iE '^(HTTP|cache-control|content-encoding)'
#   200, public, max-age=31536000, immutable, content-encoding: gzip

# Port 80: redirect, never a page.
curl -sSI http://$H/some/path?x=1 | grep -iE '^(HTTP|location)'
#   301, Location: https://<host>/some/path?x=1

# API through the door (token as usual, never printed):
TOKEN=$(sudo sed -n 's/^MULTIVIEW_CONTROL_TOKEN=//p' /etc/conspect/env)
curl -s -o /dev/null -w '%{http_code}\n' -H "Authorization: Bearer admin.$TOKEN" https://$H/api/v1/health

# HLS headers. P/PL = a live HLS output's route and playlist (GET /api/v1/outputs shows
# hls_route / the playlist name; e.g. /hls/program and program.m3u8):
P=/hls/program; PL=program.m3u8
curl -sSI https://$H$P/$PL | grep -iE '^(HTTP|cache-control|content-type)'
#   application/vnd.apple.mpegurl, max-age=1, must-revalidate
SEG=$(curl -s https://$H$P/$PL | grep -v '^#' | head -1)
curl -sSI https://$H$P/$SEG | grep -iE '^(HTTP|cache-control|etag|accept-ranges|content-type)'
curl -sS -o /dev/null -w '%{http_code}\n' -H 'Range: bytes=0-99' https://$H$P/$SEG     # 206
curl -sSI -H 'Origin: https://receiver.example' https://$H$P/$SEG | grep -i access-control-allow-origin

# ICE-TCP on 443 (browser rows below). From the host, plaintext is refused:
printf 'GET / HTTP/1.0\r\n\r\n' | timeout 3 bash -c "cat > /dev/tcp/$H/443" ; echo "exit $? (connection closed without a response)"

# A restart keeps the sockets: no refused connection while the engine restarts.
( for i in $(seq 1 200); do curl -s -o /dev/null -w '%{http_code}\n' --max-time 15 https://$H/; sleep 0.05; done ) > /tmp/during-restart.txt &
sleep 1; sudo systemctl restart conspect; wait
sort /tmp/during-restart.txt | uniq -c        # only 200 (a slow one during start-up, never 000)

# The certificate renews through the engine:
sudo certbot renew --cert-name $H --dry-run
```

Segment time-to-first-byte against the nginx path is measured once, before nginx is stopped,
with nginx moved to a spare port (see "TTFB measurement" below); after the cutover the same tool
records the engine's absolute numbers.

## 3. Rollback (one command)

```bash
cd ~/conspect && sudo bash deploy/systemd/front-door-rollback.sh
```

It builds the rolled-back state in a scratch copy (current `desired.json` with only `network`
and `control` taken from `desired.json.pre-cutover`, and `conspect.toml.pre-cutover`), validates
it and stops unless `OK   start: accepted state`, then stops the engine and `conspect.socket`,
removes the drop-in, restores the certbot renewal file, starts nginx, starts the engine (UDP 443
only, bound by the engine as before). The binary stays: it runs the old state unchanged.

## Certificates: what renews, and when certbot is retired

* **Now (this runbook)**: certbot keeps the certificate. Renewal is HTTP-01 with
  `--webroot /var/lib/conspect/acme-webroot`; the engine's port-80 listener serves
  `/.well-known/acme-challenge/<token>` from that directory (base64url tokens only, regular files
  under 1 KiB, no symlinks) and redirects everything else. The deploy hook copies the renewed
  pair to `/etc/conspect/tls/`; the engine sees the change within 30 s and serves it on the next
  handshake (log: `static certificate changed on disk`). A broken or half-written pair is
  refused and the old certificate keeps serving. `certbot.timer` stays enabled. Check after
  the cutover: `sudo certbot renew --dry-run` (step 4 did it once) and
  `systemctl list-timers certbot.timer`.
* **Later, the engine's own ACME** (`[network.tls]`, merged with #338, runbook
  [acme-certificates](acme-certificates.md)): DNS-01 through Cloudflare (recommended, no port dependency) or TLS-ALPN-01 on
  TCP 443 (the door answers `acme-tls/1`). The switch, in this order:
  1. configure `[network.tls]` (and the Cloudflare token with `multiview tls set-credential`),
     **remove** `[network] cert_file`/`key_file` (the two are exclusive; validation says so) and
     keep `acme_webroot` or drop it;
  2. validate, restart, and wait for `multiview tls status` = `serving` with the CA's
     certificate (`openssl s_client` shows the new issuer/serial);
  3. only then retire certbot: `sudo systemctl disable --now certbot.timer`, remove the deploy
     hook `/etc/letsencrypt/renewal-hooks/deploy/conspect-copy.sh` and
     `/etc/conspect/certbot-hook.env`, and leave
     `/etc/letsencrypt` in place (the rollback of that step is re-enabling the timer and
     putting the two keys back).
  Never disable `certbot.timer` while `cert_file`/`key_file` are configured: that host's
  certificate could not renew.

## TTFB measurement (acceptance: segment TTFB within 2 ms of nginx's)

`scripts/hls-ttfb.py` (Python 3, standard library) alternates requests between targets on
kept-alive TLS connections and reports p50/p90/p99 TTFB and the delta to the first target.
Measure **before** step 1, while nginx still proxies `/hls/`, with the engine's TCP door staged
on a loopback port so both paths are on the same host and the same segment:

```bash
# Stage: in ~/mv/conspect.toml add (the watcher records a restart-pending section) then
# validate + restart once, as always:
#   [network] mode = "direct", tcp_port = 9443, bind = "[::1]", http_redirect_port = 0,
#             public_host = "<H>", cert_file/key_file = the copies made by the cutover script
#             step 1 (or run step 1 of the script by hand)
python3 ~/conspect/scripts/hls-ttfb.py --insecure --playlist --count 500 --max-delta-ms 2 \
  nginx=https://$H/hls/program/program.m3u8 \
  engine=https://[::1]:9443/hls/program/program.m3u8
```

Run it on the host (both paths share the loopback), and once from the office against the public
name after the cutover. Record the table in issue #292. Remove the staging keys (or run the
cutover, which replaces them) afterwards.

## HTTP/3 and Alt-Svc (front-door slice 2, #288)

From the engine build that contains slice 2, UDP `https_port` serves HTTP/3 for the whole site
and MoQ (WebTransport under `/moq/`) from one endpoint; there is no separate MoQ listener on the
front door any more. Nothing changes on the firewall (UDP 443 is already open).

* `MULTIVIEW_MOQ_ENDPOINT` must be `https://<public host>/moq`. A bare `https://<host>` is
  normalised to `/moq` with a warning; another path is refused (MoQ unavailable, the log says
  why). Validate before the restart, as always (`<binary> validate conspect.toml` -> exit 0).
* The endpoint's certificate is the door's: `[network.tls]` (starts only once the certificate is
  `serving`; never in `setup`), else `[network] cert_file/key_file` or `[control.tls]`, else the
  `MULTIVIEW_MOQ_CERTIFICATE`/`_PRIVATE_KEY` pair. With none, the log says "no certificate for the
  HTTP/3 endpoint" and the site stays on TCP.
* Check: the journal shows `front door: HTTP/3 endpoint up` with `alpn="h3" zero_rtt=false
  alt_svc=true`. `curl -sI https://<host>/ | grep -i alt-svc` shows
  `alt-svc: h3=":443"; ma=86400`. In Chrome, the second page load reports
  `performance.getEntriesByType('navigation')[0].nextHopProtocol === "h3"`.

**While nginx still serves TCP 443** (before the cutover above), the engine's TCP door is not on
443, so the browser only learns about h3 if nginx advertises it. Add one line to the TLS
`server { ... }` block, only once the slice-2 engine runs and UDP 443 reaches it (otherwise
browsers try h3, fail, and fall back after a delay for each `ma` period):

```nginx
add_header Alt-Svc 'h3=":443"; ma=86400' always;
```

`nginx -t && systemctl reload nginx`. Rollback: remove the line and reload.

**Switching Alt-Svc off** (UDP 443 known unreachable from clients, e.g. an upstream firewall
drops it): set `alt_svc = false` under `[network]`, validate, restart. The engine keeps serving
h3 to clients that already know it (until their `ma` expires) and stops advertising it; every
page then loads over HTTP/1.1 on TCP without the fallback delay. Until the reachability prober
(slice 9) exists this switch is manual. Rollback: remove the key (default `true`).

## Troubleshoot

| Symptom | Cause / fix |
| --- | --- |
| `conspect.socket` fails: `Address already in use` | nginx (or another service) still holds 443/80: `sudo ss -ltnup '( sport = :443 or sport = :80 )'`, `systemctl disable --now nginx` |
| Log: `front door: one UDP socket (validated-flow steering unavailable)` with `SO_ATTACH_REUSEPORT_CBPF failed on the inherited pair` | the unit lacks `ReusePort=yes` or has only one `ListenDatagram=`: reinstall `deploy/systemd/conspect.socket`, `systemctl daemon-reload`, `systemctl restart conspect.socket conspect`. Service keeps working single-socket meanwhile |
| `conspect.socket` fails on the second `ListenDatagram=[::]:443` (`Address already in use`) | the systemd on the host does not apply `ReusePort=yes` before the second bind: delete one `ListenDatagram=` line from `/etc/systemd/system/conspect.socket`, `daemon-reload`, restart the socket and the engine; the engine runs single-socket (logged). Check the pair with `sudo ss -lunp 'sport = :443'` (two sockets) |
| Log: `an inherited ... socket matches no listener` | the unit and `[network]` disagree on a port (e.g. `http_redirect_port = 0` with `ListenStream=[::]:80`); fix one of them |
| Site closes the connection right after the TLS handshake | no site certificate: check `cert_file`/`key_file` are readable by `operations` (`sudo -u operations head -c1 /etc/conspect/tls/privkey.pem`) and the log line `front-door TCP door serves the site` |
| Startup error `control.ui_dir ... has no index.html` | `ui_dir` points at the wrong directory; `ls /var/www/conspect-ui/<sha>/index.html` |
| `certbot renew --dry-run` fails with `Invalid response ... 404` | the webroot path differs between certbot and `[network] acme_webroot`, or the directory is not readable by `operations` |
| A deploy of a new UI build | copy it to `/var/www/conspect-ui/<new sha>/`, set `[control] ui_dir` to it in `conspect.toml` (the watcher commits it; it applies on the next restart), validate, restart. Or omit `ui_dir` to use the UI embedded in the binary |

## Evidence in the repository (what CI proves)

* `crates/multiview-frontdoor/tests/socket_activation.rs`: the real `systemd-socket-activate`
  hands TCP door, redirect and UDP sockets to the door (the first request, sent before the
  engine existed, is answered); a restart keeps the sockets (a connection and a datagram sent
  while no engine runs are served by the next one); an inherited `ReusePort` pair keeps
  validated-flow steering (50/50 datagrams on the right socket); without activation the door
  binds itself.
* `crates/multiview-output/tests/hls_nginx_parity.rs`: the 25-row nginx golden table.
* `crates/multiview-control/tests/embed_web.rs`: UI caching, `ETag`/`304`, gzip, stale chunk 404.
* `crates/multiview-control/tests/static_cert_reload.rs`: certbot-style symlink renewal served
  without a restart; a torn pair keeps the old certificate.
* `crates/multiview-frontdoor/tests/tcp_door.rs`: HTTP-01 webroot on port 80, the redirect, and
  site hellos held until the control plane attaches.
* `crates/multiview-cli/tests/frontdoor_site_chaos.rs`: the real tick loop keeps one frame per
  tick and no extra repeats while 64 stalled 4 MiB downloads, download/range lanes and a port-80
  flood hit the door; RSS stays bounded (no whole-file buffering).
