// Native-WebSocket lifecycle for the realtime stream (docs/api/realtime.md §6).
//
// This owns ONLY transport concerns: connect, parse envelopes, track the resume
// cursor (`seq`), and reconnect with exponential backoff + full jitter. It never
// blocks: callbacks are invoked synchronously per frame and must be cheap. The
// engine is isolated (invariant #10) — a stalled UI can only lose its own
// frames, never back-pressure the engine.

import { parseEnvelope } from "./envelope";
import type { Envelope } from "./envelope";

/** The coarse connection state surfaced to the UI. */
export type RealtimeStatus =
  | "connecting"
  | "open"
  | "reconnecting"
  | "closed";

/** Callbacks the connection drives. All must be cheap + non-blocking. */
export interface RealtimeHandlers {
  /** A connection-status transition. */
  readonly onStatus: (status: RealtimeStatus) => void;
  /** A well-formed envelope arrived. Malformed frames are dropped silently. */
  readonly onEnvelope: (envelope: Envelope) => void;
  /** A sequence gap was observed (resume/re-snapshot territory). */
  readonly onGap?: (expected: number, received: number) => void;
}

/** Backoff parameters (base 0.5 s, ×2, cap 15 s — realtime.md §6). */
const BACKOFF_BASE_MS = 500;
const BACKOFF_CAP_MS = 15_000;

function backoffDelay(attempt: number): number {
  const exponential = Math.min(
    BACKOFF_CAP_MS,
    BACKOFF_BASE_MS * 2 ** attempt,
  );
  // Full jitter: pick uniformly in [0, exponential].
  return Math.random() * exponential;
}

/**
 * A resilient, self-reconnecting WebSocket client for `/api/v1/ws`. Construct,
 * call {@link start}, and {@link stop} on teardown. Not React-aware.
 */
export class RealtimeConnection {
  readonly #url: string;
  readonly #handlers: RealtimeHandlers;
  #socket: WebSocket | null = null;
  #attempt = 0;
  #lastSeq = 0;
  #stopped = false;
  #reconnectTimer: ReturnType<typeof setTimeout> | null = null;

  constructor(url: string, handlers: RealtimeHandlers) {
    this.#url = url;
    this.#handlers = handlers;
  }

  /** The last seen per-connection sequence cursor (for `$resume`). */
  get lastSeq(): number {
    return this.#lastSeq;
  }

  /** Begin connecting (idempotent while running). */
  start(): void {
    this.#stopped = false;
    this.#open();
  }

  /** Permanently stop and close the socket. Safe to call multiple times. */
  stop(): void {
    this.#stopped = true;
    if (this.#reconnectTimer !== null) {
      clearTimeout(this.#reconnectTimer);
      this.#reconnectTimer = null;
    }
    const socket = this.#socket;
    this.#socket = null;
    if (socket !== null) {
      // Detach handlers before closing to avoid a reconnect on our own close.
      socket.onopen = null;
      socket.onmessage = null;
      socket.onerror = null;
      socket.onclose = null;
      socket.close(1000);
    }
  }

  #resumeUrl(): string {
    if (this.#lastSeq <= 0) {
      return this.#url;
    }
    const separator = this.#url.includes("?") ? "&" : "?";
    return `${this.#url}${separator}last_seq=${String(this.#lastSeq)}`;
  }

  #open(): void {
    if (this.#stopped) {
      return;
    }
    this.#handlers.onStatus(this.#attempt === 0 ? "connecting" : "reconnecting");
    let socket: WebSocket;
    try {
      socket = new WebSocket(this.#resumeUrl());
    } catch {
      this.#scheduleReconnect();
      return;
    }
    this.#socket = socket;

    socket.onopen = (): void => {
      this.#attempt = 0;
      this.#handlers.onStatus("open");
    };

    socket.onmessage = (event: MessageEvent<unknown>): void => {
      if (typeof event.data !== "string") {
        // Binary meter frames (subprotocol multiview.bin.v1) are not negotiated
        // here; ignore non-text frames defensively.
        return;
      }
      const envelope = parseEnvelope(event.data);
      if (envelope === undefined) {
        return;
      }
      // Reject an unknown envelope major rather than misinterpreting it.
      if (envelope.v !== 1) {
        return;
      }
      if (envelope.seq > 0) {
        const expected = this.#lastSeq + 1;
        if (this.#lastSeq > 0 && envelope.seq > expected) {
          this.#handlers.onGap?.(expected, envelope.seq);
        }
        this.#lastSeq = envelope.seq;
      }
      this.#handlers.onEnvelope(envelope);
    };

    socket.onerror = (): void => {
      // The close handler drives reconnection; errors are advisory.
    };

    socket.onclose = (): void => {
      this.#socket = null;
      if (this.#stopped) {
        this.#handlers.onStatus("closed");
        return;
      }
      this.#scheduleReconnect();
    };
  }

  #scheduleReconnect(): void {
    if (this.#stopped) {
      return;
    }
    this.#handlers.onStatus("reconnecting");
    const delay = backoffDelay(this.#attempt);
    this.#attempt += 1;
    this.#reconnectTimer = setTimeout((): void => {
      this.#reconnectTimer = null;
      this.#open();
    }, delay);
  }
}
