#!/bin/bash
# usage: make-cert.sh <dir>   Self-signed certificate for https://localhost (harness only; Chrome runs with
# --ignore-certificate-errors). Writes <dir>/key.pem and <dir>/cert.pem unless they already exist.
set -euo pipefail
dir="${1:?dir}"; mkdir -p "$dir"
[ -s "$dir/key.pem" ] && [ -s "$dir/cert.pem" ] && exit 0
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -days 30 -subj "/CN=localhost" \
  -addext "subjectAltName=DNS:localhost,IP:127.0.0.1,IP:::1" -keyout "$dir/key.pem" -out "$dir/cert.pem" 2>/dev/null
chmod 600 "$dir/key.pem"
