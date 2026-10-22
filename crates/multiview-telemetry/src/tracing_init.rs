//! Builder for a `tracing` subscriber, configured from an `EnvFilter` directive.
//!
//! This intentionally does **not** install a global subscriber on `build()`.
//! `tracing::subscriber::set_global_default` is a one-shot per process, so a
//! library that installs it eagerly poisons tests and embedders. Instead:
//!
//! * [`SubscriberBuilder::build`] returns a `Subscriber` value the caller can
//!   use locally (e.g. `tracing::subscriber::with_default(sub, || …)` in tests)
//!   or install themselves.
//! * [`SubscriberBuilder::try_init`] is an explicit convenience that *does*
//!   install globally, returning an error if a global subscriber already exists.
//!
//! Filtering follows the standard `RUST_LOG`-style directive grammar via
//! [`tracing_subscriber::EnvFilter`]; a malformed directive is reported as a
//! [`TelemetryError::Filter`] rather than panicking.
use std::sync::Arc;

use crate::error::{Result, TelemetryError};
use crate::log_capture::{LogCaptureLayer, LogRing};
use tracing::Subscriber;
use tracing_subscriber::fmt;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

/// The concrete subscriber type produced by [`SubscriberBuilder::build`].
///
/// Boxed so the (large, layered) concrete type does not leak into the public
/// signature and so callers can hold a uniform handle. It implements
/// [`tracing::Subscriber`] and the span-lookup trait needed to use it with
/// `with_default` and global installation.
pub type BuiltSubscriber = Box<dyn Subscriber + Send + Sync + 'static>;

/// Selects where formatted log lines are written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum Output {
    /// Standard error (the default for a daemon's structured logs).
    #[default]
    Stderr,
    /// Standard output.
    Stdout,
}

/// Builds a `tracing` subscriber from an env-filter directive.
///
/// Resolution order for the filter, highest priority first:
/// 1. An explicit directive set via [`SubscriberBuilder::with_directive`].
/// 2. The `RUST_LOG` environment variable, if [`SubscriberBuilder::with_env`] is
///    enabled (the default).
/// 3. The default level from [`SubscriberBuilder::with_default_level`]
///    (defaults to `info`).
#[derive(Debug, Clone)]
pub struct SubscriberBuilder {
    directive: Option<String>,
    default_level: String,
    read_env: bool,
    output: Output,
    ansi: bool,
}

impl Default for SubscriberBuilder {
    fn default() -> Self {
        Self {
            directive: None,
            default_level: "info".to_owned(),
            read_env: true,
            output: Output::Stderr,
            ansi: false,
        }
    }
}

impl SubscriberBuilder {
    /// A builder with default settings (read `RUST_LOG`, fall back to `info`,
    /// write to stderr, no ANSI colors).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set an explicit filter directive (e.g. `"info,multiview_engine=debug"`),
    /// overriding the environment.
    #[must_use]
    pub fn with_directive(mut self, directive: impl Into<String>) -> Self {
        self.directive = Some(directive.into());
        self
    }

    /// Set the fallback level used when no explicit directive and no environment
    /// variable are present (e.g. `"warn"`).
    #[must_use]
    pub fn with_default_level(mut self, level: impl Into<String>) -> Self {
        self.default_level = level.into();
        self
    }

    /// Enable or disable reading the `RUST_LOG` environment variable.
    #[must_use]
    pub fn with_env(mut self, read_env: bool) -> Self {
        self.read_env = read_env;
        self
    }

    /// Choose the log output stream.
    #[must_use]
    pub fn with_output(mut self, output: Output) -> Self {
        self.output = output;
        self
    }

    /// Enable or disable ANSI color codes (default off, for log files/journald).
    #[must_use]
    pub fn with_ansi(mut self, ansi: bool) -> Self {
        self.ansi = ansi;
        self
    }

    /// Resolve the effective [`EnvFilter`] per the documented priority order.
    ///
    /// # Errors
    ///
    /// Returns [`TelemetryError::Filter`] if an explicit directive (or the
    /// default level) fails to parse.
    fn resolve_filter(&self) -> Result<EnvFilter> {
        if let Some(directive) = &self.directive {
            return EnvFilter::try_new(directive)
                .map_err(|e| TelemetryError::Filter(e.to_string()));
        }
        if self.read_env {
            if let Ok(filter) = EnvFilter::try_from_default_env() {
                return Ok(filter);
            }
        }
        EnvFilter::try_new(&self.default_level).map_err(|e| TelemetryError::Filter(e.to_string()))
    }

    /// Assemble a layered fmt subscriber without installing it globally.
    fn assemble(&self, filter: EnvFilter) -> BuiltSubscriber {
        let registry = tracing_subscriber::registry().with(filter);
        match self.output {
            Output::Stderr => {
                let layer = fmt::layer()
                    .with_ansi(self.ansi)
                    .with_writer(std::io::stderr);
                Box::new(registry.with(layer))
            }
            Output::Stdout => {
                let layer = fmt::layer()
                    .with_ansi(self.ansi)
                    .with_writer(std::io::stdout);
                Box::new(registry.with(layer))
            }
        }
    }

    /// Build the subscriber **without** installing it globally.
    ///
    /// The returned value can be used with
    /// `tracing::subscriber::with_default(subscriber, || …)` (e.g. in tests) or
    /// installed by the caller.
    ///
    /// # Errors
    ///
    /// Returns [`TelemetryError::Filter`] if the configured directive is invalid.
    pub fn build(&self) -> Result<BuiltSubscriber> {
        let filter = self.resolve_filter()?;
        Ok(self.assemble(filter))
    }

    /// Build the subscriber and install it as the process-global default.
    ///
    /// This is a one-shot per process. Prefer [`SubscriberBuilder::build`] in
    /// libraries and tests; use this only from a binary's `main`.
    ///
    /// # Errors
    ///
    /// Returns [`TelemetryError::Filter`] if the directive is invalid, or if a
    /// global subscriber has already been installed.
    pub fn try_init(&self) -> Result<()> {
        let filter = self.resolve_filter()?;
        let registry = tracing_subscriber::registry().with(filter);
        let result = match self.output {
            Output::Stderr => {
                let layer = fmt::layer()
                    .with_ansi(self.ansi)
                    .with_writer(std::io::stderr);
                registry.with(layer).try_init()
            }
            Output::Stdout => {
                let layer = fmt::layer()
                    .with_ansi(self.ansi)
                    .with_writer(std::io::stdout);
                registry.with(layer).try_init()
            }
        };
        result.map_err(|e| TelemetryError::Filter(e.to_string()))
    }

    /// Build the subscriber with an additional resource-scoped [`LogCaptureLayer`]
    /// feeding a fresh bounded [`LogRing`], and install it as the process-global
    /// default — returning the shared ring so the control plane can serve it over
    /// `GET /api/v1/logs` (ADR-0060).
    ///
    /// The capture layer mirrors every emitted event (ours and the libav
    /// bridge's) into the ring with its resource attribution; it sits *outside*
    /// the `EnvFilter` so the ring captures records at the same verbosity the
    /// fmt sink writes. `run_id` stamps every captured record. The ring is bounded
    /// drop-oldest and read-only — it can never back-pressure the engine
    /// (invariant #10).
    ///
    /// # Errors
    ///
    /// Returns [`TelemetryError::Filter`] if the directive is invalid, or if a
    /// global subscriber has already been installed.
    pub fn try_init_with_capture(
        &self,
        ring: Arc<LogRing>,
        run_id: impl Into<String>,
    ) -> Result<()> {
        let filter = self.resolve_filter()?;
        let capture = LogCaptureLayer::new(ring).with_run_id(run_id);
        let registry = tracing_subscriber::registry().with(filter).with(capture);
        let result = match self.output {
            Output::Stderr => {
                let layer = fmt::layer()
                    .with_ansi(self.ansi)
                    .with_writer(std::io::stderr);
                registry.with(layer).try_init()
            }
            Output::Stdout => {
                let layer = fmt::layer()
                    .with_ansi(self.ansi)
                    .with_writer(std::io::stdout);
                registry.with(layer).try_init()
            }
        };
        result.map_err(|e| TelemetryError::Filter(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_env_disabled_uses_default_level_not_env() {
        // Even if RUST_LOG were set, disabling env must fall back to the default
        // level. We assert the builder still produces a working subscriber.
        let subscriber = SubscriberBuilder::new()
            .with_env(false)
            .with_default_level("error")
            .build()
            .expect("default-level build must succeed");
        tracing::subscriber::with_default(subscriber, || {
            tracing::error!("error is visible");
        });
    }

    #[test]
    fn invalid_default_level_is_an_error() {
        let err = SubscriberBuilder::new()
            .with_env(false)
            .with_default_level("=totally bogus=")
            .build();
        assert!(matches!(err, Err(TelemetryError::Filter(_))));
    }

    #[test]
    fn stdout_output_builds() {
        let subscriber = SubscriberBuilder::new()
            .with_directive("info")
            .with_output(Output::Stdout)
            .with_ansi(true)
            .build()
            .expect("stdout subscriber must build");
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("to stdout");
        });
    }
}
