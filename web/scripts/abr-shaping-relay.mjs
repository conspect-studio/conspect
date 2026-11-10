// Userspace QUIC path shaping for the ABR acceptance probe.
//
// The kernel netem path is unavailable in this container (`tc qdisc add` ->
// RTNETLINK Operation not permitted, no CAP_NET_ADMIN), so capacity is imposed
// on the real QUIC path with a token bucket over forwarded UDP datagrams
// instead. Each consumer gets its own relay, so a browser's WebTransport
// session runs end-to-end through a shaper: QUIC's own congestion control sees
// the reduced capacity and drops, and the media arrives late or not at all
// exactly as it would over a constrained link. This is shaping of the real
// transport, not a mock rendition pick.
//
// Fidelity limits, stated plainly: token-bucket + tail-drop, no queue
// discipline (FQ-CoDel), no reordering, one client address per relay. Burst is
// 150ms of the configured rate, minimum 4KiB.
import dgram from 'node:dgram';

const nowMs = () => Number(process.hrtime.bigint() / 1000n) / 1000;

/**
 * One bidirectional shaped UDP path to a QUIC server.
 *
 * `downBps` constrains server -> browser (the media direction). `upBps`
 * constrains browser -> server and defaults to an asymmetric 1.5 Mbps so
 * acknowledgements keep flowing when the downlink is throttled hard.
 */
export function startShapingRelay({
  listenPort,
  targetPort,
  targetAddress = '::1',
  bindAddress = '::1',
  downBps,
  upBps = 1_500_000,
  // Queue depth is a property of the link, not a constant: a deep buffer on a
  // 150 kbps path is 5 seconds of standing latency, which would let the old
  // rendition keep arriving long after the selector moved off it and would
  // flatter every measurement taken during the transition. Budget 250ms of the
  // configured rate, floor 8KiB.
  maxQueueBytes = Math.max(16_384, Math.round((downBps / 8) * 0.25)),
}) {
  if (!(downBps > 0) || !(upBps > 0)) throw new Error('rates must be positive');

  const socket = dgram.createSocket({ type: 'udp6', reuseAddr: false });
  socket.bind(listenPort, bindAddress);

  const server = { address: targetAddress, port: targetPort };
  /**
   * The browser's source address per session. A compat session refresh arrives
   * from a new port, which is expected: this relay follows it and records the
   * handover, so evidence shows one session ending before the next begins rather
   * than two overlapping.
   */
  const clients = [];
  let overlaps = 0;
  const follow = (address, port) => {
    const nowMs_ = Math.round(nowMs());
    const current = clients.at(-1);
    if (current && current.address === address && current.port === port) {
      current.lastMs = nowMs_;
      return current;
    }
    // A datagram from an address that is no longer current means the previous
    // session kept sending after its replacement opened: two leases at once.
    const stale = clients.find(
      (entry) => entry.address === address && entry.port === port && entry !== current,
    );
    if (stale) {
      stale.staleSends = (stale.staleSends ?? 0) + 1;
      overlaps += 1;
      stale.lastMs = nowMs_;
      return stale;
    }
    if (current) {
      current.closedMs = nowMs_;
      current.replacedAtMs = nowMs_;
    }
    const entry = { address, port, firstMs: nowMs_, lastMs: nowMs_ };
    clients.push(entry);
    return entry;
  };
  // A dual-stack socket reports IPv4 peers as ::ffff:a.b.c.d, so compare the
  // mapped form away before deciding which side a datagram came from.
  const sameAddress = (a, b) =>
    typeof a === 'string' && typeof b === 'string'
      ? a.replace(/^::ffff:/, '') === b.replace(/^::ffff:/, '')
      : false;
  let client;
  const relays = {
    down: { queue: [], bytes: 0, dropped: 0, datagrams: 0, rate: downBps },
    up: { queue: [], bytes: 0, dropped: 0, datagrams: 0, rate: upBps },
  };
  const buckets = {
    down: { tokens: 0, last: nowMs(), burst: Math.max(4096, (downBps / 8) * 0.15) },
    up: { tokens: 0, last: nowMs(), burst: Math.max(4096, (upBps / 8) * 0.15) },
  };
  /** Bytes accepted per direction per interval, for evidence. */
  const samples = [];
  let sampleStart = nowMs();
  let sampleBytes = { down: 0, up: 0 };
  let closed = false;

  const refill = (name) => {
    const bucket = buckets[name];
    const t = nowMs();
    bucket.tokens = Math.min(bucket.burst, bucket.tokens + ((t - bucket.last) / 1000) * (relays[name].rate / 8));
    bucket.last = t;
  };

  const pump = (name) => {
    if (closed) return;
    refill(name);
    const relay = relays[name];
    while (relay.queue.length > 0) {
      const packet = relay.queue[0];
      if (buckets[name].tokens < packet.length) break;
      buckets[name].tokens -= packet.length;
      relay.queue.shift();
      if (name === 'down') {
        if (client) socket.send(packet, client.port, client.address);
      } else {
        socket.send(packet, server.port, server.address);
      }
      relay.bytes += packet.length;
      sampleBytes[name] += packet.length;
    }
  };

  const enqueue = (name, packet) => {
    const relay = relays[name];
    relay.datagrams += 1;
    refill(name);
    if (buckets[name].tokens >= packet.length) {
      buckets[name].tokens -= packet.length;
      relay.bytes += packet.length;
      sampleBytes[name] += packet.length;
      if (name === 'down') {
        if (client) socket.send(packet, client.port, client.address);
      } else {
        socket.send(packet, server.port, server.address);
      }
      return;
    }
    // Tail drop: a queue deeper than the link is latency, not capacity.
    const queued = relay.queue.reduce((sum, p) => sum + p.length, 0);
    if (queued + packet.length > maxQueueBytes) {
      relay.dropped += packet.length;
      return;
    }
    relay.queue.push(packet);
  };

  socket.on('message', (message, info) => {
    const packet = Buffer.from(message);
    if (sameAddress(info.address, server.address) && info.port === server.port) {
      enqueue('down', packet);
      return;
    }
    const entry = follow(info.address, info.port);
    client = { address: entry.address, port: entry.port };
    enqueue('up', packet);
  });
  socket.on('error', () => { closed = true; });

  const timer = setInterval(() => {
    pump('up');
    pump('down');
    const t = nowMs();
    if (t - sampleStart >= 1000) {
      samples.push({
        tMs: Math.round(t - sampleStart + (samples.length * 1000)),
        downBps: Math.round((sampleBytes.down * 8) / ((t - sampleStart) / 1000)),
        upBps: Math.round((sampleBytes.up * 8) / ((t - sampleStart) / 1000)),
        configuredDownBps: downBps,
        downDroppedBytes: relays.down.dropped,
        downQueueBytes: relays.down.queue.reduce((sum, p) => sum + p.length, 0),
      });
      sampleStart = t;
      sampleBytes = { down: 0, up: 0 };
    }
  }, 5);
  timer.unref();

  return {
    get port() {
      return socket.address().port;
    },
    /** Change the downlink capacity mid-run (the probe's perturbation). */
    setDownBps(bps) {
      if (!(bps > 0)) throw new Error('rate must be positive');
      refill('down');
      downBps = bps;
      relays.down.rate = bps;
      buckets.down.burst = Math.max(4096, (bps / 8) * 0.15);
      maxQueueBytes = Math.max(16_384, Math.round((bps / 8) * 0.25));
    },
    /** One entry per second of accepted bytes in each direction. */
    evidence() {
      return {
        configuredDownBps: downBps,
        configuredUpBps: upBps,
        down: { bytes: relays.down.bytes, datagrams: relays.down.datagrams, droppedBytes: relays.down.dropped },
        up: { bytes: relays.up.bytes, datagrams: relays.up.datagrams, droppedBytes: relays.up.dropped },
        samples,
        clientSeen: Boolean(client),
        clients,
        clientHandovers: Math.max(0, clients.length - 1),
        sessionOverlaps: overlaps,
      };
    },
    close() {
      closed = true;
      clearInterval(timer);
      return new Promise((resolve) => socket.close(resolve));
    },
  };
}
