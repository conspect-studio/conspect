// SEC-01 (ADR-RT011): the realtime WebSocket URL must carry a short-lived
// single-use ticket, NEVER the durable bearer. Each (re)connect mints a FRESH
// ticket (a ticket is consumed on the first upgrade), and an unminted connect
// falls back to a bare URL (accepted only when the control plane has auth
// disabled).
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { RealtimeConnection } from "./connection";

/** A controllable WebSocket double capturing the connect URL. */
class FakeWebSocket {
  static instances: FakeWebSocket[] = [];
  onopen: (() => void) | null = null;
  onmessage: ((event: MessageEvent<unknown>) => void) | null = null;
  onerror: (() => void) | null = null;
  onclose: (() => void) | null = null;
  readonly url: string;

  constructor(url: string) {
    this.url = url;
    FakeWebSocket.instances.push(this);
  }

  close(): void {
    // Closed by the connection on teardown; nothing to emit here.
  }
}

function noopHandlers() {
  return {
    onStatus: (): void => {
      // ignored: these tests assert on the connect URL, not status/envelopes
    },
    onEnvelope: (): void => {
      // ignored
    },
  };
}

describe("RealtimeConnection ticket auth (SEC-01)", () => {
  beforeEach(() => {
    FakeWebSocket.instances = [];
    vi.stubGlobal("WebSocket", FakeWebSocket);
  });

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("connects with ?ticket=, never a durable bearer / access_token", async () => {
    const connection = new RealtimeConnection(
      "wss://mv.local/api/v1/ws",
      noopHandlers(),
      () => Promise.resolve("ticket-abc"),
    );
    connection.start();
    await vi.waitFor(() => {
      expect(FakeWebSocket.instances.length).toBe(1);
    });

    const url = FakeWebSocket.instances[0]?.url ?? "";
    expect(url).toContain("ticket=ticket-abc");
    // The durable bearer must appear nowhere in the URL (the SEC-01 leak).
    expect(url).not.toContain("access_token");
    connection.stop();
  });

  it("mints a FRESH ticket on each reconnect (single-use safety)", async () => {
    const minted: string[] = [];
    let n = 0;
    const connection = new RealtimeConnection(
      "wss://mv.local/api/v1/ws",
      noopHandlers(),
      () => {
        n += 1;
        const ticket = `ticket-${String(n)}`;
        minted.push(ticket);
        return Promise.resolve(ticket);
      },
    );
    connection.start();
    await vi.waitFor(() => {
      expect(FakeWebSocket.instances.length).toBe(1);
    });

    // The server closes the socket → the connection reconnects (backoff).
    FakeWebSocket.instances[0]?.onclose?.();
    await vi.waitFor(
      () => {
        expect(FakeWebSocket.instances.length).toBe(2);
      },
      { timeout: 2000 },
    );

    expect(FakeWebSocket.instances[0]?.url).toContain("ticket-1");
    expect(FakeWebSocket.instances[1]?.url).toContain("ticket-2");
    // Two distinct tickets were minted — the first is never reused.
    expect(minted).toEqual(["ticket-1", "ticket-2"]);
    connection.stop();
  });

  it("falls back to a bare connect when no ticket can be minted (auth disabled)", async () => {
    const connection = new RealtimeConnection(
      "wss://mv.local/api/v1/ws",
      noopHandlers(),
      () => Promise.resolve(undefined),
    );
    connection.start();
    await vi.waitFor(() => {
      expect(FakeWebSocket.instances.length).toBe(1);
    });

    const url = FakeWebSocket.instances[0]?.url ?? "";
    expect(url).toBe("wss://mv.local/api/v1/ws");
    expect(url).not.toContain("ticket");
    connection.stop();
  });
});
