# ADR-RT002: Single versioned message envelope with discriminated payloads

- **Status:** Proposed
- **Area:** Realtime API
- **Date:** 2026-06-02
- **Source brief:** [realtime-api.md](../research/realtime-api.md)

## Decision

Every message both directions uses one versioned envelope {v,t,topic,id,seq,ts,corr,data}; control frames reuse it on topic "$control". Implement as a Rust serde internally-tagged enum (#[serde(tag="t",content="data")]) in a shared multiview-events crate deriving serde + schemars, rendered as a JSON-Schema oneOf with a const discriminator. Additive changes bump minor (clients ignore unknown t/fields); breaking changes bump v major; hello.server_v advertises supported majors and the negotiated subprotocol multiview.v1 makes the wire major explicit. High-rate meters MAY use a binary body under subprotocol multiview.bin.v1 with the same envelope shape.

## Rationale

A uniform envelope gives exactly one parse/validate/route path for WS+SSE, makes the AsyncAPI spec and typed clients trivial, and makes the resume cursor (seq) and correlation (corr) universal. schemars-derived schemas from the same Rust types are the single source of truth shared with OpenAPI, so docs cannot drift from the wire. The discriminated union maps to a perfect TypeScript switch for exhaustive client handling.

## Alternatives considered

Per-message-type bespoke frames (N parse paths, hard to version/route); protobuf/flatbuffers everywhere (better perf but worse browser DX and a separate schema toolchain); no version field (cannot evolve safely); hand-written JSON Schemas (guaranteed drift).

## Consequences

All producers must populate envelope metadata consistently. The binary meter fast-path means the documented JSON schema describes a decoded shape while the wire is binary — must be documented explicitly (contentType) or clients misparse. A shared multiview-events crate becomes a hard dependency of engine, REST, WS, and codegen.
