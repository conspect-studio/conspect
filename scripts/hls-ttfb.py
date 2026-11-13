#!/usr/bin/env python3
"""Segment time-to-first-byte: the engine vs the nginx path (issue #292).

Slice 6 acceptance: "segment TTFB within 2 ms of nginx's". This measures it
the same way for any number of URLs, alternating between them request by
request so both see the same host load:

    scripts/hls-ttfb.py --count 400 \
        nginx=https://mda001.example.net/hls/program/seg123.m4s \
        engine=https://mda001.example.net:8443/hls/program/seg123.m4s

TTFB here is: request written on an already-open TLS connection -> first
response byte read (the server's own latency; TCP and TLS setup are excluded
by reusing one keep-alive connection per target, and reported separately as
`connect`). Each request is a full GET of the segment, body drained. Use a
segment that exists for the whole run (an older one from the playlist), or
`--playlist URL` to pick the oldest segment of a live playlist on each side.

Output: per target n, p50, p90, p99, max (ms) and the p50/p90 delta to the
first target. Exit 1 when `--max-delta-ms` is given and the p50 delta of any
target exceeds it. Python 3.8+, standard library only (runs on the host).
"""

from __future__ import annotations

import argparse
import http.client
import socket
import ssl
import statistics
import sys
import time
import urllib.parse


def connect(url: urllib.parse.SplitResult, insecure: bool, timeout: float):
    host = url.hostname or ""
    if url.scheme == "https":
        ctx = ssl.create_default_context()
        if insecure:
            ctx.check_hostname = False
            ctx.verify_mode = ssl.CERT_NONE
        ctx.set_alpn_protocols(["http/1.1"])
        conn = http.client.HTTPSConnection(host, url.port or 443, timeout=timeout, context=ctx)
    else:
        conn = http.client.HTTPConnection(host, url.port or 80, timeout=timeout)
    conn.connect()
    conn.sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    return conn


def path_of(url: urllib.parse.SplitResult) -> str:
    return (url.path or "/") + (f"?{url.query}" if url.query else "")


def oldest_segment(playlist: str, insecure: bool, timeout: float) -> str:
    url = urllib.parse.urlsplit(playlist)
    conn = connect(url, insecure, timeout)
    conn.request("GET", path_of(url))
    body = conn.getresponse().read().decode("utf-8", "replace")
    conn.close()
    for line in body.splitlines():
        if line and not line.startswith("#"):
            return urllib.parse.urljoin(playlist, line.strip())
    raise SystemExit(f"no segment in {playlist}")


class Target:
    def __init__(self, name: str, url: str, insecure: bool, timeout: float):
        self.name = name
        self.url = urllib.parse.urlsplit(url)
        self.insecure = insecure
        self.timeout = timeout
        self.conn = None
        self.samples: list[float] = []
        self.connects: list[float] = []
        self.status: dict[int, int] = {}
        self.bytes = 0

    def once(self) -> None:
        if self.conn is None:
            t0 = time.perf_counter()
            self.conn = connect(self.url, self.insecure, self.timeout)
            self.connects.append((time.perf_counter() - t0) * 1000.0)
        conn = self.conn
        conn.putrequest("GET", path_of(self.url), skip_accept_encoding=True)
        conn.putheader("Accept-Encoding", "identity")
        conn.endheaders()
        t0 = time.perf_counter()
        # Block for the first byte of the response, then parse it.
        conn.sock.recv(1, socket.MSG_PEEK)
        ttfb = (time.perf_counter() - t0) * 1000.0
        response = conn.getresponse()
        data = response.read()
        self.status[response.status] = self.status.get(response.status, 0) + 1
        self.bytes = len(data)
        self.samples.append(ttfb)
        if response.will_close:
            conn.close()
            self.conn = None


def pct(values: list[float], p: float) -> float:
    ordered = sorted(values)
    if not ordered:
        return float("nan")
    k = min(len(ordered) - 1, max(0, round(p / 100.0 * (len(ordered) - 1))))
    return ordered[k]


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("targets", nargs="+", help="name=URL (segment URL, or playlist URL with --playlist)")
    ap.add_argument("--count", type=int, default=300, help="requests per target (default 300)")
    ap.add_argument("--warmup", type=int, default=20, help="discarded requests per target first")
    ap.add_argument("--playlist", action="store_true", help="targets are playlists: fetch the oldest segment")
    ap.add_argument("--insecure", action="store_true", help="do not verify certificates")
    ap.add_argument("--timeout", type=float, default=10.0)
    ap.add_argument("--max-delta-ms", type=float, default=None, help="fail when a p50 delta exceeds this")
    args = ap.parse_args()

    targets = []
    for spec in args.targets:
        name, sep, url = spec.partition("=")
        if not sep:
            ap.error(f"target {spec!r} is not name=URL")
        if args.playlist:
            url = oldest_segment(url, args.insecure, args.timeout)
        targets.append(Target(name, url, args.insecure, args.timeout))

    for _ in range(args.warmup):
        for t in targets:
            t.once()
    for t in targets:
        t.samples.clear()
    for _ in range(args.count):
        for t in targets:
            t.once()

    base = targets[0]
    print(f"{'target':<10} {'n':>5} {'bytes':>9} {'p50':>8} {'p90':>8} {'p99':>8} {'max':>8} {'d_p50':>8} {'d_p90':>8}  status")
    worst = 0.0
    for t in targets:
        p50, p90 = statistics.median(t.samples), pct(t.samples, 90)
        d50 = p50 - statistics.median(base.samples)
        d90 = p90 - pct(base.samples, 90)
        if t is not base:
            worst = max(worst, d50)
        print(
            f"{t.name:<10} {len(t.samples):>5} {t.bytes:>9} {p50:8.3f} {p90:8.3f} {pct(t.samples, 99):8.3f} "
            f"{max(t.samples):8.3f} {d50:8.3f} {d90:8.3f}  {t.status} connect_ms={statistics.median(t.connects):.2f}"
        )
    if args.max_delta_ms is not None and worst > args.max_delta_ms:
        print(f"FAIL: p50 TTFB delta {worst:.3f} ms > {args.max_delta_ms} ms", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
