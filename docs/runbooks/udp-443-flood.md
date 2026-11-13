# Runbook — UDP 443 flood floor (ADR-0107, design note §11, slice 3, issues #289/#321/#325)

## What it is and why

The network front door shares one dual-stack UDP socket on port 443 between `MoQ` (QUIC) and
WebRTC. A volumetric flood on that port threatens two things: the kernel's receive queue (shared
by every flow on that socket) and, if the engine shared a runtime with the output clock, the
clock's own cadence. The design counters this in three independent layers (design note §11):

1. **Validated-flow steering** (in the engine, Linux only): established peers are steered to a
   second `SO_REUSEPORT` socket by a kernel CBPF program, so a flood fills the listener's queue,
   never the paying viewers'. Already built (slice 3 part A); nothing here configures it.
2. **This runbook's layer: an nftables per-source floor on UDP 443**, applied *before* a packet
   reaches the engine's socket. Two tiers, each per IPv4 address and per IPv6 /64:
   * **new** flows (`ct state new,untracked`): over 2000/s, burst 1000, dropped;
   * **all** UDP 443 packets, established or not: over 25 000/s, burst 10 000, dropped. That is
     above the engine's own per-established-peer bucket (20 000/s), so a real viewer never hits it.

   There is no conntrack "established" exemption. One server reply (a QUIC Retry or Initial, a STUN
   error) is enough for conntrack to call a source established, so an exemption would let that
   source flood unmetered. The floor stops a single real (non-spoofed) source. It does **not** stop
   a spoofed or distributed flood (nftables has no return-routability proof); that remains an
   upstream/DDoS-provider problem.
3. **Receive-buffer sizing** (`net.core.rmem_max` below), so the kernel queue itself is as deep as
   this host allows before anything overflows it.

The engine runs unprivileged (no `CAP_NET_ADMIN`) and cannot list the nftables ruleset itself, so
it reads a status file a privileged systemd `ExecStartPre=+` writes at every start
(`network.udp.nft_floor_status_file`, default `/run/conspect/nft-floor.json`).

## Resource identity

- **nftables table:** `inet conspect_floor` — `deploy/nftables/conspect-udp443-floor.nft`.
- **systemd units (examples, not auto-installed):** `deploy/systemd/conspect.service`,
  `deploy/systemd/conspect.socket` (placeholder only — socket activation is slice 6, not wired up).
- **sysctl file:** `deploy/sysctl/90-conspect-udp.conf` → `/etc/sysctl.d/90-conspect-udp.conf`.
- **Status file the engine reads:** `/run/conspect/nft-floor.json` (or
  `network.udp.nft_floor_status_file`), written by `conspect.service`'s `ExecStartPre=+`.
- **Meters (exact names, all four required for `present`):** `conspect_udp443_new_v4`,
  `conspect_udp443_new_v6`, `conspect_udp443_all_v4`, `conspect_udp443_all_v6`.
- **Parser:** `multiview_cli::nft_floor` (pure file parse, re-read every 10 minutes on the
  front-door runtime — never on the program/output-clock runtime). `present`: nft JSON with the
  table and all four meters. `absent`: nft's exact missing-table error (`Error: No such file or
  directory` / `list table inet conspect_floor`) or the word `absent`. Anything else: `unknown`.
- **Floor behaviour test:** `deploy/nftables/test-floor.sh` (not run in CI; see Verify).
- **Flood tool:** `cargo run --release -p multiview-frontdoor --example udp443_flood -- ...`.
- **Chaos suite:** `cargo test --release -p multiview-frontdoor --test chaos_flood`
  (`FRONTDOOR_CHAOS_SECS`, `FRONTDOOR_CHAOS_PPS` override the defaults).

## Install

1. **sysctl (raise the usable receive buffer):**

   ```bash
   sudo install -m 0644 deploy/sysctl/90-conspect-udp.conf /etc/sysctl.d/90-conspect-udp.conf
   sudo sysctl --system
   sysctl net.core.rmem_max   # expect >= 8388608
   ```

2. **nftables floor:**

   ```bash
   sudo nft -f deploy/nftables/conspect-udp443-floor.nft
   ```

3. **systemd (adjust `User=`/paths for your host, then):**

   ```bash
   sudo install -m 0644 deploy/systemd/conspect.service /etc/systemd/system/conspect.service
   sudo systemctl daemon-reload
   sudo systemctl enable --now conspect
   ```

   `conspect.socket` is **not** enabled: socket activation is slice 6; the service binds its own
   sockets today.

## Verify

```bash
# The floor table and its rule counters:
sudo nft list table inet conspect_floor

# What the engine itself saw at its last (re-)read:
cat /run/conspect/nft-floor.json

# Validate the nft file without installing it (useful in a container without
# full CAP_NET_ADMIN, or before touching a live host). `sudo nft -c -f <path>`
# alone fails in a container with a netlink cache-init permission error even
# for a syntax-only check; inside an unprivileged user + network namespace it
# fully parses the ruleset:
unshare -Urn nft -c -f - < deploy/nftables/conspect-udp443-floor.nft
echo $?   # 0 = valid syntax

# Behaviour test: loads the file into a throwaway namespace and floods it.
deploy/nftables/test-floor.sh   # exit 0 = every scenario PASS
```

`test-floor.sh` needs `nft`, `unshare` (user namespaces), `python3` and `ip`. It skips, exiting 0,
when any of them is missing. It is **not** part of CI, because the runners have no nft. Run it after
any edit to the nft file. A separate probe table hooked after the floor counts what got through.
Results with nft 1.1.3, 2 s per scenario, about 200-240k pps offered from one source:

| Scenario | Delivered | Before this fix (`ct state established accept`) |
|---|---|---|
| (a) one-way flood, one IPv4 or IPv6 source | 5000 (2500 pps: new tier) | 5000 |
| (b) one server reply, then flood | 60 000 (30 000 pps: all tier) | all 451 457 (unmetered) |
| (c) two addresses in one /64 | 5000 together (one bucket) | 5000 |
| (c) control: two different /64s | 9998 (two buckets) | — |

Read the engine's own view of the floor and the rest of the front door's health:

- `GET /api/v1/system/capabilities` → `network.nft_floor` (`"present"` / `"absent"` / `"unknown"`,
  the **startup** read), `network.udp443_steering`, `network.recv_buffer`,
  `network.kernel_drops_supported`.
- `GET /api/v1/metrics/frontdoor` → the live (periodically re-read) `nft_floor`, the live
  `StatsSnapshot` counters (`received`, `dropped_*`, `steering_*`, `kernel_drops_*`), and
  `quic_retries`.
- Kernel receive drops directly: the `drops` column of `/proc/net/udp6` for the front door's
  socket inode(s), or `nstat -az UdpRcvbufErrors Udp6RcvbufErrors` for the whole host.

## What an operator does during an attack

The floor and the kernel buffer only bound a single real source; a distributed or spoofed
volumetric attack is beyond what this host can filter for itself. During one:

1. Confirm it is volumetric, not a code regression: `GET /api/v1/metrics/frontdoor` — rising
   `received`/`dropped_*` with `steering_updates` flat and `udp443_steering = "reuseport_cbpf"`
   means steering is doing its job; established viewers' own counters
   (`received_established_socket`, `dropped_established_peer`) should stay far below the listener's.
2. If kernel drops are rising (`kernel_drops_listener`, or `nstat UdpRcvbufErrors` climbing), raise
   `net.core.rmem_max` further and re-apply the sysctl, or escalate to the upstream/DDoS-provider
   layer (design note §11, layer 3) — this host cannot out-buffer a line-rate flood alone.
3. Escalate to the network/hosting provider's DDoS mitigation for anything that outruns the floor
   and buffer (a spoofed or sufficiently distributed source set). Do **not** attempt to raise the
   nftables meters' rates as a response to an attack already past them. They are sized to a
   single legitimate source's traffic pattern, not as an incident-response knob. If you do edit
   them, rerun `deploy/nftables/test-floor.sh` and keep the exact meter names, or the engine
   reports the floor as `unknown`.
4. The chaos suite's flood tool (below) is for **pre-incident** capacity testing on a controlled
   target, never pointed at a host you do not own.

## Flood tool and chaos suite (testing, never against a host you do not own)

```bash
# A controlled flood against a test instance (adjust --target):
cargo run --release -p multiview-frontdoor --example udp443_flood -- \
  --target '[::1]:8443' --pps 20000 --secs 10 --threads 4 --shapes initial,stun,rtp

# The real-clock chaos suite (engine tick loop + flood, asserts no unexplained lateness):
FRONTDOOR_CHAOS_SECS=10 FRONTDOOR_CHAOS_PPS=20000 \
  cargo test --release -p multiview-frontdoor --test chaos_flood
```

## Rollback

```bash
sudo nft delete table inet conspect_floor
sudo systemctl disable --now conspect   # if installed via this runbook
sudo rm -f /etc/sysctl.d/90-conspect-udp.conf
sudo sysctl --system
```

Removing the nftables table leaves kernel-level UDP 443 traffic unfiltered at that layer;
validated-flow steering (in the engine, not nftables) is unaffected either way.
