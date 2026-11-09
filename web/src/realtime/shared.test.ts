import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { subscribeRealtime, subscribeRealtimeAll } from "./shared";

vi.mock("./ticket", () => ({
  realtimeWsBaseUrl: () => "ws://test/api/v1/ws",
  mintWsTicket: () => Promise.resolve("t"),
}));

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
    // no-op
  }
  emit(frame: Record<string, unknown>): void {
    const event = new MessageEvent<unknown>("message", {
      data: JSON.stringify({ v: 1, ts: 1, ...frame }),
    });
    this.onmessage?.(event);
  }
}

async function socket(index = 0): Promise<FakeWebSocket> {
  await vi.waitFor(() => {
    expect(FakeWebSocket.instances.length).toBeGreaterThan(index);
  });
  return FakeWebSocket.instances[index]!;
}

describe("shared realtime service", () => {
  beforeEach(() => {
    FakeWebSocket.instances = [];
    vi.stubGlobal("WebSocket", FakeWebSocket);
  });
  afterEach(() => {
    vi.unstubAllGlobals();
    vi.useRealTimers();
  });

  it("opens ONE socket for many topic listeners and dispatches by topic", async () => {
    const system = vi.fn();
    const alerts = vi.fn();
    const all = vi.fn();
    const offs = [
      subscribeRealtime("system", system),
      subscribeRealtime("alerts", alerts),
      subscribeRealtimeAll(all),
    ];
    const ws = await socket();
    expect(FakeWebSocket.instances).toHaveLength(1);
    ws.emit({ t: "system.metrics", topic: "system", seq: 1, data: {} });
    expect(system).toHaveBeenCalledTimes(1);
    expect(alerts).not.toHaveBeenCalled();
    expect(all).toHaveBeenCalledTimes(1);
    offs.forEach((off) => { off(); });
  });

  it("replays the current status to a late subscriber", async () => {
    const off1 = subscribeRealtime("system", vi.fn());
    const ws = await socket();
    ws.onopen?.();
    const status = vi.fn();
    const off2 = subscribeRealtime("alerts", vi.fn(), status);
    expect(status).toHaveBeenCalledWith("open");
    off1();
    off2();
  });

  it("keeps a resubscribed topic listener after an earlier one unsubscribes", async () => {
    const a = vi.fn();
    const b = vi.fn();
    const keeper = subscribeRealtime("alerts", vi.fn());
    const offA = subscribeRealtime("system", a);
    const ws = await socket();
    offA();
    const offB = subscribeRealtime("system", b);
    ws.emit({ t: "x", topic: "system", seq: 1, data: {} });
    expect(b).toHaveBeenCalledTimes(1);
    expect(a).not.toHaveBeenCalled();
    offB();
    keeper();
  });

  it("resumes with the last seq on reconnect and tells every listener to refetch on a gap", async () => {
    const gapA = vi.fn();
    const gapB = vi.fn();
    const offA = subscribeRealtime("system", vi.fn(), undefined, gapA);
    const offB = subscribeRealtime("alerts", vi.fn(), undefined, gapB);
    const ws = await socket();
    ws.emit({ t: "a", topic: "system", seq: 5, data: {} });
    ws.emit({ t: "a", topic: "system", seq: 9, data: {} });
    expect(gapA).toHaveBeenCalledTimes(1);
    expect(gapB).toHaveBeenCalledTimes(1);
    ws.onclose?.();
    const next = await vi.waitFor(async () => socket(1), { timeout: 3000 });
    expect(next.url).toContain("last_seq=9");
    offA();
    offB();
  });

  it("delivers a corr frame to subscribers without an independent completion matcher", async () => {
    const listener = vi.fn();
    const off = subscribeRealtime("switcher", listener);
    const ws = await socket();
    ws.emit({ t: "media.player_state", topic: "switcher", id: "player-1", seq: 1, corr: "op-1", data: { state: { kind: "loading" } } });
    expect(listener).toHaveBeenCalledTimes(1);
    expect(listener.mock.calls[0]?.[0]).toMatchObject({ t: "media.player_state", corr: "op-1" });
    off();
  });
});
