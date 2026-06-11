//! Integration tests for the tracing subscriber builder.
//!
//! These must NOT install a global subscriber (that would poison other tests in
//! the same process via `set_global_default` being a one-shot). They only build
//! a subscriber and exercise it locally with `tracing::subscriber::with_default`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_telemetry::tracing_init::SubscriberBuilder;

#[test]
fn builder_constructs_a_subscriber_from_a_directive() {
    // An explicit directive must parse and produce a usable subscriber.
    let subscriber = SubscriberBuilder::new()
        .with_directive("info,multiview_engine=debug")
        .build()
        .expect("valid directive must build");

    // Using it locally must not panic and must not require global install.
    tracing::subscriber::with_default(subscriber, || {
        tracing::info!(target: "multiview_engine", "hello");
    });
}

#[test]
fn invalid_directive_is_reported_as_an_error() {
    let err = SubscriberBuilder::new()
        .with_directive("this is =not= a valid directive!!!")
        .build();
    assert!(
        err.is_err(),
        "a malformed filter directive must error, not panic"
    );
}

#[test]
fn default_builder_falls_back_when_env_is_absent() {
    // With no directive and no env, a sane default level is used and the
    // subscriber builds successfully.
    let subscriber = SubscriberBuilder::new()
        .with_default_level("warn")
        .build()
        .expect("default-level subscriber must build");
    tracing::subscriber::with_default(subscriber, || {
        tracing::warn!("warn is visible at the default level");
    });
}
