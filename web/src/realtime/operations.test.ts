import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { correlateOperation, resolveOperationEnvelope } from "./operations";
import type { Envelope } from "./envelope";

function env(corr: string, t = "operation.completed"): Envelope {
  return { v: 1, t, topic: "system", seq: 1, ts: 1, corr, data: {} };
}

describe("operation correlation", () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  it("resolves a registered operation from the matching corr", async () => {
    const p = correlateOperation({ operation_id: "a1", kind: "take" });
    expect(resolveOperationEnvelope(env("a1"))).toBe(true);
    await expect(p).resolves.toMatchObject({ operationId: "a1", replayed: false });
  });

  it("rejects on timeout", async () => {
    const p = correlateOperation({ operation_id: "a2", kind: "take" }, 100);
    const assertion = expect(p).rejects.toThrow(/timed out/);
    await vi.advanceTimersByTimeAsync(101);
    await assertion;
  });

  it("keeps an outcome that arrives BEFORE registration", async () => {
    resolveOperationEnvelope(env("early-1"));
    const p = correlateOperation({ operation_id: "early-1", kind: "take" });
    await expect(p).resolves.toMatchObject({ operationId: "early-1" });
  });

  it("expires cached early outcomes after the TTL and bounds the cache", async () => {
    resolveOperationEnvelope(env("old-1"));
    await vi.advanceTimersByTimeAsync(10 * 60_000);
    const p = correlateOperation({ operation_id: "old-1", kind: "take" }, 50);
    const assertion = expect(p).rejects.toThrow(/timed out/);
    await vi.advanceTimersByTimeAsync(60);
    await assertion;

    for (let i = 0; i < 1000; i += 1) resolveOperationEnvelope(env(`flood-${String(i)}`));
    const evicted = correlateOperation({ operation_id: "flood-0", kind: "take" }, 50);
    const evictedAssertion = expect(evicted).rejects.toThrow(/timed out/);
    await vi.advanceTimersByTimeAsync(60);
    await evictedAssertion;
    await expect(correlateOperation({ operation_id: "flood-999", kind: "take" })).resolves.toBeDefined();
  });

  it("does NOT treat a replay acceptance as completion; awaits the matching corr", async () => {
    let settled = false;
    const p = correlateOperation({ operation_id: "r1", kind: "replay" });
    void p.then(() => {
      settled = true;
    });
    await vi.advanceTimersByTimeAsync(10);
    expect(settled).toBe(false);
    resolveOperationEnvelope(env("r1"));
    await expect(p).resolves.toMatchObject({ operationId: "r1", replayed: true });
  });

  it("resolves a replay acceptance from a cached authoritative outcome", async () => {
    resolveOperationEnvelope(env("r2"));
    await expect(correlateOperation({ operation_id: "r2", kind: "replay" })).resolves.toMatchObject({
      operationId: "r2",
      replayed: true,
    });
  });
});
