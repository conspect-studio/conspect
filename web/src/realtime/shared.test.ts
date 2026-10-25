import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { subscribeRealtime, subscribeRealtimeAll } from "./shared";
import { correlateOperation } from "./operations";

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
    this.onmessage?.({ data: JSON.stringify({ v: 1, ts: 1, ...frame }) } as MessageEvent<unknown>);
  }
}

async function socket(index = 0): Promise<FakeWebSocket> {
  await vi.waitFor(() => {
    expect(FakeWebSocket.instances.length).toBeGreaterThan(index);
  });
  return FakeWebSocket.instances[index] as FakeWebSocket;
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
    const offA = subscribeRealtime("system", a);
    const ws = await socket();
    offA();
    const offB = subscribeRealtime("system", b);
    ws.emit({ t: "x", topic: "system", seq: 1, data: {} });
    expect(b).toHaveBeenCalledTimes(1);
    offB();
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

  it("resolves a pending operation from an envelope whose corr matches", async () => {
    const off = subscribeRealtime("system", vi.fn());
    const ws = await socket();
    const outcome = correlateOperation({ operation_id: "op-1", kind: "take" });
    ws.emit({ t: "operation.completed", topic: "system", seq: 1, corr: "op-1", data: {} });
    await expect(outcome).resolves.toMatchObject({ operationId: "op-1", replayed: false });
    off();
  });

  it("ignores envelopes with an unknown corr", async () => {
    const off = subscribeRealtime("system", vi.fn());
    const ws = await socket();
    const outcome = correlateOperation({ operation_id: "op-2", kind: "take" }, 50);
    ws.emit({ t: "x", topic: "system", seq: 1, corr: "other", data: {} });
    await expect(outcome).rejects.toThrow(/timed out/);
    off();
  });

  it("resolves a kind:replay acceptance immediately as replayed", async () => {
    await expect(correlateOperation({ operation_id: "op-3", kind: "replay" })).resolves.toMatchObject({
      operationId: "op-3",
      replayed: true,
    });
  });
});
