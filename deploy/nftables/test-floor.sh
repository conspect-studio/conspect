#!/usr/bin/env bash
# Behaviour test for deploy/nftables/conspect-udp443-floor.nft (issue #289).
#
# Loads the shipped file into a throwaway network namespace (`unshare -Urn`:
# an unprivileged user namespace, root inside it, nothing touches the host
# ruleset) and sends UDP 443 on loopback from chosen source addresses:
#   (a) one-way flood from one source: capped by the NEW tier
#       (2000/s burst 1000), and charged to it ONLY (the all-tier sets stay
#       empty);
#   (b) the server replies once, then the same source floods for 3 s (past
#       the new bucket's refill): conntrack now says "established" and the
#       source is capped by the ALL tier (25000/s burst 10000) — and ONLY by
#       it: delivery must reach the all-tier rate, proving established
#       packets are not also charged to the new tier;
#   (c) two addresses in the same IPv6 /64 share one bucket; an address in
#       another /64 gets its own;
#   (d) a FULL set fails closed: the shipped file with `size 65536`
#       substituted to `size 2` (temp copy), two sources fill the new-tier
#       set, a third source then delivers 0 (IPv4 and IPv6);
#   (e) a legitimate established flow paced at 10000 pps for 3 s (one viewer
#       household) is delivered >= 99 %.
# Delivered is counted by a separate probe table hooked AFTER the floor
# (`inet floor_probe`, priority 100, `udp dport 443 counter`): what the floor
# let through, independent of the receiver's socket buffer and of whether the
# ruleset under test carries counters of its own.
#
# Not part of CI: the runners have no nft. Skips (exit 0) when nft, unshare,
# python3 or ip is missing, or when user namespaces are unavailable.
#
# Usage: deploy/nftables/test-floor.sh   (no root needed; DURATION=secs)
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
rules="${here}/conspect-udp443-floor.nft"

if [[ "${CONSPECT_FLOOR_INNER:-}" != "1" ]]; then
	for tool in nft unshare python3 ip; do
		if ! command -v "$tool" >/dev/null 2>&1; then
			echo "SKIP: $tool not installed"
			exit 0
		fi
	done
	if ! unshare -Urn true 2>/dev/null; then
		echo "SKIP: unshare -Urn (user + network namespace) not permitted here"
		exit 0
	fi
	CONSPECT_FLOOR_INNER=1 exec unshare -Urn "$0" "$@"
fi

duration="${DURATION:-2}"
ip link set lo up
for addr in 2001:db8:1::a 2001:db8:1::b 2001:db8:2::c 2001:db8:3::d 2001:db8:ff::1; do
	ip -6 addr add "${addr}/128" dev lo nodad
done

# reload [RULES]: load RULES (default: the shipped file) plus the probe.
reload() {
	nft delete table inet conspect_floor 2>/dev/null || true
	nft delete table inet floor_probe 2>/dev/null || true
	nft -f "${1:-$rules}"
	nft -f - <<'NFT'
table inet floor_probe {
	chain input {
		type filter hook input priority 100; policy accept;
		udp dport 443 counter
	}
}
NFT
}

# Packets the floor let through to UDP 443 since the last reload.
delivered() {
	nft -j list table inet floor_probe | python3 -c '
import json, sys
total = 0
for item in json.load(sys.stdin)["nftables"]:
    for expr in item.get("rule", {}).get("expr", []):
        if "counter" in expr:
            total += expr["counter"]["packets"]
print(total)'
}

# flood SERVER SRC... : send as fast as possible for $duration s (or $SECS)
# from each source (round robin); print "sent elapsed". REPLY_FIRST=1 first
# sends one datagram from each source and has the server answer it
# (conntrack then marks the flow established); those warm-up datagrams are
# counted in "sent". COUNT=n sends exactly n per source instead. PPS=n paces the total send rate to n packets/s.
flood() {
	python3 - "${SECS:-$duration}" "$@" <<'PY'
import os, socket, sys, time
duration = float(sys.argv[1]); server = sys.argv[2]; sources = sys.argv[3:]
pps = float(os.environ.get("PPS", "0"))
fam = socket.AF_INET6 if ":" in server else socket.AF_INET
srv = socket.socket(fam, socket.SOCK_DGRAM)
srv.bind((server, 443))
socks = []
for src in sources:
    s = socket.socket(fam, socket.SOCK_DGRAM)
    s.bind((src, 0))
    s.setblocking(False)
    socks.append(s)
warm = 0
if os.environ.get("REPLY_FIRST") == "1":
    srv.settimeout(2)
    for s in socks:
        s.sendto(b"hello", (server, 443))
        _, peer = srv.recvfrom(64)
        srv.sendto(b"reply", peer)
        warm += 1
    for s in socks:
        s.setblocking(True); s.settimeout(2); s.recvfrom(64); s.setblocking(False)
payload = b"x" * 64
sent = 0
if os.environ.get("COUNT"):
    for s in socks:
        for _ in range(int(os.environ["COUNT"])):
            s.sendto(payload, (server, 443)); sent += 1
    print(sent + warm, 0.0)
    sys.exit(0)
start = time.monotonic(); end = start + duration
while True:
    now = time.monotonic()
    if now >= end:
        break
    if pps and sent >= pps * (now - start):
        time.sleep(0.0005)
        continue
    for s in socks:
        for _ in range(64 if not pps else 10):
            try:
                s.sendto(payload, (server, 443)); sent += 1
            except BlockingIOError:
                pass
print(sent + warm, time.monotonic() - start)
PY
}

fail=0
# check NAME SENT ELAPSED DELIVERED BURST RATE
# Delivered must stay within burst + rate * elapsed (+10 % slack for timer
# granularity) and below what was sent.
check() {
	local name="$1" sent="$2" elapsed="$3" got="$4" burst="$5" rate="$6"
	python3 - "$name" "$sent" "$elapsed" "$got" "$burst" "$rate" <<'PY' || fail=1
import sys
name, sent, elapsed, delivered, burst, rate = sys.argv[1], int(sys.argv[2]), float(sys.argv[3]), int(sys.argv[4]), int(sys.argv[5]), int(sys.argv[6])
drop = sent - delivered
bound = (burst + rate * elapsed) * 1.10
ok = drop > 0 and delivered <= bound
print(f"{'PASS' if ok else 'FAIL'} {name}: sent {sent} in {elapsed:.2f}s ({sent/elapsed:.0f} pps), "
      f"dropped {drop}, delivered {delivered} ({delivered/elapsed:.0f} pps), bound {bound:.0f}")
sys.exit(0 if ok else 1)
PY
}

echo "nft $(nft --version | awk '{print $2}'), duration ${duration}s per scenario"

# (a) one-way flood, new tier.
for pair in "127.0.0.1 127.0.0.2" "2001:db8:ff::1 2001:db8:1::a"; do
	set -- $pair
	reload
	read -r sent elapsed < <(flood "$1" "$2")
	check "(a) one-way flood $2 -> new tier" "$sent" "$elapsed" "$(delivered)" 1000 2000
	# Exactly one tier: no packet of this flood (all ct state new) may have
	# been charged to the all tier, so the all-tier sets hold no element.
	all_elems=$(nft -j list table inet conspect_floor | python3 -c '
import json, sys
n = 0
for item in json.load(sys.stdin)["nftables"]:
    s = item.get("set")
    if s and s["name"] in ("udp443_all_v4", "udp443_all_v6"):
        n += len(s.get("elem", [])) + 1
print(n)')
	if (( all_elems == 2 )); then
		echo "PASS (a) $2 new-flow packets charged to the new tier only (all-tier sets empty)"
	else
		echo "FAIL (a) $2 all-tier sets missing or charged: $(( all_elems - 2 )) element(s) or sets absent"
		fail=1
	fi
done

# (b) one reply, then flood for 3 s: capped by the all tier only. The upper
# bound is the all tier's; the lower bound (80 % of burst + rate * elapsed)
# fails if established packets were ALSO charged to the new tier (which
# would cap them near 1000 + 2000 * elapsed).
for pair in "127.0.0.1 127.0.0.3" "2001:db8:ff::1 2001:db8:1::a"; do
	set -- $pair
	reload
	read -r sent elapsed < <(SECS=3 REPLY_FIRST=1 flood "$1" "$2")
	got="$(delivered)"
	check "(b) reply then flood $2 -> all tier" "$sent" "$elapsed" "$got" 10000 25000
	lower=$(python3 -c "print(int((10000 + 25000 * $elapsed) * 0.8))")
	if (( got >= lower )); then
		echo "PASS (b) $2 not double-charged: delivered ${got} >= ${lower}"
	else
		echo "FAIL (b) $2 double-charged: delivered ${got} < ${lower}"
		fail=1
	fi
done

# (c) /64 siblings share one new-tier bucket; another /64 does not.
reload
read -r sent elapsed < <(flood 2001:db8:ff::1 2001:db8:1::a 2001:db8:1::b)
check "(c) two addresses, one /64 -> one bucket" "$sent" "$elapsed" "$(delivered)" 1000 2000
reload
read -r sent elapsed < <(flood 2001:db8:ff::1 2001:db8:1::a 2001:db8:2::c)
got="$(delivered)"
lower=$(python3 -c "print(int(2 * 1000 + 2 * 2000 * $elapsed * 0.8))")
if (( got >= lower )); then
	echo "PASS (c) control, two /64s -> two buckets: delivered ${got} >= ${lower}"
else
	echo "FAIL (c) control, two /64s -> two buckets: delivered ${got} < ${lower}"
	fail=1
fi

# (d) a full set fails closed: size 2, two sources fill the new-tier set
# (one datagram each, both delivered), then a third source floods.
small="$(mktemp)"
trap 'rm -f "$small"' EXIT
sed 's/size 65536;/size 2;/' "$rules" >"$small"
for trio in "127.0.0.1 127.0.0.4 127.0.0.5 127.0.0.6" \
	"2001:db8:ff::1 2001:db8:1::a 2001:db8:2::c 2001:db8:3::d"; do
	set -- $trio
	reload "$small"
	COUNT=1 flood "$1" "$2" >/dev/null
	COUNT=1 flood "$1" "$3" >/dev/null
	filled="$(delivered)"
	read -r sent elapsed < <(flood "$1" "$4")
	got=$(( $(delivered) - filled ))
	if (( filled == 2 && got == 0 && sent > 0 )); then
		echo "PASS (d) full set (size 2) fails closed for $4: fill delivered ${filled}, then sent ${sent}, delivered ${got}"
	else
		echo "FAIL (d) full set (size 2) for $4: fill delivered ${filled}, then sent ${sent}, delivered ${got}"
		fail=1
	fi
done

# (e) a legitimate established flow at 10000 pps for 3 s: >= 99 % delivered.
for pair in "127.0.0.1 127.0.0.7" "2001:db8:ff::1 2001:db8:1::b"; do
	set -- $pair
	reload
	read -r sent elapsed < <(SECS=3 PPS=10000 REPLY_FIRST=1 flood "$1" "$2")
	got="$(delivered)"
	if python3 -c "import sys; sys.exit(0 if $got >= 0.99 * $sent else 1)"; then
		echo "PASS (e) established $2 at ~10000 pps: sent ${sent} in $(printf %.2f "$elapsed")s, delivered ${got}"
	else
		echo "FAIL (e) established $2 at ~10000 pps: sent ${sent} in $(printf %.2f "$elapsed")s, delivered ${got} (< 99 %)"
		fail=1
	fi
done

exit "$fail"
