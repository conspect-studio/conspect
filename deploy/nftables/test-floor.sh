#!/usr/bin/env bash
# Behaviour test for deploy/nftables/conspect-udp443-floor.nft (issue #289).
#
# Loads the shipped file into a throwaway network namespace (`unshare -Urn`:
# an unprivileged user namespace, root inside it, nothing touches the host
# ruleset) and floods UDP 443 on loopback from chosen source addresses:
#   (a) one-way flood from one source: capped by the NEW tier
#       (2000/s burst 1000);
#   (b) the server replies once, then the same source floods: conntrack now
#       says "established", yet the source is still capped by the ALL tier
#       (25000/s burst 10000) — one reply does not lift the floor;
#   (c) two addresses in the same IPv6 /64 share one bucket; an address in
#       another /64 gets its own.
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
for addr in 2001:db8:1::a 2001:db8:1::b 2001:db8:2::c 2001:db8:ff::1; do
	ip -6 addr add "${addr}/128" dev lo nodad
done

reload() {
	nft delete table inet conspect_floor 2>/dev/null || true
	nft delete table inet floor_probe 2>/dev/null || true
	nft -f "$rules"
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

# flood SERVER SRC... : send as fast as possible for $duration s from each
# source (round robin); print "sent elapsed". REPLY_FIRST=1 first sends one
# datagram from each source and has the server answer it (conntrack then
# marks the flow established), outside the measured window.
flood() {
	python3 - "$duration" "$@" <<'PY'
import os, socket, sys, time
duration = float(sys.argv[1]); server = sys.argv[2]; sources = sys.argv[3:]
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
start = time.monotonic(); end = start + duration
while time.monotonic() < end:
    for s in socks:
        for _ in range(64):
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
done

# (b) one reply, then flood: still capped, by the all tier.
for pair in "127.0.0.1 127.0.0.3" "2001:db8:ff::1 2001:db8:1::a"; do
	set -- $pair
	reload
	read -r sent elapsed < <(REPLY_FIRST=1 flood "$1" "$2")
	check "(b) reply then flood $2 -> all tier" "$sent" "$elapsed" "$(delivered)" 10000 25000
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

exit "$fail"
