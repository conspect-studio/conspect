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

  it("resolves BOTH of two concurrent registrations for the same operation id", async () => {
    const a = correlateOperation({ operation_id: "dup-1", kind: "take" });
    const b = correlateOperation({ operation_id: "dup-1", kind: "replay" });
    expect(resolveOperationEnvelope(env("dup-1"))).toBe(true);
    await expect(a).resolves.toMatchObject({ operationId: "dup-1", replayed: false });
    await expect(b).resolves.toMatchObject({ operationId: "dup-1", replayed: true });
  });

  it("a registration timing out does not starve a sibling registration", async () => {
    const short = correlateOperation({ operation_id: "dup-2", kind: "take" }, 50);
    const shortAssertion = expect(short).rejects.toThrow(/timed out/);
    const long = correlateOperation({ operation_id: "dup-2", kind: "replay" }, 10_000);
    await vi.advanceTimersByTimeAsync(60);
    await shortAssertion;
    resolveOperationEnvelope(env("dup-2"));
    await expect(long).resolves.toMatchObject({ operationId: "dup-2" });
  });

  it("a replay AFTER completion resolves from the cache, repeatedly", async () => {
    const first = correlateOperation({ operation_id: "dup-3", kind: "take" });
    resolveOperationEnvelope(env("dup-3"));
    await first;
    await expect(correlateOperation({ operation_id: "dup-3", kind: "replay" })).resolves.toMatchObject({
      operationId: "dup-3",
      replayed: true,
    });
    await expect(correlateOperation({ operation_id: "dup-3", kind: "replay" })).resolves.toMatchObject({
      operationId: "dup-3",
    });
  });

  it("a completed outcome still expires after the TTL", async () => {
    const first = correlateOperation({ operation_id: "dup-4", kind: "take" });
    resolveOperationEnvelope(env("dup-4"));
    await first;
    await vi.advanceTimersByTimeAsync(10 * 60_000);
    const late = correlateOperation({ operation_id: "dup-4", kind: "replay" }, 50);
    const assertion = expect(late).rejects.toThrow(/timed out/);
    await vi.advanceTimersByTimeAsync(60);
    await assertion;
  });
});
