[![crates.io](https://img.shields.io/crates/v/web-transport-moq)](https://crates.io/crates/web-transport-moq)
[![docs.rs](https://img.shields.io/docsrs/web-transport-moq)](https://docs.rs/web-transport-moq)
[![discord](https://img.shields.io/discord/1124083992740761730)](https://discord.gg/FCYF3p99mr)

# web-transport-moq
A wrapper around the [moq-noq](../README.md) API (the MoQ fork of Noq, a Quinn fork), abstracting away the annoying HTTP/3 internals. The same adapter for upstream noq is [web-transport-noq](https://github.com/moq-dev/web-transport).
Provides a QUIC-like API but with web support!

## WebTransport
[WebTransport](https://developer.mozilla.org/en-US/docs/Web/API/WebTransport_API) is a new web API that allows for low-level, bidirectional communication between a client and a server.
It's [available in the browser](https://caniuse.com/webtransport) as an alternative to HTTP and WebSockets.

WebTransport is layered on top of HTTP/3 which itself is layered on top of QUIC.
This library hides that detail and exposes only the QUIC API, delegating as much as possible to the underlying QUIC implementation (Noq, a Quinn fork).

QUIC provides two primary APIs:

## Streams

QUIC streams are ordered, reliable, flow-controlled, and optionally bidirectional.
Both endpoints can create and close streams (including an error code) with no overhead.
You can think of them as TCP connections, but shared over a single QUIC connection.

## Datagrams

QUIC datagrams are unordered, unreliable, and not flow-controlled.
Both endpoints can send datagrams below the MTU size (~1.2kb minimum) and they might arrive out of order or not at all.
They are basically UDP packets, except they are encrypted and congestion controlled.

# Usage
Build a [Client](https://docs.rs/web-transport-moq/latest/web_transport_moq/struct.Client.html) or [Server](https://docs.rs/web-transport-moq/latest/web_transport_moq/struct.Server.html) with its builder, or wrap your own [noq::Endpoint](https://docs.rs/moq-noq/latest/moq_noq/struct.Endpoint.html) with `Client::new` / `Server::new`; the only requirement is that the ALPN is set to `web_transport_moq::ALPN` (aka `h3`).
Connecting or accepting takes over the QUIC connection and performs the boring HTTP/3 handshake for you.

See the [examples](examples) or [moq-tokio](https://github.com/moq-dev/moq/blob/main/rs/moq-tokio/src/noq.rs) for a full setup.

```rust
    // Create a QUIC client that trusts the system roots.
    let client = web_transport_moq::ClientBuilder::new().with_system_roots()?;

    // Connect to the given URL.
    let session = client.connect(Url::parse("https://localhost")?).await?;

    // Create a bidirectional stream.
    let (mut send, mut recv) = session.open_bi().await?;

    // Send a message.
    send.write(b"hello").await?;
```

## API
The `web-transport-moq` API is almost identical to the Noq API, except that [Connection](https://docs.rs/moq-noq/latest/moq_noq/struct.Connection.html) is called [Session](https://docs.rs/web-transport-moq/latest/web_transport_moq/struct.Session.html).

When possible, `Deref` is used to expose the underlying Noq API.
However some of the API is wrapped or unavailable due to WebTransport limitations.
- Stream IDs are not available.
- Error codes are not full VarInts (62-bits) and significantly smaller.
