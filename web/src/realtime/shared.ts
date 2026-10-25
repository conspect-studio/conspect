// One module-level realtime connection shared by every hook (SW-19, #45).
//
// Hooks register a topic-keyed listener; the first subscriber opens the single
// `RealtimeConnection`, the last one closes it. Resume (`last_seq`, ADR-RT002/003)
// lives in the connection; a sequence gap is fanned out to every listener's
// `onGap` so each one refetches/rebuilds its own state. Every envelope is also
// offered to the operation-correlation layer (`corr` -> pending operation id).
import { RealtimeConnection } from "./connection";
import type { RealtimeStatus } from "./connection";
import type { Envelope } from "./envelope";
import { resolveOperationEnvelope } from "./operations";
import { mintWsTicket, realtimeWsBaseUrl } from "./ticket";

export type EnvelopeListener = (envelope: Envelope) => void;
export type StatusListener = (status: RealtimeStatus) => void;
export type GapListener = (expected: number, received: number) => void;

interface Subscriber {
  readonly topic: string;
  readonly onEnvelope: EnvelopeListener;
  readonly onStatus?: StatusListener | undefined;
  readonly onGap?: GapListener | undefined;
}

/** Topic wildcard: receives every envelope. */
const ALL_TOPICS = "*";

const subscribers = new Set<Subscriber>();
let connection: RealtimeConnection | undefined;
let status: RealtimeStatus = "connecting";
/** Latest sample of the `system` topic, replayed to late subscribers: a
 *  periodic metrics stream would otherwise leave a freshly mounted consumer
 *  empty until the next tick (the old per-hook socket got a fresh stream). */
const STICKY_TOPIC = "system";
let sticky: Envelope | undefined;

function dispatchEnvelope(envelope: Envelope): void {
  resolveOperationEnvelope(envelope);
  if (envelope.topic === STICKY_TOPIC) {
    sticky = envelope;
  }
  for (const sub of [...subscribers]) {
    if (sub.topic === envelope.topic || sub.topic === ALL_TOPICS) {
      sub.onEnvelope(envelope);
    }
  }
}

function start(): void {
  status = "connecting";
  connection = new RealtimeConnection(
    realtimeWsBaseUrl(),
    {
      onStatus: (next) => {
        status = next;
        for (const sub of [...subscribers]) {
          sub.onStatus?.(next);
        }
      },
      onEnvelope: dispatchEnvelope,
      onGap: (expected, received) => {
        for (const sub of [...subscribers]) {
          sub.onGap?.(expected, received);
        }
      },
    },
    mintWsTicket,
  );
  connection.start();
}

/**
 * Subscribe to one topic (or `"*"`) on the shared socket. Returns the
 * unsubscribe function; the socket closes when the last subscriber leaves.
 */
export function subscribeRealtime(
  topic: string,
  onEnvelope: EnvelopeListener,
  onStatus?: StatusListener,
  onGap?: GapListener,
): () => void {
  const sub: Subscriber = { topic, onEnvelope, onStatus, onGap };
  subscribers.add(sub);
  if (connection === undefined) {
    start();
  } else {
    onStatus?.(status);
    if (topic === STICKY_TOPIC && sticky !== undefined) {
      onEnvelope(sticky);
    }
  }
  return (): void => {
    if (!subscribers.delete(sub)) {
      return;
    }
    if (subscribers.size === 0) {
      connection?.stop();
      connection = undefined;
      sticky = undefined;
    }
  };
}

/** Subscribe to every topic. */
export function subscribeRealtimeAll(
  onEnvelope: EnvelopeListener,
  onStatus?: StatusListener,
  onGap?: GapListener,
): () => void {
  return subscribeRealtime(ALL_TOPICS, onEnvelope, onStatus, onGap);
}
