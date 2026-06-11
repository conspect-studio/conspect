//! An AMWA **NMOS IS-07** event & tally model: the IP-native equivalent of
//! GPI/GPO for tally and Boolean events (broadcast-multiviewer brief §2).
//!
//! IS-07 ("Event & Tally Specification") carries typed events between a *source*
//! and *receivers*. The **transport-agnostic message model** is pure JSON and is
//! defined here in full so it is exhaustively unit-testable with no sockets:
//!
//! * an [`Is07Message`] is either a `state` message (the current value of an
//!   event, with an [`Is07Timing`] block) or a `health` heartbeat;
//! * the value is an [`Is07EventType`]-discriminated [`Is07Payload`]
//!   (`boolean` / `number` / `string`), matching the IS-07 type vocabulary;
//! * a receiver subscribes to a set of source ids via an [`Is07Subscription`]
//!   command (the WebSocket subscription request).
//!
//! The pure model maps both ways to Multiview's own vocabulary: a `boolean` IS-07
//! event is a **GPI bit** ([`GpiEvent`]), and a tally lamp resolves to an IS-07
//! `number` (the 0/1/2/3 TSL palette code) so an external multiviewer can light
//! a lamp from Multiview's arbitrated state.
//!
//! ## Transports
//!
//! IS-07 is carried over **WebSocket** (the primary, always-available transport
//! — the control plane already runs a WS server, so the WS binding reuses the
//! existing realtime fan-out) and optionally **MQTT**. The MQTT transport pulls
//! a broker client (a native dependency) and is therefore behind the
//! **off-by-default `is07-mqtt` feature**; the pure message/codec model above is
//! always compiled and always tested. With the feature off, Multiview still speaks
//! IS-07 over WebSocket.
use multiview_core::tally::TallyColor;
use multiview_events::{TallyEvent, TallyTarget};
use serde::{Deserialize, Serialize};

/// The IS-07 event value type (the `payload.type` vocabulary).
///
/// Serialised **tagged** is unnecessary here — this is a unit discriminator
/// rendered as its lowercase wire string. `#[non_exhaustive]` so the `object`
/// composite type can be added later without a breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Is07EventType {
    /// A Boolean (on/off) event — the GPI/tally lamp primitive.
    Boolean,
    /// A numeric event (e.g. a tally palette code or a measured value).
    Number,
    /// A string event (e.g. a UMD label).
    String,
}

/// The typed value of an IS-07 event.
///
/// Internally tagged on `type` (the IS-07 `payload.type`), never `untagged`
/// (repo conventions). `#[non_exhaustive]` so further value types can be added.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Is07Payload {
    /// A Boolean value.
    Boolean {
        /// The Boolean value.
        value: bool,
    },
    /// A numeric value.
    Number {
        /// The numeric value.
        value: i64,
        /// The scaling factor (IS-07 numbers are `value`/`scale`); `1` for an
        /// integer such as a tally palette code.
        #[serde(default = "one")]
        scale: i64,
    },
    /// A string value.
    String {
        /// The string value.
        value: String,
    },
}

/// The default IS-07 numeric scale (`1`).
const fn one() -> i64 {
    1
}

impl Is07Payload {
    /// The IS-07 event type of this payload.
    #[must_use]
    pub const fn event_type(&self) -> Is07EventType {
        match self {
            Self::Boolean { .. } => Is07EventType::Boolean,
            Self::Number { .. } => Is07EventType::Number,
            Self::String { .. } => Is07EventType::String,
        }
    }
}

/// The IS-07 timing block carried on a `state` message.
///
/// Both timestamps are TAI in the IS-07 `<seconds>:<nanoseconds>` string form.
/// The model carries them verbatim; the control plane stamps them from its own
/// clock at the transport boundary (never from an input PTS — invariant #1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Is07Timing {
    /// When the event was created (`creation_timestamp`).
    pub creation_timestamp: String,
    /// When the originating condition occurred (`origin_timestamp`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_timestamp: Option<String>,
}

/// The IS-07 message kind (`message_type`): a state update or a health heartbeat.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(tag = "message_type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Is07Message {
    /// The current value of an event source.
    State {
        /// The originating source id (NMOS UUID, opaque here).
        source_id: String,
        /// The flow id the event belongs to (opaque here).
        flow_id: String,
        /// The typed event value.
        payload: Is07Payload,
        /// The IS-07 timing block.
        timing: Is07Timing,
    },
    /// A heartbeat keeping the connection alive (carries the last-seen health
    /// timestamp).
    Health {
        /// The originating source id.
        source_id: String,
        /// The health heartbeat timestamp (TAI string).
        heartbeat: String,
    },
}

impl Is07Message {
    /// The IS-07 source id this message is about.
    #[must_use]
    pub fn source_id(&self) -> &str {
        match self {
            Self::State { source_id, .. } | Self::Health { source_id, .. } => source_id,
        }
    }
}

/// An IS-07 receiver subscription command (the WebSocket subscription request).
///
/// A receiver asks to receive state for a set of source ids over the WebSocket
/// connection; the server then streams [`Is07Message::State`] for each.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Is07Subscription {
    /// The IS-07 command (always `subscription` for this request).
    pub command: Is07Command,
    /// The source ids to subscribe to.
    pub sources: Vec<String>,
}

/// The IS-07 WebSocket command vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Is07Command {
    /// Subscribe to a set of source ids.
    Subscription,
    /// A health heartbeat command.
    Health,
}

/// A general-purpose **interface** event: a Boolean GPI/GPO bit.
///
/// This is Multiview's own vocabulary for a Boolean tally/contact-closure event,
/// independent of IS-07's wire form. The IS-07 model converts to/from it so the
/// rest of the control plane (and the realtime stream) deals in [`GpiEvent`],
/// not raw IS-07 JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct GpiEvent {
    /// The logical GPI line id this event is for.
    pub line: String,
    /// Whether the contact is closed / the bit is asserted.
    pub asserted: bool,
}

impl GpiEvent {
    /// Build a [`GpiEvent`] from an IS-07 `state` message carrying a Boolean
    /// payload, using the message's `source_id` as the GPI line id.
    ///
    /// Returns [`None`] for a non-`state` message or a non-Boolean payload.
    #[must_use]
    pub fn from_is07(message: &Is07Message) -> Option<Self> {
        match message {
            Is07Message::State {
                source_id, payload, ..
            } => match payload {
                Is07Payload::Boolean { value } => Some(Self {
                    line: source_id.clone(),
                    asserted: *value,
                }),
                _ => None,
            },
            Is07Message::Health { .. } => None,
        }
    }

    /// Render this GPI event as an IS-07 Boolean `state` message for emission to
    /// subscribed receivers, stamped with `creation_timestamp`.
    #[must_use]
    pub fn to_is07(&self, creation_timestamp: impl Into<String>) -> Is07Message {
        Is07Message::State {
            source_id: self.line.clone(),
            flow_id: self.line.clone(),
            payload: Is07Payload::Boolean {
                value: self.asserted,
            },
            timing: Is07Timing {
                creation_timestamp: creation_timestamp.into(),
                origin_timestamp: None,
            },
        }
    }
}

/// Map a [`TallyColor`] to its IS-07 numeric palette code (the TSL 0/1/2/3
/// vocabulary: off/red/green/amber).
#[must_use]
pub fn tally_color_to_is07(
    color: TallyColor,
    creation_timestamp: impl Into<String>,
) -> Is07Message {
    Is07Message::State {
        source_id: "tally".to_owned(),
        flow_id: "tally".to_owned(),
        payload: Is07Payload::Number {
            value: i64::from(color.tsl_code()),
            scale: 1,
        },
        timing: Is07Timing {
            creation_timestamp: creation_timestamp.into(),
            origin_timestamp: None,
        },
    }
}

/// Render a resolved [`TallyEvent`] as an IS-07 `number` `state` message keyed by
/// the target, so an external IS-07 receiver can light a lamp from Multiview's
/// arbitrated tally state.
#[must_use]
pub fn tally_event_to_is07(
    event: &TallyEvent,
    creation_timestamp: impl Into<String>,
) -> Is07Message {
    let source_id = match &event.target {
        TallyTarget::Tile { index } => format!("tile:{index}"),
        TallyTarget::Element { name } => format!("element:{name}"),
        _ => "tally".to_owned(),
    };
    let flow_id = source_id.clone();
    Is07Message::State {
        source_id,
        flow_id,
        payload: Is07Payload::Number {
            value: i64::from(event.state.color.tsl_code()),
            scale: 1,
        },
        timing: Is07Timing {
            creation_timestamp: creation_timestamp.into(),
            origin_timestamp: None,
        },
    }
}

/// Recover a [`TallyState`](multiview_core::tally::TallyState)'s lamp colour from an
/// IS-07 `number` `state` message carrying a TSL palette code (`0..=3`).
///
/// Returns [`None`] for a non-`state` message, a non-`number` payload, or a
/// numeric value outside the TSL palette range.
#[must_use]
pub fn tally_color_from_is07(message: &Is07Message) -> Option<TallyColor> {
    match message {
        Is07Message::State { payload, .. } => match payload {
            Is07Payload::Number { value, scale } => {
                if *scale != 1 {
                    return None;
                }
                let code = u8::try_from(*value).ok()?;
                TallyColor::from_tsl_code(code)
            }
            _ => None,
        },
        Is07Message::Health { .. } => None,
    }
}

pub mod mqtt {
    //! MQTT transport bindings for IS-07.
    //!
    //! IS-07 may be carried over MQTT instead of (or alongside) WebSocket. The
    //! **pure parts** — the NMOS topic convention, the JSON wire codec, the
    //! [`Qos`] vocabulary, and the bounded drop-oldest [`PublishQueue`] that
    //! isolates the engine from a slow broker — are **always compiled and always
    //! tested**, so Multiview can model and serialise IS-07/MQTT messages with no
    //! native deps.
    //!
    //! The **live broker client** ([`MqttPublisher`]/[`MqttSubscriber`], built on
    //! `rumqttc`) is a native async dependency and therefore lives behind the
    //! off-by-default `is07-mqtt` feature; with the feature off Multiview still
    //! speaks IS-07 over WebSocket and can still model the MQTT wire form.
    //!
    //! **Isolation (invariant #10).** A live publisher drains the bounded
    //! [`PublishQueue`] on a detached task and `try_publish`es to the broker; the
    //! engine-facing [`Publisher::try_publish`] never blocks and never awaits the
    //! broker — when the queue is full it **drops the oldest** message (the
    //! conflation posture of the realtime fan-out, `realtime-api.md §8`), so a
    //! dead or slow broker can never back-pressure the engine.
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    use super::Is07Message;

    /// The NMOS MQTT topic prefix for IS-07 source events.
    const TOPIC_PREFIX: &str = "x-nmos/events/v1.0/sources";

    /// The wildcard topic filter a receiver subscribes to in order to receive
    /// every IS-07 source's events (the single-level `+` matches any source id).
    pub const SUBSCRIBE_TOPIC_FILTER: &str = "x-nmos/events/v1.0/sources/+";

    /// The MQTT topic an IS-07 message for `source_id` is published on, following
    /// the NMOS convention `x-nmos/events/v1.0/sources/<source_id>`.
    #[must_use]
    pub fn topic_for_source(source_id: &str) -> String {
        format!("{TOPIC_PREFIX}/{source_id}")
    }

    /// The MQTT topic for a message, derived from its source id.
    #[must_use]
    pub fn topic_for_message(message: &Is07Message) -> String {
        topic_for_source(message.source_id())
    }

    /// MQTT delivery quality-of-service for an IS-07 publish/subscribe.
    ///
    /// The wire codes match the MQTT spec (`0`/`1`/`2`). NMOS guidance for IS-07
    /// state delivery is **at-least-once** (the default) so a receiver does not
    /// miss a tally change; a high-rate informational event may opt down to
    /// at-most-once.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    #[non_exhaustive]
    pub enum Qos {
        /// Fire-and-forget (`0`): no broker acknowledgement.
        AtMostOnce,
        /// At-least-once (`1`): acknowledged, may be duplicated. The default for
        /// IS-07 state messages.
        #[default]
        AtLeastOnce,
        /// Exactly-once (`2`): the four-way handshake.
        ExactlyOnce,
    }

    impl Qos {
        /// The MQTT wire code for this [`Qos`] (`0`/`1`/`2`).
        #[must_use]
        pub const fn code(self) -> u8 {
            match self {
                Self::AtMostOnce => 0,
                Self::AtLeastOnce => 1,
                Self::ExactlyOnce => 2,
            }
        }
    }

    /// A failure decoding an MQTT payload back into an [`Is07Message`].
    #[derive(Debug, thiserror::Error)]
    #[non_exhaustive]
    pub enum MqttDecodeError {
        /// The payload was not the expected IS-07 JSON.
        #[error("invalid IS-07 MQTT payload: {0}")]
        Json(#[from] serde_json::Error),
    }

    /// Serialise an [`Is07Message`] to its MQTT payload bytes (compact IS-07
    /// JSON, the tagged wire model — never `untagged`).
    ///
    /// # Errors
    ///
    /// [`serde_json::Error`] if the message cannot be serialised (it always can
    /// for the current model; the result type keeps the seam total).
    pub fn encode(message: &Is07Message) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(message)
    }

    /// Decode an MQTT payload back into an [`Is07Message`].
    ///
    /// # Errors
    ///
    /// [`MqttDecodeError::Json`] if `bytes` is not valid IS-07 JSON. A decode
    /// failure is a *dropped frame*, never a panic — the caller degrades.
    pub fn decode(bytes: &[u8]) -> Result<Is07Message, MqttDecodeError> {
        Ok(serde_json::from_slice(bytes)?)
    }

    /// The shared, bounded, drop-oldest buffer behind a [`Publisher`].
    ///
    /// Holds at most `capacity` messages; the newest always wins. This is the
    /// load-bearing isolation primitive (invariant #10): the producer never
    /// blocks, so a stalled drainer (a dead broker) cannot pace the engine.
    #[derive(Debug)]
    struct Shared {
        queue: Mutex<VecDeque<Is07Message>>,
        capacity: usize,
        dropped: AtomicU64,
    }

    /// The engine-facing handle to a bounded drop-oldest publish queue.
    ///
    /// [`Publisher::try_publish`] is non-blocking and infallible from the
    /// engine's perspective: it either enqueues the message or drops the oldest
    /// to make room, **never** awaiting a consumer.
    #[derive(Debug, Clone)]
    pub struct Publisher {
        shared: Arc<Shared>,
    }

    /// The consumer side of a [`PublishQueue`]: the live broker drainer reads
    /// messages from here and forwards them to the broker.
    #[derive(Debug)]
    pub struct PublishReceiver {
        shared: Arc<Shared>,
    }

    /// A bounded, drop-oldest publish channel for IS-07 MQTT messages.
    #[derive(Debug)]
    pub struct PublishQueue;

    impl PublishQueue {
        /// Create a bounded drop-oldest queue with the given capacity, returning
        /// the producer ([`Publisher`]) and consumer ([`PublishReceiver`]) ends.
        ///
        /// A `capacity` of `0` is treated as `1` so the queue can always hold the
        /// single newest message.
        #[must_use]
        pub fn bounded(capacity: usize) -> (Publisher, PublishReceiver) {
            let capacity = capacity.max(1);
            let shared = Arc::new(Shared {
                queue: Mutex::new(VecDeque::with_capacity(capacity)),
                capacity,
                dropped: AtomicU64::new(0),
            });
            (
                Publisher {
                    shared: Arc::clone(&shared),
                },
                PublishReceiver { shared },
            )
        }
    }

    impl Publisher {
        /// Enqueue a message without ever blocking.
        ///
        /// Returns `true` if the message was enqueued with room to spare, or
        /// `false` if the queue was full and the **oldest** message was dropped
        /// to admit this one (the dropped-count is also incremented). Either way
        /// the newest message is retained — the engine is never paced.
        ///
        // The bool is an *advisory* drop indicator: the engine fire-and-forgets
        // (it reads aggregate drops via `dropped()` for telemetry), so the
        // result is deliberately ignorable — `#[must_use]` would mislead callers.
        #[allow(clippy::must_use_candidate)]
        pub fn try_publish(&self, message: Is07Message) -> bool {
            // A poisoned lock means a drainer panicked mid-pop; recover the guard
            // and keep serving rather than propagating a panic onto the engine.
            let mut queue = match self.shared.queue.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            if queue.len() >= self.shared.capacity {
                queue.pop_front();
                self.shared.dropped.fetch_add(1, Ordering::Relaxed);
                queue.push_back(message);
                false
            } else {
                queue.push_back(message);
                true
            }
        }

        /// The total number of messages dropped-oldest so far (for telemetry /
        /// the `$lag`-style conflation marker).
        #[must_use]
        pub fn dropped(&self) -> u64 {
            self.shared.dropped.load(Ordering::Relaxed)
        }
    }

    impl PublishReceiver {
        /// Pop the oldest queued message, or [`None`] if the queue is empty.
        #[must_use]
        pub fn try_recv(&mut self) -> Option<Is07Message> {
            let mut queue = match self.shared.queue.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            queue.pop_front()
        }
    }

    #[cfg(feature = "is07-mqtt")]
    pub use live::{BrokerConfig, MqttPublisher, MqttSubscriber, MqttTransportError};

    #[cfg(feature = "is07-mqtt")]
    mod live {
        //! The live MQTT broker client (off-by-default `is07-mqtt` feature).
        //!
        //! `rumqttc`'s `AsyncClient` owns its own bounded request channel and a
        //! detached `EventLoop` that must be polled to make progress. The
        //! [`MqttPublisher`] spawns a task that drains the pure [`PublishQueue`]
        //! and `try_publish`es each message to the broker, plus a task that polls
        //! the event loop — so the engine-facing [`super::Publisher::try_publish`]
        //! never touches the socket. The [`MqttSubscriber`] subscribes to the
        //! NMOS wildcard and decodes each inbound frame back into an
        //! [`Is07Message`], degrading a bad frame to a drop, never a panic.
        use std::time::Duration;

        use rumqttc::{AsyncClient, Event, Incoming, MqttOptions, QoS};
        use tokio::sync::mpsc;

        use super::super::Is07Message;
        use super::{
            decode, topic_for_message, PublishQueue, Publisher, Qos, SUBSCRIBE_TOPIC_FILTER,
        };

        /// How a [`MqttPublisher`]/[`MqttSubscriber`] reaches the broker.
        #[derive(Debug, Clone)]
        #[non_exhaustive]
        pub struct BrokerConfig {
            /// The broker host (DNS name or IP).
            pub host: String,
            /// The broker port (plain MQTT is conventionally `1883`).
            pub port: u16,
            /// The MQTT client id this connection registers as.
            pub client_id: String,
            /// The [`Qos`] used for publishes and the subscription.
            pub qos: Qos,
            /// The bound on the in-flight drop-oldest queue / inbound channel.
            pub capacity: usize,
        }

        impl BrokerConfig {
            /// A broker config for `host:port` registering as `client_id`, with
            /// the default at-least-once [`Qos`] and a 16-deep in-flight queue.
            #[must_use]
            pub fn new(host: impl Into<String>, port: u16, client_id: impl Into<String>) -> Self {
                Self {
                    host: host.into(),
                    port,
                    client_id: client_id.into(),
                    qos: Qos::AtLeastOnce,
                    capacity: 16,
                }
            }

            /// Override the publish/subscribe [`Qos`].
            #[must_use]
            pub const fn with_qos(mut self, qos: Qos) -> Self {
                self.qos = qos;
                self
            }

            /// Override the in-flight drop-oldest queue / inbound channel bound.
            #[must_use]
            pub const fn with_capacity(mut self, capacity: usize) -> Self {
                self.capacity = capacity;
                self
            }

            /// Map the NMOS [`Qos`] to the `rumqttc` wire `QoS`.
            const fn rumqttc_qos(&self) -> QoS {
                match self.qos {
                    Qos::AtMostOnce => QoS::AtMostOnce,
                    Qos::AtLeastOnce => QoS::AtLeastOnce,
                    Qos::ExactlyOnce => QoS::ExactlyOnce,
                }
            }

            /// Build the `rumqttc` options for this config.
            fn options(&self) -> MqttOptions {
                let mut opts =
                    MqttOptions::new(self.client_id.clone(), self.host.clone(), self.port);
                opts.set_keep_alive(Duration::from_secs(15));
                opts
            }

            /// The capacity used for the in-flight queue / inbound channel (never
            /// zero, so the channel can always hold the single newest message).
            const fn cap(&self) -> usize {
                if self.capacity == 0 {
                    1
                } else {
                    self.capacity
                }
            }
        }

        /// A failure establishing or running a live MQTT transport.
        #[derive(Debug, thiserror::Error)]
        #[non_exhaustive]
        pub enum MqttTransportError {
            /// The `rumqttc` client rejected a request (e.g. an invalid topic).
            #[error("MQTT client error: {0}")]
            Client(#[from] rumqttc::ClientError),
        }

        /// A live IS-07 MQTT publisher.
        ///
        /// The engine pushes messages via [`Self::try_publish`] (which forwards to
        /// the bounded drop-oldest [`super::Publisher`]); a detached task drains
        /// the queue to the broker. Dropping the publisher stops the drain task.
        #[derive(Debug)]
        pub struct MqttPublisher {
            producer: Publisher,
            qos: QoS,
            drain: tokio::task::JoinHandle<()>,
            poll: tokio::task::JoinHandle<()>,
        }

        impl MqttPublisher {
            /// Connect to the broker and start the drain + event-loop tasks.
            ///
            /// # Errors
            ///
            /// Never returns an error today (connection is lazy in `rumqttc`); the
            /// `Result` keeps the seam total for future eager-connect variants.
            pub async fn connect(config: &BrokerConfig) -> Result<Self, MqttTransportError> {
                let (client, mut eventloop) = AsyncClient::new(config.options(), config.cap());
                let (producer, mut receiver) = PublishQueue::bounded(config.cap());
                let qos = config.rumqttc_qos();

                // Poll the event loop so the client makes progress. This task
                // owns the socket; it never signals back to the engine.
                let poll = tokio::spawn(async move {
                    loop {
                        if eventloop.poll().await.is_err() {
                            tokio::time::sleep(Duration::from_millis(200)).await;
                        }
                    }
                });

                // Drain the bounded queue to the broker. `try_publish` is
                // non-blocking; on a full client channel the message is dropped
                // (the engine is never paced). A short idle sleep avoids a busy
                // loop when the queue is empty.
                let drain_client = client;
                let drain = tokio::spawn(async move {
                    loop {
                        match receiver.try_recv() {
                            Some(message) => {
                                let topic = topic_for_message(&message);
                                if let Ok(payload) = super::encode(&message) {
                                    // Drop on any client-side failure; never block.
                                    let _ = drain_client.try_publish(topic, qos, false, payload);
                                }
                            }
                            None => tokio::time::sleep(Duration::from_millis(5)).await,
                        }
                    }
                });

                // Yield once so the runtime can schedule the just-spawned poll +
                // drain tasks before the caller starts publishing — the client
                // begins its TCP connect attempt without a publish round-trip.
                tokio::task::yield_now().await;

                Ok(Self {
                    producer,
                    qos,
                    drain,
                    poll,
                })
            }

            /// Enqueue a message for publication without blocking.
            ///
            /// Returns `true` if enqueued with room to spare, `false` if the
            /// oldest queued message was dropped to admit this one. Never awaits
            /// the broker — invariant #10.
            // Advisory drop indicator, fire-and-forget at the engine boundary —
            // see `super::Publisher::try_publish`; `#[must_use]` would mislead.
            #[allow(clippy::must_use_candidate)]
            pub fn try_publish(&self, message: Is07Message) -> bool {
                self.producer.try_publish(message)
            }

            /// The `rumqttc` wire [`QoS`](rumqttc::QoS) this publisher emits at.
            #[must_use]
            pub const fn qos(&self) -> QoS {
                self.qos
            }
        }

        impl Drop for MqttPublisher {
            fn drop(&mut self) {
                self.drain.abort();
                self.poll.abort();
            }
        }

        /// A live IS-07 MQTT subscriber.
        ///
        /// Subscribes to the NMOS wildcard and decodes each inbound frame into an
        /// [`Is07Message`], delivered over a bounded channel. A frame that fails
        /// to decode is dropped, never a panic.
        #[derive(Debug)]
        pub struct MqttSubscriber {
            inbound: mpsc::Receiver<Is07Message>,
            poll: tokio::task::JoinHandle<()>,
        }

        impl MqttSubscriber {
            /// Connect, subscribe to the IS-07 wildcard, and start decoding.
            ///
            /// # Errors
            ///
            /// [`MqttTransportError::Client`] if the subscribe request is rejected.
            pub async fn connect(config: &BrokerConfig) -> Result<Self, MqttTransportError> {
                let (client, mut eventloop) = AsyncClient::new(config.options(), config.cap());
                client
                    .subscribe(SUBSCRIBE_TOPIC_FILTER, config.rumqttc_qos())
                    .await?;
                let (tx, inbound) = mpsc::channel(config.cap());

                let poll = tokio::spawn(async move {
                    loop {
                        match eventloop.poll().await {
                            Ok(Event::Incoming(Incoming::Publish(publish))) => {
                                if let Ok(message) = decode(&publish.payload) {
                                    // Drop-oldest on a full inbound channel: a slow
                                    // reader never back-pressures the broker poll.
                                    if tx.try_send(message).is_err() {
                                        // Channel full or closed; drop this frame.
                                    }
                                }
                            }
                            Ok(_) => {}
                            Err(_) => {
                                tokio::time::sleep(Duration::from_millis(200)).await;
                            }
                        }
                    }
                });

                Ok(Self { inbound, poll })
            }

            /// Receive the next decoded inbound IS-07 message, or [`None`] if the
            /// transport has shut down.
            pub async fn recv(&mut self) -> Option<Is07Message> {
                self.inbound.recv().await
            }
        }

        impl Drop for MqttSubscriber {
            fn drop(&mut self) {
                self.poll.abort();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use multiview_core::tally::{TallyColor, TallyState};
    use multiview_events::{TallyEvent, TallyTarget};

    use super::{
        tally_color_from_is07, tally_color_to_is07, tally_event_to_is07, GpiEvent, Is07Command,
        Is07EventType, Is07Message, Is07Payload, Is07Subscription, Is07Timing,
    };

    fn timing() -> Is07Timing {
        Is07Timing {
            creation_timestamp: "1700000000:0".to_owned(),
            origin_timestamp: None,
        }
    }

    #[test]
    fn state_message_round_trips_through_json_tagged() {
        let msg = Is07Message::State {
            source_id: "src-1".to_owned(),
            flow_id: "flow-1".to_owned(),
            payload: Is07Payload::Boolean { value: true },
            timing: timing(),
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["message_type"], "state");
        assert_eq!(json["payload"]["type"], "boolean");
        assert_eq!(json["payload"]["value"], true);
        let back: Is07Message = serde_json::from_value(json).unwrap();
        assert_eq!(back, msg);
    }

    #[test]
    fn payload_event_type_matches_variant() {
        assert_eq!(
            Is07Payload::Boolean { value: false }.event_type(),
            Is07EventType::Boolean
        );
        assert_eq!(
            Is07Payload::Number { value: 3, scale: 1 }.event_type(),
            Is07EventType::Number
        );
        assert_eq!(
            Is07Payload::String {
                value: "x".to_owned()
            }
            .event_type(),
            Is07EventType::String
        );
    }

    #[test]
    fn subscription_command_serialises_to_the_wire_vocabulary() {
        let sub = Is07Subscription {
            command: Is07Command::Subscription,
            sources: vec!["a".to_owned(), "b".to_owned()],
        };
        let json = serde_json::to_value(&sub).unwrap();
        assert_eq!(json["command"], "subscription");
        assert_eq!(json["sources"][0], "a");
        let back: Is07Subscription = serde_json::from_value(json).unwrap();
        assert_eq!(back, sub);
    }

    #[test]
    fn gpi_round_trips_through_is07_boolean_state() {
        let gpi = GpiEvent {
            line: "preview-bus".to_owned(),
            asserted: true,
        };
        let msg = gpi.to_is07("1700000000:0");
        // The IS-07 message carries the Boolean and the line as the source id.
        match &msg {
            Is07Message::State {
                source_id, payload, ..
            } => {
                assert_eq!(source_id, "preview-bus");
                assert_eq!(*payload, Is07Payload::Boolean { value: true });
            }
            other => panic!("expected state, got {other:?}"),
        }
        let back = GpiEvent::from_is07(&msg).unwrap();
        assert_eq!(back, gpi);
    }

    #[test]
    fn gpi_from_is07_rejects_non_boolean_and_health() {
        let number = Is07Message::State {
            source_id: "s".to_owned(),
            flow_id: "f".to_owned(),
            payload: Is07Payload::Number { value: 1, scale: 1 },
            timing: timing(),
        };
        assert!(GpiEvent::from_is07(&number).is_none());
        let health = Is07Message::Health {
            source_id: "s".to_owned(),
            heartbeat: "1700000000:0".to_owned(),
        };
        assert!(GpiEvent::from_is07(&health).is_none());
    }

    #[test]
    fn tally_color_round_trips_through_is07_number() {
        for color in [
            TallyColor::Off,
            TallyColor::Red,
            TallyColor::Green,
            TallyColor::Amber,
        ] {
            let msg = tally_color_to_is07(color, "1700000000:0");
            assert_eq!(tally_color_from_is07(&msg), Some(color));
        }
    }

    #[test]
    fn tally_color_from_is07_rejects_out_of_palette_and_scaled() {
        let bad_code = Is07Message::State {
            source_id: "s".to_owned(),
            flow_id: "f".to_owned(),
            payload: Is07Payload::Number { value: 9, scale: 1 },
            timing: timing(),
        };
        assert_eq!(tally_color_from_is07(&bad_code), None);
        let scaled = Is07Message::State {
            source_id: "s".to_owned(),
            flow_id: "f".to_owned(),
            payload: Is07Payload::Number {
                value: 1,
                scale: 100,
            },
            timing: timing(),
        };
        assert_eq!(tally_color_from_is07(&scaled), None);
    }

    #[test]
    fn tally_event_to_is07_keys_by_target_and_carries_palette_code() {
        let event = TallyEvent {
            target: TallyTarget::Tile { index: 5 },
            state: TallyState::program(),
        };
        let msg = tally_event_to_is07(&event, "1700000000:0");
        match &msg {
            Is07Message::State {
                source_id, payload, ..
            } => {
                assert_eq!(source_id, "tile:5");
                assert_eq!(
                    *payload,
                    Is07Payload::Number {
                        value: i64::from(TallyColor::Red.tsl_code()),
                        scale: 1
                    }
                );
            }
            other => panic!("expected state, got {other:?}"),
        }
    }

    #[test]
    fn mqtt_topic_follows_the_nmos_convention() {
        use super::mqtt;
        assert_eq!(
            mqtt::topic_for_source("abc"),
            "x-nmos/events/v1.0/sources/abc"
        );
        let msg = Is07Message::Health {
            source_id: "xyz".to_owned(),
            heartbeat: "1700000000:0".to_owned(),
        };
        assert_eq!(
            mqtt::topic_for_message(&msg),
            "x-nmos/events/v1.0/sources/xyz"
        );
    }
}
