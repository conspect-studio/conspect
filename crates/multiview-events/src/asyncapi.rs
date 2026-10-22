//! `AsyncAPI` 3.0 document generation from the multiview-events wire types.
//!
//! Per ADR-RT006 the realtime event API is documented in `AsyncAPI` 3.0, derived
//! from the same serde Rust types that carry the wire contract. The `asyncapi-rust`
//! crate (v0.2, Nov 2025) is young and **lacks WebSocket bindings**, so this module
//! uses the hand-assembly approach described in ADR-RT006's Consequences section:
//! build the document as a `serde_json::Value` tree from the canonical type
//! definitions in [`crate::event`], [`crate::envelope`], [`crate::topic`], and
//! [`crate::subscription`], then inject WS-specific binding blocks that the
//! generator would omit.
//!
//! The document structure follows the `AsyncAPI` 3.0 spec:
//! <https://www.asyncapi.com/docs/reference/specification/v3.0.0>
//!
//! Channels:
//! - `ws`  → `/api/v1/ws`  (WebSocket, bidirectional)
//! - `sse` → `/api/v1/events` (HTTP SSE, server→client only)
//!
//! Messages are the envelope-typed frames; the envelope `t` discriminator maps
//! to a JSON-Schema `oneOf` within each message's payload schema.
use serde_json::{json, Value};

/// Generate the complete `AsyncAPI` 3.0 document for the Multiview realtime API
/// as a pretty-printed JSON string.
///
/// The output is **deterministic**: calling this function twice with no
/// intervening mutation produces identical strings. This property is required by
/// the CI drift-gate (ADR-RT006 Decision: CI regenerates and fails on any diff).
///
/// # Structure
///
/// ```text
/// asyncapi: "3.0.0"
/// info: { title, version, description }
/// servers: { ws-server, sse-server }
/// channels: { ws, sse }
/// operations: { subscribe-ws, subscribe-sse }
/// components/messages: { Envelope, TileState, AudioMeter, … }
/// components/schemas:  { … all payload schemas … }
/// ```
#[must_use]
pub fn generate_asyncapi_document() -> String {
    let doc = build_document();
    // to_string_pretty produces consistent output: sorted keys within each
    // serde_json::Map are insertion-ordered (serde_json preserves insertion
    // order by default), so the output is deterministic as long as the
    // construction order below is stable.
    match serde_json::to_string_pretty(&doc) {
        Ok(mut s) => {
            // Append a trailing newline for POSIX-cleanliness and diff friendliness.
            s.push('\n');
            s
        }
        // serde_json::to_string_pretty only fails on non-finite floats or
        // recursive values; neither can occur with the Value tree we build here.
        Err(_) => String::new(),
    }
}

/// Assemble the `AsyncAPI` 3.0 [`Value`] document.
fn build_document() -> Value {
    json!({
        "asyncapi": "3.0.0",
        "info": build_info(),
        "servers": build_servers(),
        "channels": build_channels(),
        "operations": build_operations(),
        "components": {
            // AsyncAPI 3.0 forbids a top-level `messages` field: reusable message
            // definitions live under `components.messages`. Channels reference them
            // via `#/components/messages/<name>` and operations reference the
            // channel's own message alias (see `build_operations`).
            "messages": build_messages(),
            "schemas": build_schemas()
        }
    })
}

fn build_info() -> Value {
    json!({
        "title": "Multiview Realtime API",
        "version": "1.0.0",
        "description": concat!(
            "WebSocket and SSE realtime event stream for the Multiview engine. ",
            "Every message uses a single versioned envelope (ADR-RT002). ",
            "Topics: tiles, inputs, outputs, audio.meters, alerts, alarms, tally, ",
            "layout, config, logs, jobs, preview, system, capabilities, devices, switcher. ",
            "The WebSocket endpoint (/api/v1/ws) is bidirectional (events + subscription ",
            "control frames); the SSE endpoint (/api/v1/events) is server-to-client only.",
        ),
        "license": {
            "name": "Multiview Source-Available Non-Commercial License",
            "url": "https://github.com/aperim/multiview/blob/main/LICENSE"
        },
        "contact": {
            "name": "The Multiview Authors",
            "url": "https://github.com/aperim/multiview"
        },
        "externalDocs": {
            // AsyncAPI 3.0 requires `format: uri` — an absolute URL, not a repo-relative path.
            "description": "Realtime API reference",
            "url": "https://github.com/aperim/multiview/blob/main/docs/api/realtime.md"
        }
    })
}

fn build_servers() -> Value {
    json!({
        "ws-server": {
            "host": "{host}",
            "pathname": "/api/v1/ws",
            "protocol": "ws",
            "description": "WebSocket endpoint (bidirectional). Primary transport.",
            "variables": {
                "host": {
                    "description": "Hostname and optional port of the Multiview daemon. IPv6-first: the default is the IPv6 loopback `[::1]`; override with your host.",
                    "default": "[::1]:8080"
                }
            }
        },
        "sse-server": {
            "host": "{host}",
            "pathname": "/api/v1/events",
            "protocol": "http",
            "description": "Server-Sent Events endpoint (server→client only). Fallback transport.",
            "variables": {
                "host": {
                    "description": "Hostname and optional port of the Multiview daemon. IPv6-first: the default is the IPv6 loopback `[::1]`; override with your host.",
                    "default": "[::1]:8080"
                }
            }
        }
    })
}

/// Build the two channel definitions.
///
/// ADR-RT006 Consequences note that `asyncapi-rust` v0.2 lacks WS bindings;
/// the `bindings.ws` block on the `ws` channel is the **post-processed injection**
/// described there — it is hand-assembled here alongside the rest of the document
/// rather than in a separate post-process step, since this module IS the generator.
fn build_channels() -> Value {
    json!({
        "ws": {
            "address": "/api/v1/ws",
            "title": "WebSocket realtime channel",
            "description": concat!(
                "Bidirectional WebSocket channel. The server sends event envelopes; ",
                "the client sends subscription control frames ($subscribe, $unsubscribe, ",
                "$set_rate, $resume, $pong). Negotiated subprotocol: `multiview.v1` ",
                "(or `multiview.bin.v1` for compact binary meter frames).",
            ),
            "servers": [{ "$ref": "#/servers/ws-server" }],
            "messages": {
                "EnvelopeMessage": { "$ref": "#/components/messages/Envelope" }
            },
            // WS binding injected here: asyncapi-rust v0.2 lacks bindings support.
            // Spec ref: https://github.com/asyncapi/bindings/blob/master/websockets
            "bindings": {
                "ws": {
                    "method": "GET",
                    "query": {
                        "type": "object",
                        "description": "Optional authentication token as a query parameter.",
                        "properties": {
                            "token": {
                                "type": "string",
                                "description": "Bearer token (alternative to Authorization header)."
                            }
                        }
                    },
                    "headers": {
                        "type": "object",
                        "properties": {
                            "Authorization": {
                                "type": "string",
                                "description": "Bearer token: `Bearer <jwt>`"
                            },
                            "Sec-WebSocket-Protocol": {
                                "type": "string",
                                "enum": ["multiview.v1", "multiview.bin.v1"],
                                "description": "Subprotocol selection; makes the envelope major explicit."
                            }
                        }
                    },
                    "bindingVersion": "0.1.0"
                }
            }
        },
        "sse": {
            "address": "/api/v1/events",
            "title": "SSE realtime channel (fallback)",
            "description": concat!(
                "Server-Sent Events fallback for proxies or environments that strip the ",
                "WebSocket Upgrade header. Server-to-client only: identical envelope shape, ",
                "no client subscription control frames or WHEP signaling.",
            ),
            "servers": [{ "$ref": "#/servers/sse-server" }],
            "messages": {
                "EnvelopeMessage": { "$ref": "#/components/messages/Envelope" }
            }
        }
    })
}

fn build_operations() -> Value {
    // AsyncAPI 3.0 (`asyncapi3-operation-messages-from-referred-channel`): an
    // operation's `messages` MUST reference the channel's OWN message aliases,
    // i.e. `#/channels/<channel>/messages/<alias>`, not the reusable
    // `#/components/messages/...` entries directly.
    json!({
        "subscribe-ws": {
            "action": "receive",
            "channel": { "$ref": "#/channels/ws" },
            "title": "Receive realtime events (WebSocket)",
            "description": concat!(
                "Receive all event types on the WebSocket channel after subscribing to ",
                "the desired topics via $subscribe control frames.",
            ),
            "messages": [{ "$ref": "#/channels/ws/messages/EnvelopeMessage" }]
        },
        "subscribe-sse": {
            "action": "receive",
            "channel": { "$ref": "#/channels/sse" },
            "title": "Receive realtime events (SSE)",
            "description": "Receive all event types on the SSE fallback channel.",
            "messages": [{ "$ref": "#/channels/sse/messages/EnvelopeMessage" }]
        }
    })
}

/// Build the `components.messages` block.
///
/// Each entry is a named message definition carrying a `payload` schema. The
/// `Envelope` message carries the full versioned envelope shape; the per-event
/// messages carry only their `data` payload schemas and are referenced from the
/// envelope's `oneOf` discriminator.
fn build_messages() -> Value {
    let mut map = serde_json::Map::new();
    // Core envelope message.
    map.extend(build_messages_envelope());
    // Data event messages.
    map.extend(build_messages_data_events());
    // Switcher realtime surface messages (ADR-RT008).
    map.extend(build_messages_switcher_events());
    // Devices realtime surface messages (ADR-RT007).
    map.extend(build_messages_device_events());
    // Control frame messages.
    map.extend(build_messages_control_frames());
    Value::Object(map)
}

/// Build the `Envelope` message definition.
fn build_messages_envelope() -> serde_json::Map<String, Value> {
    let mut map = serde_json::Map::new();
    map.insert("Envelope".to_owned(), json!({
        "name": "Envelope",
        "title": "Versioned realtime envelope",
        "summary": "The single wire frame used for all realtime messages (ADR-RT002).",
        "description": concat!(
            "Every message in both directions uses this envelope. The `t` field is the ",
            "discriminator selecting the `data` schema. The `seq` field is a per-connection ",
            "monotonic cursor; gaps indicate dropped frames and trigger resume/resync. ",
            "The `ts` field carries engine-monotonic nanoseconds (same clock family as ",
            "output PTS). `v` is the envelope schema major; clients MUST reject unknown majors.",
        ),
        "contentType": "application/json",
        "payload": envelope_schema()
    }));
    map
}

/// Build data-event message definitions (tile, audio, output, alert, …).
fn build_messages_data_events() -> serde_json::Map<String, Value> {
    let mut map = serde_json::Map::new();
    map.insert("TileState".to_owned(), json!({
        "name": "TileState",
        "title": "Tile state machine transition",
        "summary": "A tile transitioned between lifecycle states (LIVE/STALE/RECONNECTING/NO_SIGNAL).",
        "description": concat!(
            "Emitted on topic `tiles` whenever a tile's state machine transitions. ",
            "Invariant #2: Live → Stale → Reconnecting → NoSignal. The compositor ",
            "holds the last-good frame while STALE/RECONNECTING; renders a placeholder on NO_SIGNAL.",
        ),
        "contentType": "application/json",
        "payload": { "$ref": "#/components/schemas/TileState" }
    }));
    map.insert("TilesSnapshot".to_owned(), tiles_snapshot_message());
    map.insert("AudioMeter".to_owned(), json!({
        "name": "AudioMeter",
        "title": "High-rate audio meter sample",
        "summary": "Per-input/track peak/RMS/clip meter sample (numeric only, never audio data).",
        "description": concat!(
            "Emitted on topic `audio.meters`. This is the sole high-rate conflated topic; ",
            "frames are conflated/sampled at 10–30 Hz on the wire. A slow consumer can only ",
            "lose its own meter frames — the engine never back-pressures. The binary fast-path ",
            "(subprotocol `multiview.bin.v1`) carries the same fields in compact form; ",
            "this schema describes the decoded (JSON) shape.",
        ),
        "contentType": "application/json",
        "payload": { "$ref": "#/components/schemas/AudioMeter" }
    }));
    map.insert("AudioLoudness".to_owned(), audio_loudness_message());
    map.insert("SystemMetrics".to_owned(), system_metrics_message());
    map.insert("OutputStatus".to_owned(), json!({
        "name": "OutputStatus",
        "title": "Output sink status update",
        "summary": "An output sink changed run state (starting/running/migrating/error).",
        "description": "Emitted on topic `outputs`. Make-before-break migration emits `migrating` during the parallel-output window.",
        "contentType": "application/json",
        "payload": { "$ref": "#/components/schemas/OutputStatus" }
    }));
    map.insert("Alert".to_owned(), json!({
        "name": "Alert",
        "title": "Operator alert raised or cleared",
        "summary": "An alert was raised (alert.raised) or cleared (alert.cleared) on topic `alerts`.",
        "description": concat!(
            "The `key` field is a stable dedupe identifier for the condition; multiple ",
            "`alert.raised` frames with the same key coalesce rather than stacking.",
        ),
        "contentType": "application/json",
        "payload": { "$ref": "#/components/schemas/Alert" }
    }));
    map.insert("HealthWarning".to_owned(), health_warning_message());
    map.insert("ShedLoad".to_owned(), shed_load_message());
    map.insert("InputConnection".to_owned(), json!({
        "name": "InputConnection",
        "title": "Input source connection state change",
        "summary": "An input source transitioned connection state (input.connection on topic `inputs`).",
        "contentType": "application/json",
        "payload": { "$ref": "#/components/schemas/InputConnection" }
    }));
    map.insert("InputStreams".to_owned(), input_streams_message());
    map.insert(
        "JobProgress".to_owned(),
        json!({
            "name": "JobProgress",
            "title": "Long-running job progress",
            "summary": "Progress of a REST command job correlated by `corr` (topic `jobs`).",
            "contentType": "application/json",
            "payload": { "$ref": "#/components/schemas/JobProgress" }
        }),
    );
    map.insert(
        "AlarmTransition".to_owned(),
        json!({
            "name": "AlarmTransition",
            "title": "Monitoring alarm lifecycle event",
            "summary": "An alarm was raised, updated, cleared, or acknowledged (topic `alarms`).",
            "contentType": "application/json",
            "payload": { "$ref": "#/components/schemas/AlarmTransition" }
        }),
    );
    map.insert(
        "TallyEvent".to_owned(),
        json!({
            "name": "TallyEvent",
            "title": "Tally lamp/UMD state change",
            "summary": "Resolved tally state for one tile/element changed (topic `tally`).",
            "contentType": "application/json",
            "payload": { "$ref": "#/components/schemas/TallyEvent" }
        }),
    );
    map.insert(
        "SalvoEvent".to_owned(),
        json!({
            "name": "SalvoEvent",
            "title": "Salvo arm/take lifecycle",
            "summary": "A salvo was armed, taken, or cancelled (topic `tally`).",
            "contentType": "application/json",
            "payload": { "$ref": "#/components/schemas/SalvoEvent" }
        }),
    );
    map
}

/// The `tiles` `$snapshot` `AsyncAPI` message definition. Factored out so the
/// data-events builder stays within the line budget.
fn tiles_snapshot_message() -> Value {
    json!({
        "name": "TilesSnapshot",
        "title": "Connect-time tiles baseline ($snapshot on topic tiles)",
        "summary": "Full current per-tile lifecycle state a fresh client rebuilds its tile cache from.",
        "description": concat!(
            "Emitted once at connect (after `$hello`) on topic `tiles` with `t` ",
            "`$snapshot` (realtime.md §5): the snapshot-then-delta baseline ",
            "(`snapshot ⊕ ordered deltas = current truth`, ADR-RT003). A receiver ",
            "MUST treat it as a state REBUILD, never a merge. Despite the ",
            "`$`-prefixed tag it rides its data topic, not `$control`.",
        ),
        "contentType": "application/json",
        "payload": { "$ref": "#/components/schemas/TilesSnapshot" }
    })
}

/// The `health.warning.*` (SA-0) `AsyncAPI` message definition. Factored out so
/// the data-events builder stays within the line budget.
fn health_warning_message() -> Value {
    json!({
        "name": "HealthWarning",
        "title": "Actionable health warning raised or cleared",
        "summary": "A health warning was raised (health.warning.raised) or cleared (health.warning.cleared) on topic `alerts`.",
        "description": concat!(
            "A richer sibling of `Alert` carrying a stable `code`, the affected ",
            "`subsystem`, and a concrete `remediation`. The `code` is the dedupe ",
            "key: frames with the same code coalesce rather than stacking. ",
            "Capability-mismatch codes (e.g. `gpu-present-no-vulkan-adapter`) are ",
            "latched build-time facts — raised once, cleared on reconfigure/restart.",
        ),
        "contentType": "application/json",
        "payload": { "$ref": "#/components/schemas/HealthWarning" }
    })
}

/// The `shed.load` `AsyncAPI` message definition. Factored out so the
/// data-events builder stays within the line budget.
fn shed_load_message() -> Value {
    json!({
        "name": "ShedLoad",
        "title": "Resource-adaptive shed-load decision",
        "summary": "The engine shed work to relieve sustained overload (shed.load on topic `alerts`).",
        "description": concat!(
            "A discrete, lossless degradation-signal event (invariant #9): the ",
            "engine relieved sustained overload by shedding work rather than ",
            "blocking the output clock (invariants #1 + #10). The `reason` says ",
            "why a shed was chosen over hold/migrate; `scope` says what was shed ",
            "(program / a specific input / a shared resource); `level` is the ",
            "degradation-ladder rung after the shed and `dropped` the cumulative ",
            "frames/units shed. The consent-independent retention store records ",
            "these for the §7.2 support bundle (ADR-0052).",
        ),
        "contentType": "application/json",
        "payload": { "$ref": "#/components/schemas/ShedLoad" }
    })
}

/// The `input.streams` (RT-3) `AsyncAPI` message definition. Factored out so the
/// data-events builder stays within the line budget.
fn input_streams_message() -> Value {
    json!({
        "name": "InputStreams",
        "title": "Input elementary-stream inventory",
        "summary": "An input's stream inventory appeared or changed on re-probe (input.streams on topic `inputs`).",
        "description": concat!(
            "Read-only discovery: every elementary stream (video / audio tracks / ",
            "subtitles / SCTE-35 / KLV / timecode) the input offers, each with a ",
            "stable kind-scoped id. Emitted on first appearance and as a delta on ",
            "re-probe / PMT-version bump.",
        ),
        "contentType": "application/json",
        "payload": { "$ref": "#/components/schemas/InputStreams" }
    })
}

/// Build the Switcher realtime-surface message definitions (ADR-RT008): the
/// lossless `media.player_state` lifecycle event on topic `switcher`.
fn build_messages_switcher_events() -> serde_json::Map<String, Value> {
    let mut map = serde_json::Map::new();
    map.insert("MediaPlayerEvent".to_owned(), json!({
        "name": "MediaPlayerEvent",
        "title": "Media-player transport-state lifecycle",
        "summary": "A media player transitioned transport state (media.player_state on topic `switcher`).",
        "description": concat!(
            "Emitted on topic `switcher` with envelope `id` = player id. A LOSSLESS ",
            "lifecycle event kept in the bounded replay ring (ADR-RT008): discrete ",
            "transport-state changes (cued / playing / paused / stopped / ",
            "vamping{exit_armed} / eof / loading) plus the playhead `position_frames`. ",
            "It must never be conflated — a control surface reconstructs authoritative ",
            "player history from it. Per-frame position is NOT streamed; clients ",
            "interpolate `position_frames` between events at the output cadence.",
        ),
        "contentType": "application/json",
        "payload": { "$ref": "#/components/schemas/MediaPlayerEvent" }
    }));
    map
}

/// Build the Devices realtime-surface message definitions (ADR-RT007): the
/// conflated `device.status` / `timing.status` telemetry lanes plus the
/// lossless low-rate device lifecycle events, all on topic `devices`.
fn build_messages_device_events() -> serde_json::Map<String, Value> {
    let mut map = serde_json::Map::new();
    map.insert("DeviceStatus".to_owned(), json!({
        "name": "DeviceStatus",
        "title": "Conflated per-device runtime snapshot",
        "summary": "Latest-wins device status: state/mode/capabilities/streams/sync/temperature (device.status on topic `devices`).",
        "description": concat!(
            "Emitted on topic `devices` with envelope `id` = device id. CONFLATED ",
            "latest-wins telemetry (vendor-polled at ~1 s cadence): the latest value ",
            "supersedes all prior values, and the event is excluded from the lossless ",
            "replay ring — a re-snapshot heals it (ADR-RT007 extends ADR-RT003's ",
            "ring-exclusion rule to per-event-type granularity on this mixed-cadence ",
            "topic). Staleness is surfaced via `last_seen_ts`, never papered over.",
        ),
        "contentType": "application/json",
        "payload": { "$ref": "#/components/schemas/DeviceStatus" }
    }));
    map.insert("DeviceAdopted".to_owned(), json!({
        "name": "DeviceAdopted",
        "title": "Device adopted into the registry",
        "summary": "A device was adopted (device.adopted on topic `devices`; lossless lifecycle lane).",
        "contentType": "application/json",
        "payload": { "$ref": "#/components/schemas/DeviceAdopted" }
    }));
    map.insert("DeviceRemoved".to_owned(), json!({
        "name": "DeviceRemoved",
        "title": "Device removed from the registry",
        "summary": "A device was removed (device.removed on topic `devices`; lossless lifecycle lane).",
        "contentType": "application/json",
        "payload": { "$ref": "#/components/schemas/DeviceRemoved" }
    }));
    map.insert("DeviceMode".to_owned(), json!({
        "name": "DeviceMode",
        "title": "Device mode convergence",
        "summary": "Mode convergence started/finished/failed with its declared impact (device.mode on topic `devices`).",
        "description": concat!(
            "Carries the impact declared BEFORE apply per the instant-apply doctrine: ",
            "vendor-decoder mode convergence is `dev`-class (the device pipeline ",
            "restarts; Multiview program output is unaffected).",
        ),
        "contentType": "application/json",
        "payload": { "$ref": "#/components/schemas/DeviceMode" }
    }));
    map.insert("DeviceError".to_owned(), json!({
        "name": "DeviceError",
        "title": "Driver-reported device error",
        "summary": "A device error was reported (device.error on topic `devices`; lossless lifecycle lane).",
        "contentType": "application/json",
        "payload": { "$ref": "#/components/schemas/DeviceError" }
    }));
    map.insert("DeviceSync".to_owned(), json!({
        "name": "DeviceSync",
        "title": "Device sync participation change",
        "summary": "Sync-group membership / achieved-tier change or drift-threshold crossing (device.sync on topic `devices`).",
        "contentType": "application/json",
        "payload": { "$ref": "#/components/schemas/DeviceSync" }
    }));
    map.insert("DeviceDiscovered".to_owned(), json!({
        "name": "DeviceDiscovered",
        "title": "Untrusted discovery-inventory row",
        "summary": "A discovery row streamed while a scan operation runs (device.discovered on topic `devices`, correlated via `corr`).",
        "description": concat!(
            "Discovery results are an UNTRUSTED inventory requiring explicit ",
            "confirm-adopt — never auto-ingested. IPv6-first: IPv4 results are ",
            "explicitly labelled `ipv4-legacy`.",
        ),
        "contentType": "application/json",
        "payload": { "$ref": "#/components/schemas/DeviceDiscovered" }
    }));
    insert_cast_session_messages(&mut map);
    map.insert("TimingStatus".to_owned(), json!({
        "name": "TimingStatus",
        "title": "Outbound presentation epoch + sync telemetry",
        "summary": "The per-program WallClockRef epoch, link offset, clock source/quality, and per-sync-group achieved skew (timing.status on topic `devices`).",
        "description": concat!(
            "Emitted on topic `devices` with envelope `id` = program or sync-group id ",
            "(ADR-M010). CONFLATED latest-wins and excluded from the lossless replay ",
            "ring: the epoch is an exact affine map that stays valid when stale, so a ",
            "receiver that misses updates keeps the last epoch and free-runs — it ",
            "degrades, never stalls. The achieved tier is the MEASURED tier, never an ",
            "aspirational one.",
        ),
        "contentType": "application/json",
        "payload": { "$ref": "#/components/schemas/TimingStatus" }
    }));
    map
}

/// Insert the two ephemeral cast-session lifecycle message definitions
/// (`cast.session.started` / `cast.session.removed`) into the `devices`-topic
/// message map. Extracted from [`build_messages_device_events`] to keep that
/// builder under the function-length budget.
fn insert_cast_session_messages(map: &mut serde_json::Map<String, Value>) {
    map.insert("CastSessionStarted".to_owned(), json!({
        "name": "CastSessionStarted",
        "title": "Ephemeral cast session started",
        "summary": "An ad-hoc cast session was started (cast.session.started on topic `devices`; lossless lifecycle lane).",
        "description": concat!(
            "Emitted on topic `devices` with envelope `id` = session id when ",
            "`POST /api/v1/cast/sessions` accepts a start (the runtime record ",
            "exists and its supervised actor is spawned): the session-list ",
            "MEMBERSHIP changed, so clients refresh the list immediately instead ",
            "of waiting for a REST re-poll. The session's live state rides the ",
            "conflated `device.status` lane keyed by the same session id.",
        ),
        "contentType": "application/json",
        "payload": { "$ref": "#/components/schemas/CastSessionStarted" }
    }));
    map.insert("CastSessionRemoved".to_owned(), json!({
        "name": "CastSessionRemoved",
        "title": "Ephemeral cast session removed",
        "summary": "An ad-hoc cast session was removed (cast.session.removed on topic `devices`; lossless lifecycle lane).",
        "description": concat!(
            "Emitted on topic `devices` with envelope `id` = session id when a ",
            "session is stopped (`DELETE /api/v1/cast/sessions/{id}` — the ",
            "receiver STOP that clears the TV) or promoted to a saved device ",
            "(`POST /{id}/save` retires the ephemeral record while playback ",
            "continues under the promoted device id).",
        ),
        "contentType": "application/json",
        "payload": { "$ref": "#/components/schemas/CastSessionRemoved" }
    }));
}

/// Build control-frame message definitions ($hello, $subscribe, …).
fn build_messages_control_frames() -> serde_json::Map<String, Value> {
    let mut map = serde_json::Map::new();
    map.insert(
        "Hello".to_owned(),
        json!({
            "name": "Hello",
            "title": "$hello control frame",
            "summary": "First server frame after auth; advertises connection parameters.",
            "contentType": "application/json",
            "payload": { "$ref": "#/components/schemas/Hello" }
        }),
    );
    map.insert(
        "Subscribe".to_owned(),
        json!({
            "name": "Subscribe",
            "title": "$subscribe control frame (client to server)",
            "summary": "Client requests subscription to one or more topics.",
            "contentType": "application/json",
            "payload": { "$ref": "#/components/schemas/Subscribe" }
        }),
    );
    map
}

/// The envelope schema: the shape every frame must conform to.
///
/// Per ADR-RT002 the `t` + `data` pair is the discriminator + body.
/// Optional fields (`id`, `corr`) are marked `required: false`.
fn envelope_schema() -> Value {
    json!({
        "type": "object",
        "required": ["v", "t", "topic", "seq", "ts", "data"],
        "properties": {
            "v": {
                "type": "integer",
                "format": "uint16",
                "description": "Envelope schema major. A receiver MUST reject an unknown major.",
                "const": 1
            },
            "t": {
                "type": "string",
                "description": "Dotted event type; the discriminator selecting the `data` schema."
            },
            "topic": {
                "type": "string",
                "description": "Subscription routing key. Control frames use `$control`.",
                "enum": [
                    "$control", "system", "capabilities", "inputs", "tiles", "outputs",
                    "audio.meters", "audio.loudness", "alerts", "alarms", "tally",
                    "layout", "config", "logs", "jobs", "preview", "devices", "switcher"
                ]
            },
            "id": {
                "type": "string",
                "description": "Optional resource scope (tile/input/output/job id)."
            },
            "seq": {
                "type": "integer",
                "format": "uint64",
                "description": "Per-connection monotonic resume cursor. A gap means frames were dropped."
            },
            "ts": {
                "type": "integer",
                "format": "int64",
                "description": "Engine monotonic nanoseconds (same clock family as output PTS)."
            },
            "corr": {
                "type": "string",
                "description": "Optional correlation id echoing a REST command / job."
            },
            "data": {
                "type": "object",
                "description": "Typed payload selected by `t`. Schema depends on the event type.",
                "oneOf": event_payload_one_of()
            }
        }
    })
}

/// Build the `oneOf` discriminator list for the envelope `data` field.
///
/// Each entry is a `$ref` into `#/components/schemas` for a known payload type.
/// The discriminator is the envelope-level `t` field (not inside `data` itself).
fn event_payload_one_of() -> Value {
    json!([
        { "$ref": "#/components/schemas/TileState" },
        { "$ref": "#/components/schemas/TilesSnapshot" },
        { "$ref": "#/components/schemas/AudioMeter" },
        { "$ref": "#/components/schemas/AudioLoudness" },
        { "$ref": "#/components/schemas/OutputStatus" },
        { "$ref": "#/components/schemas/Alert" },
        { "$ref": "#/components/schemas/HealthWarning" },
        { "$ref": "#/components/schemas/InputConnection" },
        { "$ref": "#/components/schemas/InputStreams" },
        { "$ref": "#/components/schemas/JobProgress" },
        { "$ref": "#/components/schemas/AlarmTransition" },
        { "$ref": "#/components/schemas/TallyEvent" },
        { "$ref": "#/components/schemas/SalvoEvent" },
        { "$ref": "#/components/schemas/MediaPlayerEvent" },
        { "$ref": "#/components/schemas/DeviceStatus" },
        { "$ref": "#/components/schemas/DeviceAdopted" },
        { "$ref": "#/components/schemas/DeviceRemoved" },
        { "$ref": "#/components/schemas/DeviceMode" },
        { "$ref": "#/components/schemas/DeviceError" },
        { "$ref": "#/components/schemas/DeviceSync" },
        { "$ref": "#/components/schemas/DeviceDiscovered" },
        { "$ref": "#/components/schemas/CastSessionStarted" },
        { "$ref": "#/components/schemas/CastSessionRemoved" },
        { "$ref": "#/components/schemas/TimingStatus" },
        { "$ref": "#/components/schemas/Hello" },
        { "$ref": "#/components/schemas/Subscribe" },
        { "$ref": "#/components/schemas/Subscribed" },
        { "$ref": "#/components/schemas/Unsubscribe" },
        { "$ref": "#/components/schemas/SetRate" },
        { "$ref": "#/components/schemas/Resume" },
        { "$ref": "#/components/schemas/Resync" },
        { "$ref": "#/components/schemas/Lag" },
        { "$ref": "#/components/schemas/ProtocolError" }
    ])
}

/// Build the `components/schemas` map: one schema per wire payload type.
///
/// These schemas mirror the serde wire shapes of the types in [`crate::event`]
/// and [`crate::subscription`]. The canonical source of truth is the Rust type
/// definitions (any drift here must be corrected in favour of the Rust code).
fn build_schemas() -> Value {
    // Assembled in three sections (core data events, the Devices surface,
    // control frames) because a single `json!` literal of this size exceeds
    // the macro recursion limit. Section order is stable, so the generated
    // document stays deterministic.
    let mut map = serde_json::Map::new();
    for section in [
        core_event_schemas(),
        switcher_event_schemas(),
        device_event_schemas(),
        control_frame_schemas(),
    ] {
        // Each section builder returns a `json!` object literal, so the
        // object arm always matches; nothing is skipped.
        if let Value::Object(entries) = section {
            map.extend(entries);
        }
    }
    Value::Object(map)
}

/// The core data-event payload schemas (tiles, audio, outputs, alerts, …).
fn core_event_schemas() -> Value {
    json!({
        "LifecycleState": lifecycle_state_schema(),
        "TileState": tile_state_schema(),
        "TileSnapshotEntry": tile_snapshot_entry_schema(),
        "TilesSnapshot": tiles_snapshot_schema(),
        "AudioMeter": audio_meter_schema(),
        "AudioLoudness": audio_loudness_schema(),
        "SystemMetrics": system_metrics_schema(),
        "GpuMetrics": gpu_metrics_schema(),
        "GpuVendor": gpu_vendor_schema(),
        "OutputRunState": output_run_state_schema(),
        "OutputStatus": output_status_schema(),
        "AlertSeverity": alert_severity_schema(),
        "Alert": alert_schema(),
        "WarningSeverity": warning_severity_schema(),
        "WarningCode": warning_code_schema(),
        "HealthWarning": health_warning_schema(),
        "ShedReason": shed_reason_schema(),
        "ShedScope": shed_scope_schema(),
        "ShedLoad": shed_load_schema(),
        "InputConnection": input_connection_schema(),
        "InputStreams": input_streams_schema(),
        "JobProgress": job_progress_schema(),
        "AlarmTransition": alarm_transition_schema(),
        "TallyTarget": tally_target_schema(),
        "TallyEvent": tally_event_schema(),
        "SalvoEvent": salvo_event_schema()
    })
}

/// The Devices realtime-surface payload schemas (ADR-RT007).
fn device_event_schemas() -> Value {
    json!({
        "DeviceState": device_state_schema(),
        "SyncCapability": sync_capability_schema(),
        "DeviceCapabilities": device_capabilities_schema(),
        "DeviceStreamStatus": device_stream_status_schema(),
        "AchievedSync": achieved_sync_schema(),
        "DeviceSyncSummary": device_sync_summary_schema(),
        "DeviceStatus": device_status_schema(),
        "DeviceAdopted": device_adopted_schema(),
        "DeviceRemoved": device_removed_schema(),
        "ImpactClass": impact_class_schema(),
        "DeviceMode": device_mode_schema(),
        "DeviceError": device_error_schema(),
        "SyncChange": sync_change_schema(),
        "DeviceSync": device_sync_schema(),
        "DeviceDiscovered": device_discovered_schema(),
        "CastSessionStarted": cast_session_started_schema(),
        "CastSessionRemoved": cast_session_removed_schema(),
        "WallClockRef": wall_clock_ref_schema(),
        "SyncGroupSkew": sync_group_skew_schema(),
        "TimingStatus": timing_status_schema()
    })
}

/// The `$control`-frame payload schemas ($hello, $subscribe, …).
fn control_frame_schemas() -> Value {
    json!({
        "Hello": hello_schema(),
        "Subscribe": subscribe_schema(),
        "Subscribed": subscribed_schema(),
        "Unsubscribe": unsubscribe_schema(),
        "SetRate": set_rate_schema(),
        "Resume": resume_schema(),
        "ResyncReason": resync_reason_schema(),
        "Resync": resync_schema(),
        "LagAction": lag_action_schema(),
        "Lag": lag_schema(),
        "ProtocolError": protocol_error_schema()
    })
}

// --- Individual payload schemas ---
// Each function mirrors the serde wire shape of its Rust counterpart.

fn lifecycle_state_schema() -> Value {
    json!({
        "type": "string",
        "description": "Tile/input lifecycle state (invariant #2: Live → Stale → Reconnecting → NoSignal).",
        "enum": ["LIVE", "STALE", "RECONNECTING", "NO_SIGNAL"]
    })
}

fn tile_state_schema() -> Value {
    json!({
        "type": "object",
        "description": "Data body of the `tile.state` event: a tile lifecycle transition.",
        "required": ["from", "to", "trigger"],
        "properties": {
            "from": { "$ref": "#/components/schemas/LifecycleState" },
            "to": { "$ref": "#/components/schemas/LifecycleState" },
            "input": {
                "type": "string",
                "description": "The input bound to the tile at the time of the transition, if any."
            },
            "trigger": {
                "type": "string",
                "description": "Short machine-readable trigger label (e.g. `nosignal_timeout`)."
            }
        }
    })
}

fn tile_snapshot_entry_schema() -> Value {
    json!({
        "type": "object",
        "description": concat!(
            "One tile's current lifecycle state inside a `tiles` `$snapshot`. ",
            "The `id` is the same key the sparse `tile.state` deltas scope their ",
            "envelope `id` with, so a snapshot-rebuilt cache and its delta ",
            "patches address the same rows.",
        ),
        "required": ["id", "state"],
        "properties": {
            "id": {
                "type": "string",
                "description": "The tile id (the bound source id in the run projection)."
            },
            "state": { "$ref": "#/components/schemas/LifecycleState" },
            "input": {
                "type": "string",
                "description": "The input bound to the tile, if known."
            }
        }
    })
}

fn tiles_snapshot_schema() -> Value {
    json!({
        "type": "object",
        "description": concat!(
            "Data body of the `tiles`-topic `$snapshot` frame: the full current ",
            "per-tile lifecycle baseline sent once at connect (after `$hello`). ",
            "A receiver MUST rebuild (replace) its tile state from it, never merge.",
        ),
        "required": ["as_of_seq", "tiles"],
        "properties": {
            "as_of_seq": {
                "type": "integer",
                "format": "uint64",
                "description": "The engine state sequence this baseline is current as of."
            },
            "tiles": {
                "type": "array",
                "items": { "$ref": "#/components/schemas/TileSnapshotEntry" },
                "description": "Every tile's current lifecycle state."
            }
        }
    })
}

fn audio_meter_schema() -> Value {
    json!({
        "type": "object",
        "description": concat!(
            "Data body of the `audio.meter` event: a high-rate per-input/track peak/RMS/clip ",
            "sample. This is numeric metadata only — never audio content. ",
            "Conflated/sampled at 10–30 Hz on the wire (high-rate lane).",
        ),
        "required": ["track", "peak_db", "rms_db", "clip", "overflow", "sampled_hz"],
        "properties": {
            "track": {
                "type": "integer",
                "format": "uint32",
                "description": "Track index."
            },
            "peak_db": {
                "type": "array",
                "items": { "type": "number", "format": "float" },
                "description": "Per-channel peak level (dBFS)."
            },
            "rms_db": {
                "type": "array",
                "items": { "type": "number", "format": "float" },
                "description": "Per-channel RMS level (dBFS)."
            },
            "clip": {
                "type": "boolean",
                "description": "Whether any channel clipped in this window."
            },
            "overflow": {
                "type": "boolean",
                "description": "Whether the meter pipeline overflowed (dropped windows)."
            },
            "sampled_hz": {
                "type": "integer",
                "format": "uint32",
                "description": "Effective wire cadence (Hz)."
            }
        }
    })
}

fn audio_loudness_message() -> Value {
    json!({
        "name": "AudioLoudness",
        "title": "Program-bus EBU R128 loudness sample",
        "summary": "Momentary/short-term/integrated LUFS + LRA + true-peak (dBTP) + compliance reference (numeric only).",
        "description": concat!(
            "Emitted on topic `audio.loudness` (AUD-8). A high-rate CONFLATED compliance lane ",
            "(BS.1770-4 / EBU Tech 3341/3342, ADR-R006): the slow-cadence M/S/I/LRA/dBTP sample ",
            "the loudness meter measures read-only off the audio bus, pushed at ~10 Hz and ",
            "excluded from the lossless replay ring. A slow consumer can only lose its own samples ",
            "— the engine never back-pressures (inv #10). BALLISTICS are applied client-side: the ",
            "wire carries raw measured values; the browser meter applies the display decay/peak-hold. ",
            "The compliance reference (`target_lufs`/`ceiling_dbtp`/`tolerance_lu`) rides every sample ",
            "so the meter colours against the same target the program bus is normalised toward.",
        ),
        "contentType": "application/json",
        "payload": { "$ref": "#/components/schemas/AudioLoudness" }
    })
}

fn audio_loudness_schema() -> Value {
    json!({
        "type": "object",
        "description": concat!(
            "Data body of the `audio.loudness` event: a program-bus EBU R128 loudness sample ",
            "(M/S/I LUFS, LRA, true-peak dBTP) plus the compliance reference. Numeric metadata ",
            "only — never audio content. Conflated/sampled at ~10 Hz on the wire (high-rate lane); ",
            "ballistics are applied client-side. The integrating fields are absent below the ",
            "-70 LUFS absolute gate (no fabricated value).",
        ),
        "required": ["program", "target_lufs", "ceiling_dbtp", "tolerance_lu", "sampled_hz"],
        "properties": {
            "program": {
                "type": "integer",
                "format": "uint32",
                "description": "Program/bus index this loudness sample is for."
            },
            "momentary": {
                "type": "number",
                "format": "float",
                "description": "Momentary loudness (400 ms window), LUFS. Absent below the absolute gate."
            },
            "short_term": {
                "type": "number",
                "format": "float",
                "description": "Short-term loudness (3 s window), LUFS. Absent below the absolute gate."
            },
            "integrated": {
                "type": "number",
                "format": "float",
                "description": "Integrated (gated) loudness, LUFS. Absent until enough gated audio."
            },
            "lra": {
                "type": "number",
                "format": "float",
                "description": "Loudness range (EBU Tech 3342), LU. Absent until enough gated audio."
            },
            "true_peak_dbtp": {
                "type": "number",
                "format": "float",
                "description": "Maximum true-peak across channels (4x oversampled), dBTP. Absent when disabled."
            },
            "target_lufs": {
                "type": "number",
                "format": "float",
                "description": "Normalisation target loudness, LUFS (compliance reference, e.g. -23 / -16)."
            },
            "ceiling_dbtp": {
                "type": "number",
                "format": "float",
                "description": "True-peak ceiling, dBTP (compliance reference, e.g. -1.5)."
            },
            "tolerance_lu": {
                "type": "number",
                "format": "float",
                "description": "Live convergence tolerance, LU (the in-spec band is target ± tolerance)."
            },
            "gain_db": {
                "type": "number",
                "format": "float",
                "description": "Makeup gain the loudnorm processor is applying, dB. Absent when no normaliser is engaged."
            },
            "sampled_hz": {
                "type": "integer",
                "format": "uint32",
                "description": "Effective wire cadence (Hz)."
            }
        }
    })
}

fn system_metrics_message() -> Value {
    json!({
        "name": "SystemMetrics",
        "title": "High-rate whole-system metrics sample",
        "summary": "CPU / GPU / encoder-decoder utilisation sample (numeric only).",
        "description": concat!(
            "Emitted on topic `system`. A high-rate conflated lane like `audio.meters`: ",
            "samples are latest-only at ~1-2 Hz on the wire and excluded from the lossless ",
            "replay ring. PUSHED, never polled — a slow UI loses only its own samples and the ",
            "engine never back-pressures (inv #10). Live values only; historic windows (the ",
            "data decisions are made from) are a separate cold REST query.",
        ),
        "contentType": "application/json",
        "payload": { "$ref": "#/components/schemas/SystemMetrics" }
    })
}

fn system_metrics_schema() -> Value {
    json!({
        "type": "object",
        "description": concat!(
            "Data body of the `system.metrics` event: a high-rate whole-system sample ",
            "(cpu / gpu / encoder-decoder). Numeric only; conflated at ~1-2 Hz (high-rate lane).",
        ),
        "required": ["cpu_util", "sampled_hz"],
        "properties": {
            "cpu_util": { "type": "number", "format": "float", "description": "Whole-system CPU utilisation, 0.0-1.0." },
            "mem_used_bytes": { "type": "integer", "format": "uint64", "description": "Host memory in use (bytes), if known." },
            "mem_total_bytes": { "type": "integer", "format": "uint64", "description": "Total host memory (bytes), if known." },
            "gpus": {
                "type": "array",
                "items": { "$ref": "#/components/schemas/GpuMetrics" },
                "description": "Per-GPU utilisation samples; empty on a GPU-free host."
            },
            "program_fps": { "type": "number", "format": "float", "description": "Aggregate program output rate (fps), if running." },
            "sampled_hz": { "type": "integer", "format": "uint32", "description": "Effective wire sampling cadence (Hz)." }
        }
    })
}

fn gpu_metrics_schema() -> Value {
    json!({
        "type": "object",
        "description": "A per-GPU utilisation sample. Optional fields are absent where the vendor does not expose that signal.",
        "required": ["id", "vendor", "compute_util", "mem_used_bytes", "mem_total_bytes"],
        "properties": {
            "id": { "type": "string", "description": "Stable device identity (UUID where available, else an index)." },
            "vendor": { "$ref": "#/components/schemas/GpuVendor" },
            "name": { "type": "string", "description": "Human-readable device name, if known." },
            "compute_util": { "type": "number", "format": "float", "description": "Compute (graphics/CUDA) utilisation, 0.0-1.0." },
            "mem_used_bytes": { "type": "integer", "format": "uint64", "description": "VRAM in use (bytes)." },
            "mem_total_bytes": { "type": "integer", "format": "uint64", "description": "Total VRAM (bytes)." },
            "encoder_util": { "type": "number", "format": "float", "description": "Encoder (NVENC/QSV) ASIC utilisation, 0.0-1.0 (vendor-dependent)." },
            "decoder_util": { "type": "number", "format": "float", "description": "Decoder (NVDEC/QSV) ASIC utilisation, 0.0-1.0 (vendor-dependent)." },
            "encoder_sessions": { "type": "integer", "format": "uint32", "description": "DEVICE-WIDE active concurrent encode sessions (NVIDIA) across all processes." },
            "encoder_session_ceiling": { "type": "integer", "format": "uint32", "description": "Runtime-discovered concurrent encode-session ceiling (NVIDIA)." },
            "self_compute_util": { "type": "number", "format": "float", "description": "Our process's compute (SM) utilisation on this GPU, 0.0-1.0." },
            "self_encoder_util": { "type": "number", "format": "float", "description": "Our process's encoder (NVENC) utilisation, 0.0-1.0." },
            "self_decoder_util": { "type": "number", "format": "float", "description": "Our process's decoder (NVDEC) utilisation, 0.0-1.0." },
            "self_mem_used_bytes": { "type": "integer", "format": "uint64", "description": "VRAM (bytes) attributed to our process on this GPU." },
            "self_encoder_sessions": { "type": "integer", "format": "uint32", "description": "Encode sessions owned by our process on this GPU." }
        }
    })
}

fn gpu_vendor_schema() -> Value {
    json!({
        "type": "string",
        "description": "GPU/accelerator hardware vendor (selects which per-engine signals to expect).",
        "enum": ["nvidia", "intel", "amd", "apple", "other"]
    })
}

fn output_run_state_schema() -> Value {
    json!({
        "type": "string",
        "description": "Running state of an output sink.",
        "enum": ["starting", "running", "migrating", "error"]
    })
}

fn output_status_schema() -> Value {
    json!({
        "type": "object",
        "description": "Data body of the `output.status` event.",
        "required": ["state"],
        "properties": {
            "state": { "$ref": "#/components/schemas/OutputRunState" },
            "bitrate_bps": {
                "type": "integer",
                "format": "uint64",
                "description": "Measured output bitrate (bits/sec), if known."
            },
            "clients": {
                "type": "integer",
                "format": "uint32",
                "description": "Number of currently-connected consumers, if known."
            }
        }
    })
}

fn alert_severity_schema() -> Value {
    json!({
        "type": "string",
        "description": "Severity of an operator alert.",
        "enum": ["info", "warning", "critical"]
    })
}

fn alert_schema() -> Value {
    json!({
        "type": "object",
        "description": concat!(
            "Data body of `alert.raised` and `alert.cleared` events. ",
            "The `key` field is a stable dedupe identifier: multiple raised frames ",
            "with the same key coalesce.",
        ),
        "required": ["key", "severity", "title", "active"],
        "properties": {
            "key": {
                "type": "string",
                "description": "Stable dedupe key for the condition."
            },
            "severity": { "$ref": "#/components/schemas/AlertSeverity" },
            "title": {
                "type": "string",
                "description": "Short human-readable title."
            },
            "detail": {
                "type": "string",
                "description": "Optional longer detail."
            },
            "active": {
                "type": "boolean",
                "description": "Whether the condition is currently active."
            }
        }
    })
}

fn warning_severity_schema() -> Value {
    json!({
        "type": "string",
        "description": "Severity of a health warning (sibling of AlertSeverity).",
        "enum": ["info", "warning", "critical"]
    })
}

fn warning_code_schema() -> Value {
    json!({
        "type": "string",
        "description": concat!(
            "Stable catalog code of a health warning (kebab-case). ",
            "`#[non_exhaustive]`: the catalog grows over time, so a client must ",
            "treat an unknown code as a forward-compatible warning, not an error.",
        ),
        "enum": ["gpu-present-no-vulkan-adapter", "config-file-invalid", "config-file-requires-restart", "config-file-apply-incomplete"]
    })
}

fn health_warning_schema() -> Value {
    json!({
        "type": "object",
        "description": concat!(
            "Data body of `health.warning.raised` and `health.warning.cleared`. ",
            "A richer sibling of `Alert`: the `code` is the stable dedupe key, and ",
            "`remediation` carries the concrete fix. Capability-mismatch codes are ",
            "latched build-time facts (raised once, cleared on reconfigure/restart).",
        ),
        "required": ["code", "severity", "subsystem", "message", "remediation", "since", "active"],
        "properties": {
            "code": { "$ref": "#/components/schemas/WarningCode" },
            "severity": { "$ref": "#/components/schemas/WarningSeverity" },
            "subsystem": {
                "type": "string",
                "description": "The affected subsystem (e.g. `compositor`, `decode`, `encode`, `gpu`)."
            },
            "message": {
                "type": "string",
                "description": "A clear, human-readable description of the condition."
            },
            "remediation": {
                "type": "string",
                "description": "The concrete remediation — what the operator must do to fix it."
            },
            "since": {
                "type": "integer",
                "format": "int64",
                "description": "When the condition was first raised (engine monotonic nanoseconds)."
            },
            "active": {
                "type": "boolean",
                "description": "Whether the condition is currently active (raise vs clear)."
            }
        }
    })
}

fn shed_reason_schema() -> Value {
    json!({
        "type": "string",
        "description": concat!(
            "Why the resource-adaptive controller shed load rather than holding ",
            "or migrating the pipeline (invariant #9). Mirrors the engine's ",
            "`ShedReason`; additive + non-exhaustive (ADR-RT002/RT003).",
        ),
        "enum": ["pinned", "display_bound", "no_better_home", "anti_storm", "encoder_overload"]
    })
}

fn shed_scope_schema() -> Value {
    json!({
        "type": "object",
        "description": "What a shed-load decision applied to (tagged by `kind`).",
        "required": ["kind"],
        // AsyncAPI 3.0 Schema Object: `discriminator` is the property NAME.
        "discriminator": "kind",
        "oneOf": [
            {
                "type": "object",
                "required": ["kind"],
                "properties": {
                    "kind": { "type": "string", "const": "program" }
                },
                "description": "The shed touched the whole-program encode/egress path (a composited frame was dropped)."
            },
            {
                "type": "object",
                "required": ["kind", "id"],
                "properties": {
                    "kind": { "type": "string", "const": "input" },
                    "id": { "type": "string", "description": "The configured input/source id the shed degraded." }
                },
                "description": "The shed degraded a specific input/tile (cheapest-impact-first ladder)."
            },
            {
                "type": "object",
                "required": ["kind"],
                "properties": {
                    "kind": { "type": "string", "const": "shared" }
                },
                "description": "The shed degraded a shared resource (e.g. a preview/encode pool)."
            }
        ]
    })
}

fn shed_load_schema() -> Value {
    json!({
        "type": "object",
        "description": concat!(
            "Data body of the `shed.load` event: a resource-adaptive shed-load ",
            "decision (invariant #9). The consent-independent retention store ",
            "records these for the §7.2 support bundle (ADR-0052).",
        ),
        "required": ["reason", "scope", "level", "dropped"],
        "properties": {
            "reason": { "$ref": "#/components/schemas/ShedReason" },
            "scope": { "$ref": "#/components/schemas/ShedScope" },
            "level": {
                "type": "integer",
                "format": "uint32",
                "description": "The degradation-ladder level after the shed (0 = full quality)."
            },
            "dropped": {
                "type": "integer",
                "format": "uint64",
                "description": "Cumulative frames/units shed under this condition at the time of the event."
            }
        }
    })
}

fn input_connection_schema() -> Value {
    json!({
        "type": "object",
        "description": "Data body of the `input.connection` event.",
        "required": ["input_id", "state"],
        "properties": {
            "input_id": {
                "type": "string",
                "description": "The owning input's id (the configured source id) — the object axis this lifecycle event is authorized under (ADR-W026)."
            },
            "state": { "$ref": "#/components/schemas/LifecycleState" },
            "attempt": {
                "type": "integer",
                "format": "uint32",
                "description": "Reconnect attempt counter, if reconnecting."
            }
        }
    })
}

fn input_streams_schema() -> Value {
    // The inventory itself is a rich nested model (`multiview_core::stream`:
    // StreamDescriptor / StableStreamId / StreamKind / StreamDetail). The Rust
    // type is the canonical source of truth; this AsyncAPI mirror keeps the
    // top-level shape exact and treats the inventory as an open object rather
    // than re-stating the full nested schema (which would drift). The typed
    // schema is surfaced on the REST `GET /inputs/{id}/streams` OpenAPI surface.
    json!({
        "type": "object",
        "description": "Data body of the `input.streams` event: an input's full elementary-stream inventory (RT-3).",
        "required": ["input_id", "inventory"],
        "properties": {
            "input_id": {
                "type": "string",
                "description": "The owning input's configured source id."
            },
            "inventory": {
                "type": "object",
                "description": "The StreamInventory (multiview_core::stream::StreamInventory): every elementary stream the input offers.",
                "required": ["streams"],
                "properties": {
                    "input_id": {
                        "type": ["string", "null"],
                        "description": "The input id the inventory is bound to, if known."
                    },
                    "streams": {
                        "type": "array",
                        "description": "Every elementary stream, in container order.",
                        "items": { "type": "object" }
                    }
                }
            }
        }
    })
}

fn job_progress_schema() -> Value {
    json!({
        "type": "object",
        "description": "Data body of `job.progress`; correlated to a REST command via the envelope `corr`.",
        "required": ["phase", "pct"],
        "properties": {
            "phase": {
                "type": "string",
                "description": "Short machine-readable phase label."
            },
            "pct": {
                "type": "integer",
                "minimum": 0,
                "maximum": 100,
                "description": "Percent complete (0–100)."
            },
            "message": {
                "type": "string",
                "description": "Optional human-readable progress message."
            }
        }
    })
}

fn alarm_transition_schema() -> Value {
    json!({
        "type": "object",
        "description": concat!(
            "Data body of `alarm.raised`, `alarm.updated`, `alarm.cleared`, `alarm.acked`. ",
            "Carries the current AlarmRecord value after the transition (X.733-aligned).",
        ),
        "required": ["record"],
        "properties": {
            "record": {
                "type": "object",
                "description": "The current alarm record after the transition.",
                "additionalProperties": true
            }
        }
    })
}

fn tally_target_schema() -> Value {
    json!({
        "type": "object",
        "description": "What a tally state applies to (tagged by `kind`).",
        "required": ["kind"],
        // AsyncAPI 3.0 Schema Object: `discriminator` is the property NAME (a
        // string), not the OpenAPI-style `{ propertyName }` object.
        "discriminator": "kind",
        "oneOf": [
            {
                "type": "object",
                "required": ["kind", "index"],
                "properties": {
                    "kind": { "type": "string", "const": "tile" },
                    "index": { "type": "integer", "format": "uint32", "description": "Zero-based tile index." }
                }
            },
            {
                "type": "object",
                "required": ["kind", "name"],
                "properties": {
                    "kind": { "type": "string", "const": "element" },
                    "name": { "type": "string", "description": "Element name." }
                }
            }
        ]
    })
}

fn tally_event_schema() -> Value {
    json!({
        "type": "object",
        "description": "Data body of `tally.state`: resolved tally lamp/UMD state for one element.",
        "required": ["target", "state"],
        "properties": {
            "target": { "$ref": "#/components/schemas/TallyTarget" },
            "state": {
                "type": "object",
                "description": "The resolved tally lamp/UMD state.",
                "additionalProperties": true
            }
        }
    })
}

fn salvo_event_schema() -> Value {
    json!({
        "type": "object",
        "description": "Data body of `salvo.armed`, `salvo.taken`, `salvo.cancelled`.",
        "required": ["salvo", "phase"],
        "properties": {
            "salvo": { "type": "string", "description": "Stable salvo identifier/name." },
            "phase": {
                "type": "string",
                "enum": ["armed", "taken", "cancelled"],
                "description": "The lifecycle phase this event reports."
            },
            "head": {
                "type": "string",
                "description": "Output head this recall applies to, if scoped."
            }
        }
    })
}

// --- Switcher realtime-surface schemas (ADR-RT008) ---
// Mirror the serde wire shapes of the switcher types in `crate::event`; the Rust
// definitions are the canonical source of truth.

/// The Switcher realtime-surface payload schemas (ADR-RT008): the media-player
/// transport-state event and its tagged state union.
fn switcher_event_schemas() -> Value {
    json!({
        "MediaPlayerState": media_player_state_schema(),
        "MediaPlayerEvent": media_player_event_schema()
    })
}

fn media_player_state_schema() -> Value {
    json!({
        "type": "object",
        "description": concat!(
            "A media player's discrete transport state (tagged by `kind`, never ",
            "untagged). The `vamping` variant carries `exit_armed`: when set, the ",
            "current vamp lap finishes then the player exits cleanly at the boundary. ",
            "`#[non_exhaustive]`: a client must treat an unknown kind as forward-",
            "compatible, not an error.",
        ),
        "required": ["kind"],
        // AsyncAPI 3.0 Schema Object: `discriminator` is the property NAME.
        "discriminator": "kind",
        "oneOf": [
            {
                "type": "object",
                "required": ["kind"],
                "properties": { "kind": { "type": "string", "const": "loading" } },
                "description": "Opening/decoding the asset toward the cue point; not yet primed."
            },
            {
                "type": "object",
                "required": ["kind"],
                "properties": { "kind": { "type": "string", "const": "cued" } },
                "description": "Asset loaded, in-point frame primed, holding paused — ready to take."
            },
            {
                "type": "object",
                "required": ["kind"],
                "properties": { "kind": { "type": "string", "const": "playing" } },
                "description": "Playing forward at the output cadence."
            },
            {
                "type": "object",
                "required": ["kind"],
                "properties": { "kind": { "type": "string", "const": "paused" } },
                "description": "Paused, holding the current frame."
            },
            {
                "type": "object",
                "required": ["kind"],
                "properties": { "kind": { "type": "string", "const": "stopped" } },
                "description": "Stopped/idle; no asset rolling."
            },
            {
                "type": "object",
                "required": ["kind", "exit_armed"],
                "properties": {
                    "kind": { "type": "string", "const": "vamping" },
                    "exit_armed": {
                        "type": "boolean",
                        "description": "Whether a clean exit is armed: finish the current lap, then leave the vamp at the boundary."
                    }
                },
                "description": "Looping a fill segment to hold until a cue (the VT vamp/exit feature)."
            },
            {
                "type": "object",
                "required": ["kind"],
                "properties": { "kind": { "type": "string", "const": "eof" } },
                "description": "Reached end-of-file with no loop/vamp engaged."
            }
        ]
    })
}

fn media_player_event_schema() -> Value {
    json!({
        "type": "object",
        "description": concat!(
            "Data body of `media.player_state` (ADR-RT008): a media player's discrete ",
            "transport-state transition, envelope `id` = player id. LOSSLESS — kept in ",
            "the replay ring, never conflated. `position_frames` is the playhead in ",
            "integer frames at the output cadence (never a float; clients interpolate ",
            "between events).",
        ),
        "required": ["player", "state", "position_frames"],
        "properties": {
            "player": { "type": "string", "description": "Stable media-player id (matches the envelope `id`)." },
            "asset": { "type": "string", "description": "The asset currently loaded in the player, if any." },
            "state": { "$ref": "#/components/schemas/MediaPlayerState" },
            "position_frames": {
                "type": "integer",
                "format": "uint64",
                "description": "Playhead position at the transition, in integer frames at the output cadence."
            }
        }
    })
}

// --- Devices realtime-surface schemas (ADR-RT007) ---
// Mirror the serde wire shapes of the Devices types in `crate::event`; the
// Rust definitions are the canonical source of truth.

fn device_state_schema() -> Value {
    json!({
        "type": "string",
        "description": "Managed-device lifecycle state (managed-devices.md §2.2), uppercase on the wire.",
        "enum": ["DISCOVERED", "ADOPTING", "ONLINE", "DEGRADED", "AUTH_FAILED", "UNREACHABLE"]
    })
}

fn sync_capability_schema() -> Value {
    json!({
        "type": "string",
        "description": "How well a device can participate in synchronized presentation (fixed probed tri-state).",
        "enum": ["frame-accurate", "offset-only", "none"]
    })
}

fn device_capabilities_schema() -> Value {
    json!({
        "type": "object",
        "description": "Fixed probed capability flags — a driver maps its device into exactly this shape.",
        "required": ["encode", "decode", "display", "sync", "audio", "reboot", "firmware_update"],
        "properties": {
            "encode": { "type": "boolean", "description": "The device can encode (offers streams Multiview ingests as Sources)." },
            "decode": { "type": "boolean", "description": "The device can decode (receive a Multiview output)." },
            "display": { "type": "boolean", "description": "The device drives a physical display." },
            "sync": { "$ref": "#/components/schemas/SyncCapability" },
            "audio": { "type": "boolean", "description": "The device handles audio." },
            "reboot": { "type": "boolean", "description": "The device can be rebooted via the management channel." },
            "firmware_update": { "type": "boolean", "description": "The device supports managed firmware update." }
        }
    })
}

fn device_stream_status_schema() -> Value {
    json!({
        "type": "object",
        "description": "One device-reported active stream. Optional fields are absent where the vendor does not report that figure.",
        "required": ["role", "healthy"],
        "properties": {
            "role": {
                "type": "string",
                "enum": ["encode", "decode"],
                "description": "Whether the device is encoding or decoding this stream."
            },
            "output_ref": { "type": "string", "description": "The Multiview output this decode stream is bound to, if any." },
            "bitrate_bps": { "type": "integer", "format": "uint64", "description": "Device-reported stream bitrate (bits/sec), if reported." },
            "fps": { "type": "number", "format": "float", "description": "Device-reported stream rate (fps), if reported." },
            "healthy": { "type": "boolean", "description": "Whether the device reports this stream as healthy." }
        }
    })
}

fn achieved_sync_schema() -> Value {
    json!({
        "type": "string",
        "description": concat!(
            "The MEASURED sync tier actually achieved (never aspirational): ",
            "frame-accurate (our nodes), bounded-skew (vendor decoders, ±100–500 ms ",
            "drift), or none (never part of a synchronized canvas).",
        ),
        "enum": ["frame-accurate", "bounded-skew", "none"]
    })
}

fn device_sync_summary_schema() -> Value {
    json!({
        "type": "object",
        "description": "A device's sync-group membership summary inside a device.status snapshot.",
        "required": ["group", "offset_ms", "achieved"],
        "properties": {
            "group": { "type": "string", "description": "The sync group this device belongs to." },
            "offset_ms": { "type": "integer", "format": "int64", "description": "Per-member presentation offset trim (ms, AES67 link-offset semantics)." },
            "achieved": { "$ref": "#/components/schemas/AchievedSync" }
        }
    })
}

fn device_status_schema() -> Value {
    json!({
        "type": "object",
        "description": concat!(
            "Data body of `device.status` (managed-devices.md §2.1): the conflated ",
            "latest-wins per-device runtime snapshot, envelope `id` = device id. ",
            "Excluded from the lossless replay ring (a re-snapshot heals it); ",
            "staleness is surfaced via `last_seen_ts`.",
        ),
        "required": ["device_id", "state"],
        "properties": {
            "device_id": { "type": "string", "description": "The registry device id this snapshot describes." },
            "state": { "$ref": "#/components/schemas/DeviceState" },
            "mode": { "type": "string", "description": "The device's current converged mode (driver vocabulary), once known." },
            "capabilities": { "$ref": "#/components/schemas/DeviceCapabilities" },
            "streams": {
                "type": "array",
                "items": { "$ref": "#/components/schemas/DeviceStreamStatus" },
                "description": "Device-reported active streams (omitted when none / unknown)."
            },
            "sync": { "$ref": "#/components/schemas/DeviceSyncSummary" },
            "temperature_c": { "type": "number", "format": "float", "description": "Device-reported temperature (°C), where exposed." },
            "last_seen_ts": { "type": "integer", "format": "int64", "description": "When the device last answered (engine monotonic nanoseconds)." }
        }
    })
}

fn device_adopted_schema() -> Value {
    json!({
        "type": "object",
        "description": "Data body of `device.adopted`: a device was adopted into the registry.",
        "required": ["device_id", "driver"],
        "properties": {
            "device_id": { "type": "string", "description": "The registry device id." },
            "driver": { "type": "string", "description": "The compiled-in driver managing it (e.g. `zowietek`)." },
            "name": { "type": "string", "description": "The operator-facing display name, if set." }
        }
    })
}

fn device_removed_schema() -> Value {
    json!({
        "type": "object",
        "description": "Data body of `device.removed`: a device was removed from the registry.",
        "required": ["device_id"],
        "properties": {
            "device_id": { "type": "string", "description": "The registry device id that was removed." }
        }
    })
}

fn cast_session_started_schema() -> Value {
    json!({
        "type": "object",
        "description": concat!(
            "Data body of `cast.session.started`: an ephemeral cast session was started ",
            "(session-list membership changed). The session's live state rides the ",
            "conflated `device.status` lane keyed by the same session id.",
        ),
        "required": ["session_id", "address", "output"],
        "properties": {
            "session_id": { "type": "string", "description": "The runtime session id (`cast-session-…`, UUID-fresh per start)." },
            "name": { "type": "string", "description": "The operator-facing name, if given." },
            "address": { "type": "string", "description": "The device authority dialled (`host[:port]`, IPv6 bracketed)." },
            "output": { "type": "string", "description": "The output id whose HLS rendition the session casts." }
        }
    })
}

fn cast_session_removed_schema() -> Value {
    json!({
        "type": "object",
        "description": concat!(
            "Data body of `cast.session.removed`: an ephemeral cast session was removed — ",
            "stopped (the receiver STOP that clears the TV) or promoted to a saved device ",
            "(playback continues under the promoted device id).",
        ),
        "required": ["session_id"],
        "properties": {
            "session_id": { "type": "string", "description": "The runtime session id that was removed." }
        }
    })
}

fn impact_class_schema() -> Value {
    json!({
        "type": "string",
        "description": concat!(
            "Declared impact class of a management change: cp (control-plane only), ",
            "c1 (hot/seamless at a frame boundary), c2 (controlled reset via ",
            "make-before-break), dev (the DEVICE pipeline restarts; Multiview ",
            "program output is unaffected).",
        ),
        "enum": ["cp", "c1", "c2", "dev"]
    })
}

fn device_mode_schema() -> Value {
    json!({
        "type": "object",
        "description": concat!(
            "Data body of `device.mode`: a mode convergence started/finished/failed, ",
            "carrying the impact declared BEFORE apply (instant-apply doctrine).",
        ),
        "required": ["device_id", "mode", "phase", "impact"],
        "properties": {
            "device_id": { "type": "string", "description": "The registry device id converging." },
            "mode": { "type": "string", "description": "The target mode being converged to (driver vocabulary)." },
            "phase": {
                "type": "string",
                "enum": ["started", "finished", "failed"],
                "description": "Which convergence phase this event reports."
            },
            "impact": { "$ref": "#/components/schemas/ImpactClass" },
            "detail": { "type": "string", "description": "The human-readable declared-impact statement, if the driver provides one." }
        }
    })
}

fn device_error_schema() -> Value {
    json!({
        "type": "object",
        "description": "Data body of `device.error`: a driver-reported device error.",
        "required": ["device_id", "message"],
        "properties": {
            "device_id": { "type": "string", "description": "The registry device id the error concerns." },
            "code": { "type": "string", "description": "Short machine-readable code where the driver has one (vendor codes pass through verbatim)." },
            "message": { "type": "string", "description": "Human-readable description of the error." }
        }
    })
}

fn sync_change_schema() -> Value {
    json!({
        "type": "object",
        "description": "What changed about a device's sync participation (tagged by `kind`, never untagged).",
        "required": ["kind"],
        // AsyncAPI 3.0 Schema Object: `discriminator` is the property NAME.
        "discriminator": "kind",
        "oneOf": [
            {
                "type": "object",
                "required": ["kind", "offset_ms"],
                "properties": {
                    "kind": { "type": "string", "const": "joined" },
                    "offset_ms": { "type": "integer", "format": "int64", "description": "The per-member offset trim (ms)." }
                }
            },
            {
                "type": "object",
                "required": ["kind"],
                "properties": {
                    "kind": { "type": "string", "const": "left" }
                }
            },
            {
                "type": "object",
                "required": ["kind", "achieved"],
                "properties": {
                    "kind": { "type": "string", "const": "tier" },
                    "achieved": { "$ref": "#/components/schemas/AchievedSync" }
                }
            },
            {
                "type": "object",
                "required": ["kind", "measured_skew_ms", "target_skew_ms", "exceeded"],
                "properties": {
                    "kind": { "type": "string", "const": "drift" },
                    "measured_skew_ms": { "type": "number", "format": "float", "description": "The measured skew for this member (ms)." },
                    "target_skew_ms": { "type": "integer", "format": "uint32", "description": "The group's configured skew target (ms)." },
                    "exceeded": { "type": "boolean", "description": "true = crossed above the target; false = recovered back inside it." }
                }
            }
        ]
    })
}

fn device_sync_schema() -> Value {
    json!({
        "type": "object",
        "description": "Data body of `device.sync`: sync-group membership / achieved-tier / drift change.",
        "required": ["device_id", "group", "change"],
        "properties": {
            "device_id": { "type": "string", "description": "The member device id." },
            "group": { "type": "string", "description": "The sync group concerned." },
            "change": { "$ref": "#/components/schemas/SyncChange" }
        }
    })
}

fn device_discovered_schema() -> Value {
    json!({
        "type": "object",
        "description": concat!(
            "Data body of `device.discovered`: one untrusted discovery-inventory row ",
            "streamed while a scan runs (correlated via the envelope `corr`). ",
            "Requires explicit confirm-adopt; never auto-ingested.",
        ),
        "required": ["driver", "address", "family"],
        "properties": {
            "driver": { "type": "string", "description": "The candidate driver that recognised the device." },
            "address": { "type": "string", "description": "The management endpoint (URL/host; IPv6 literals bracketed)." },
            "family": {
                "type": "string",
                "enum": ["ipv6", "ipv4-legacy"],
                "description": "Address family — IPv6-first; IPv4 results are labelled legacy."
            },
            "name": { "type": "string", "description": "The advertised device name, if any." },
            "domain": {
                "type": "string",
                "description": concat!(
                    "The discovery domain the observing node stamped on this row (ADR-W026), ",
                    "sourced solely from that node's operator-declared `[discovery] domain` ",
                    "config — never from the responder payload or TXT records. Absent when the ",
                    "observing node declared no domain; a discovery-scoped principal is then ",
                    "denied the row (fail-closed).",
                )
            }
        }
    })
}

fn wall_clock_ref_schema() -> Value {
    json!({
        "type": "object",
        "description": concat!(
            "The exact affine media↔wall map (multiview_core::wallclock::WallClockRef, ",
            "ADR-0038): wall(pts) = wall_at_anchor_ns + rescale(pts − media_at_anchor). ",
            "Integer/rational arithmetic only — never float.",
        ),
        "required": ["wall_at_anchor_ns", "media_at_anchor", "rate"],
        "properties": {
            "wall_at_anchor_ns": { "type": "integer", "format": "int64", "description": "Wall-clock instant (ns past the Unix epoch) at the anchor sample." },
            "media_at_anchor": { "type": "integer", "format": "int64", "description": "Media PTS of the anchor sample, in units of `rate`." },
            "rate": {
                "type": "object",
                "description": "The media rate (ticks per second) as an exact rational.",
                "required": ["num", "den"],
                "properties": {
                    "num": { "type": "integer", "format": "int64", "description": "Numerator." },
                    "den": { "type": "integer", "format": "int64", "description": "Denominator." }
                }
            }
        }
    })
}

fn sync_group_skew_schema() -> Value {
    json!({
        "type": "object",
        "description": "One sync group's MEASURED skew/tier (achieved tier = weakest member, never over-claimed).",
        "required": ["group", "achieved"],
        "properties": {
            "group": { "type": "string", "description": "The sync group measured." },
            "achieved": { "$ref": "#/components/schemas/AchievedSync" },
            "measured_skew_ms": { "type": "number", "format": "float", "description": "The worst measured member skew (ms), where a measurement exists." }
        }
    })
}

fn timing_status_schema() -> Value {
    json!({
        "type": "object",
        "description": concat!(
            "Data body of `timing.status` (ADR-M010): the outbound presentation epoch ",
            "plus per-sync-group achieved skew, envelope `id` = program or sync-group ",
            "id. Latest-wins and ring-excluded: the affine epoch stays valid when ",
            "stale, so receivers free-run on a missed update — they never stall.",
        ),
        "required": ["stream_id", "epoch", "link_offset_ns", "clock_source", "clock_quality"],
        "properties": {
            "stream_id": { "type": "string", "description": "The program/output stream this epoch maps." },
            "epoch": { "$ref": "#/components/schemas/WallClockRef" },
            "link_offset_ns": { "type": "integer", "format": "int64", "description": "Fixed receiver-side presentation delay (ns, AES67 link-offset semantics): uniformity is the goal, not smallness." },
            "clock_source": {
                "type": "string",
                "enum": ["ptp", "system"],
                "description": "What disciplines the wall estimate (ST 2059-2 PTP servo or chrony-disciplined system time). The clock labels the timeline; it never paces the tick loop."
            },
            "clock_quality": {
                "type": "string",
                "enum": ["locked", "holdover", "acquiring", "freerun"],
                "description": "The discipline quality of that clock (the engine servo's lock-state lifecycle)."
            },
            "groups": {
                "type": "array",
                "items": { "$ref": "#/components/schemas/SyncGroupSkew" },
                "description": "Per-sync-group measured skew/tier (omitted when no groups exist)."
            }
        }
    })
}

fn hello_schema() -> Value {
    json!({
        "type": "object",
        "description": "Data body of `$hello`: the first server frame after auth.",
        "required": [
            "session_id", "server_v", "heartbeat_ms",
            "min_rate_hz", "max_rate_hz", "default_rate_hz", "replay_ring"
        ],
        "properties": {
            "session_id": { "type": "string", "description": "Server-assigned session id." },
            "server_v": {
                "type": "array",
                "items": { "type": "integer", "format": "uint16" },
                "description": "Envelope schema majors this server can speak."
            },
            "heartbeat_ms": {
                "type": "integer",
                "format": "uint32",
                "description": "Heartbeat interval (milliseconds)."
            },
            "min_rate_hz": {
                "type": "integer",
                "format": "uint32",
                "description": "Minimum clamped wire cadence (Hz)."
            },
            "max_rate_hz": {
                "type": "integer",
                "format": "uint32",
                "description": "Maximum clamped wire cadence (Hz)."
            },
            "default_rate_hz": {
                "type": "integer",
                "format": "uint32",
                "description": "Default wire cadence when `rate_hz` is omitted from `$subscribe`."
            },
            "replay_ring": {
                "type": "integer",
                "format": "uint32",
                "description": "Replay ring size (frames per session/topic)."
            }
        }
    })
}

fn subscribe_schema() -> Value {
    json!({
        "type": "object",
        "description": "Data body of `$subscribe` (client→server): subscribe to topics.",
        "required": ["topics"],
        "properties": {
            "topics": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Topics to subscribe to."
            },
            "ids": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Optional resource-id allowlist."
            },
            "rate_hz": {
                "type": "integer",
                "format": "uint32",
                "description": "Optional max cadence (Hz); server clamps and reports effective."
            },
            "since_seq": {
                "type": "integer",
                "format": "uint64",
                "description": "Optional resume cursor: subscribe + replay from after this seq."
            }
        }
    })
}

fn subscribed_schema() -> Value {
    json!({
        "type": "object",
        "description": "Data body of `$subscribed`: per-topic ack before the snapshot.",
        "required": ["topic", "effective_rate_hz", "snapshot_seq"],
        "properties": {
            "topic": { "type": "string", "description": "The topic that was subscribed." },
            "effective_rate_hz": {
                "type": "integer",
                "format": "uint32",
                "description": "Actual cadence after server clamping."
            },
            "snapshot_seq": {
                "type": "integer",
                "format": "uint64",
                "description": "The `seq` the forthcoming snapshot is current as of."
            }
        }
    })
}

fn unsubscribe_schema() -> Value {
    json!({
        "type": "object",
        "description": "Data body of `$unsubscribe` (client→server): stop receiving topics.",
        "required": ["topics"],
        "properties": {
            "topics": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Topics to stop receiving."
            }
        }
    })
}

fn set_rate_schema() -> Value {
    json!({
        "type": "object",
        "description": "Data body of `$set_rate` (client→server): change a topic's wire cadence.",
        "required": ["topic", "rate_hz"],
        "properties": {
            "topic": { "type": "string", "description": "Topic whose cadence is changing." },
            "rate_hz": {
                "type": "integer",
                "format": "uint32",
                "description": "Requested cadence (Hz); server clamps to [min, max]."
            }
        }
    })
}

fn resume_schema() -> Value {
    json!({
        "type": "object",
        "description": "Data body of `$resume` (client→server): present last-seen cursor on reconnect.",
        "required": ["session_id", "last_seq"],
        "properties": {
            "session_id": { "type": "string", "description": "Session to resume." },
            "last_seq": {
                "type": "integer",
                "format": "uint64",
                "description": "Last `seq` the client successfully observed."
            }
        }
    })
}

fn resync_reason_schema() -> Value {
    json!({
        "type": "string",
        "description": "Why a $resync was issued (the client must rebuild, not merge). The first three are unrecoverable resume gaps; `authz_changed` is a mid-session authorization change (object scope narrowed/widened) on an intact connection (ADR-RT010).",
        "enum": ["seq_evicted", "unknown_session", "session_expired", "authz_changed"]
    })
}

fn resync_schema() -> Value {
    json!({
        "type": "object",
        "description": concat!(
            "Data body of `$resync`: the gap is unrecoverable; the client MUST rebuild state ",
            "from the fresh snapshot that follows.",
        ),
        "required": ["reason", "resubscribe"],
        "properties": {
            "reason": { "$ref": "#/components/schemas/ResyncReason" },
            "resubscribe": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Topics the client must rebuild."
            }
        }
    })
}

fn lag_action_schema() -> Value {
    json!({
        "type": "string",
        "description": "What the server did with the dropped frames.",
        "enum": ["conflated", "resnapshot"]
    })
}

fn lag_schema() -> Value {
    json!({
        "type": "object",
        "description": "Data body of `$lag`: this connection's queue overflowed for a topic.",
        "required": ["topic", "dropped_n", "action"],
        "properties": {
            "topic": { "type": "string", "description": "Topic whose frames were dropped." },
            "dropped_n": {
                "type": "integer",
                "format": "uint64",
                "description": "Number of frames dropped."
            },
            "action": { "$ref": "#/components/schemas/LagAction" }
        }
    })
}

fn protocol_error_schema() -> Value {
    json!({
        "type": "object",
        "description": "Data body of `$error`: a control-plane error.",
        "required": ["code", "message"],
        "properties": {
            "code": { "type": "string", "description": "Short stable machine-readable error code." },
            "message": { "type": "string", "description": "Human-readable description." }
        }
    })
}
