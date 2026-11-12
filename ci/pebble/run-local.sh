#!/usr/bin/env bash
# Start / stop the Pebble ACME test stack used by `tests/acme_pebble.rs`.
# Shared by the `acme-pebble` CI job and local runs.
#
#   ci/pebble/run-local.sh start   # starts challtestsrv + 3 Pebbles, waits, writes env
#   ci/pebble/run-local.sh stop    # kills what `start` started
#
# Inputs (environment):
#   PEBBLE_BIN        path to the pebble binary (v2.10.3)
#   CHALLTESTSRV_BIN  path to the pebble-challtestsrv binary (v2.10.3)
#   PEBBLE_WORK       work dir holding test/certs/pebble.minica.pem and
#                     test/certs/localhost/{cert,key}.pem (Pebble's own test PKI;
#                     no private key is committed to this repository)
#   PEBBLE_PORT_SHIFT optional (default 0): add this to every port (directories
#                     14000-14002, management 15000-15002, challtestsrv DNS 8053 /
#                     management 8055, tlsPort 5001, httpPorts 5002-5004) so a local
#                     run can coexist with another stack on the default ports.
# Output: $PEBBLE_WORK/pebble.env with the PEBBLE_* / CHALLTESTSRV_* variables
# the tests read (source it, or append it to $GITHUB_ENV).
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
work="${PEBBLE_WORK:?set PEBBLE_WORK}"
pids="$work/pids"

stop() {
  if [[ -f "$pids" ]]; then
    while read -r pid; do kill "$pid" 2>/dev/null || true; done <"$pids"
    rm -f "$pids"
  fi
}

start() {
  : "${PEBBLE_BIN:?set PEBBLE_BIN}" "${CHALLTESTSRV_BIN:?set CHALLTESTSRV_BIN}"
  stop
  cd "$work"
  shift_by="${PEBBLE_PORT_SHIFT:-0}"
  p() { echo $(($1 + shift_by)); }
  dns_port=$(p 8053)
  mgmt_port=$(p 8055)
  tls_port=$(p 5001)
  "$CHALLTESTSRV_BIN" -http01 "" -https01 "" -tlsalpn01 "" -doh "" \
    -dnsserver ":$dns_port" -management ":$mgmt_port" \
    -defaultIPv4 127.0.0.1 -defaultIPv6 ::1 >"$work/challtestsrv.log" 2>&1 &
  echo $! >>"$pids"
  for name in standard eab nonce; do
    reject=0
    [[ "$name" == nonce ]] && reject=40
    sed -e "s/:14000\"/:$(p 14000)\"/; s/:14001\"/:$(p 14001)\"/; s/:14002\"/:$(p 14002)\"/" \
      -e "s/:15000\"/:$(p 15000)\"/; s/:15001\"/:$(p 15001)\"/; s/:15002\"/:$(p 15002)\"/" \
      -e "s/\"tlsPort\": 5001/\"tlsPort\": $tls_port/" \
      -e "s/\"httpPort\": \(500[2-4]\)/\"httpPort\": \1+$shift_by/" \
      "$here/pebble-$name.json" >"$work/pebble-$name.json"
    # sed cannot add; finish the httpPort arithmetic here.
    http=$(grep -o '"httpPort": [0-9]*' "$here/pebble-$name.json" | grep -o '[0-9]*$')
    sed -i "s/\"httpPort\": [0-9]*+[0-9]*/\"httpPort\": $(p "$http")/" "$work/pebble-$name.json"
    PEBBLE_VA_NOSLEEP=1 PEBBLE_AUTHZREUSE=0 PEBBLE_WFE_NONCEREJECT=$reject \
      "$PEBBLE_BIN" -config "$work/pebble-$name.json" -dnsserver "127.0.0.1:$dns_port" \
      >"$work/pebble-$name.log" 2>&1 &
    echo $! >>"$pids"
  done
  root="$work/test/certs/pebble.minica.pem"
  for port in $(p 14000) $(p 14001) $(p 14002); do
    for _ in $(seq 1 60); do
      if curl -fsS --cacert "$root" "https://localhost:$port/dir" >/dev/null 2>&1; then break; fi
      sleep 0.5
    done
    curl -fsS --cacert "$root" "https://localhost:$port/dir" >/dev/null
  done
  for _ in $(seq 1 60); do
    if curl -fsS -X POST -d '{"host":"probe.invalid.","value":"x"}' "http://127.0.0.1:$mgmt_port/set-txt" >/dev/null 2>&1; then break; fi
    sleep 0.5
  done
  curl -fsS -X POST -d '{"host":"probe.invalid."}' "http://127.0.0.1:$mgmt_port/clear-txt" >/dev/null
  cat >"$work/pebble.env" <<ENV
PEBBLE_DIRECTORY=https://localhost:$(p 14000)/dir
PEBBLE_EAB_DIRECTORY=https://localhost:$(p 14001)/dir
PEBBLE_NONCE_DIRECTORY=https://localhost:$(p 14002)/dir
PEBBLE_ROOT=$root
PEBBLE_TLS_PORT=$tls_port
CHALLTESTSRV_MGMT=http://127.0.0.1:$mgmt_port
CHALLTESTSRV_DNS=127.0.0.1:$dns_port
ENV
  echo "pebble stack up; env in $work/pebble.env"
}

case "${1:-}" in
  start) start ;;
  stop) stop ;;
  *) echo "usage: $0 start|stop" >&2; exit 2 ;;
esac
