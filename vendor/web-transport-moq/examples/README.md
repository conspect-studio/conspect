# Example

## Simple Echo
A simple [server](echo-server.rs) and [client](echo-client.rs).

There's also advanced examples [server](echo-server-advanced.rs) and [client](echo-client-advanced.rs) that construct the QUIC connection manually.

QUIC requires TLS, which makes the initial setup a bit more involved.

-   Generate a certificate, for example `rcgen` or `mkcert localhost`
-   Run the Rust server: `cargo run --example echo-server -- --tls-cert localhost.crt --tls-key localhost.key`
-   Run the Rust client: `cargo run --example echo-client -- --tls-cert localhost.crt`
