//! Application setup: tracing/observability initialization.
//!
//! Call [`setup_tracing`] inside the async context. It returns an optional
//! [`EyesShutdownHandle`] — keep it alive in your `main` scope:
//!
//! ```rust,ignore
//! let _eyes_handle = setup_tracing("my-app")?;
//! ```
//!
//! # Supported Backends
//!
//! - **Console**: Tree-formatted logs (default) or JSON with `JSON_LOGS` env var
//! - **Eyes**: Distributed tracing via `EYES_ORG_ID` + `EYES_APP_ID` (both required)

use color_eyre::eyre::Context;
use eyes_subscriber::{EyesLayer, EyesSubscriberBuilder};
use tracing_subscriber::filter::{LevelFilter, Targets};
use tracing_subscriber::{
    EnvFilter, Layer as _, Registry, layer::SubscriberExt as _, util::SubscriberInitExt as _,
};
use tracing_tree::HierarchicalLayer;
use uuid::Uuid;

// Re-export for consumers
pub use eyes_subscriber::EyesShutdownHandle;

/// Sets up tracing with optional Eyes and stdout layers.
///
/// Returns an optional `EyesShutdownHandle` that should be used for graceful shutdown
/// when Eyes is configured (via `EYES_ORG_ID` and `EYES_APP_ID` environment variables).
///
/// # Environment Variables
///
/// - `RUST_LOG`: Log filter (defaults to `info,{crate_name}=trace,tower_http=debug,serenity=error`).
///   rig's content-bearing targets are always capped regardless of this
///   value, including span-scoped directives: `rig::completions` and
///   `rig::streaming` at DEBUG, `rig_agent::agent::typed` at ERROR (see
///   [`filter_stack`]).
/// - `JSON_LOGS`: If set, outputs JSON logs instead of hierarchical
/// - `EYES_ORG_ID`: Eyes organization ID (UUID)
/// - `EYES_APP_ID`: Eyes application ID (UUID)
/// - `EYES_URL`: Eyes server URL (defaults to `https://eyes.coreyja.com`)
pub fn setup_tracing(crate_name: &str) -> color_eyre::Result<Option<EyesShutdownHandle>> {
    TracingConfig::new(crate_name).init()
}

/// Tracing setup with an optional boot-stable Eyes process identity.
/// Use the same identity in the boot manifest and heartbeat configuration.
pub struct TracingConfig<'a> {
    crate_name: &'a str,
    process: Option<eyes_subscriber::ProcessIdentity>,
}

impl<'a> TracingConfig<'a> {
    #[must_use]
    pub fn new(crate_name: &'a str) -> Self {
        Self {
            crate_name,
            process: None,
        }
    }

    #[must_use]
    pub fn process(mut self, identity: eyes_subscriber::ProcessIdentity) -> Self {
        self.process = Some(identity);
        self
    }

    pub fn init(self) -> color_eyre::Result<Option<EyesShutdownHandle>> {
        initialize_tracing(self.crate_name, self.process.as_ref())
    }
}

fn initialize_tracing(
    crate_name: &str,
    process: Option<&eyes_subscriber::ProcessIdentity>,
) -> color_eyre::Result<Option<EyesShutdownHandle>> {
    let rust_log = std::env::var("RUST_LOG")
        .unwrap_or_else(|_| format!("info,{crate_name}=trace,tower_http=debug,serenity=error"));

    let (env_filter, rig_cap) = filter_stack(&rust_log)?;

    // Setup Eyes layer if configured
    let (eyes_layer, eyes_shutdown_handle) = setup_eyes_layer(process)?;

    let stdout_layer = if std::env::var("JSON_LOGS").is_ok() {
        println!("Logging to STDOUT as JSON");

        tracing_subscriber::fmt::layer()
            .json()
            .with_current_span(true)
            .boxed()
    } else {
        let hierarchical = HierarchicalLayer::default()
            .with_writer(std::io::stdout)
            .with_indent_lines(true)
            .with_indent_amount(2)
            .with_thread_names(true)
            .with_thread_ids(true)
            .with_verbose_exit(true)
            .with_verbose_entry(true)
            .with_targets(true);

        println!("Logging to STDOUT as hierarchical");

        hierarchical.boxed()
    };

    Registry::default()
        .with(stdout_layer)
        .with(eyes_layer)
        .with(env_filter)
        .with(rig_cap)
        .try_init()?;

    Ok(eyes_shutdown_handle)
}

/// Builds the tracing filter stack for a `RUST_LOG` string: the parsed
/// [`EnvFilter`] AND the static rig content cap ([`rig_content_cap`]).
/// This is the one constructor both `initialize_tracing` and the tests
/// use, so the tests exercise the exact production composition.
///
/// Both filters are stacked as global filter layers
/// (`SubscriberExt::with`), and a stacked filter layer vetoes: `Layered`
/// enables a callsite only when EVERY layer enables it
/// (`&&`-composed `enabled`, tracing-subscriber 0.3.23
/// `src/layer/layered.rs:105-108`; `register_callsite` interest combines
/// through `pick_interest`, `layered.rs:99-103`). An event therefore
/// reaches a writer only when both the `RUST_LOG`-driven `EnvFilter` and
/// the cap enable it — and the cap is a compiled-in [`Targets`] filter
/// that no `RUST_LOG` value can raise.
///
/// The cap must NOT be `EnvFilter::add_directive` calls on the parsed
/// filter. Directives come in static and dynamic (span-scoped) kinds, and
/// `EnvFilter::enabled` consults the dynamic span-scope directives FIRST
/// (`src/filter/env/mod.rs:499-529`): inside any span matched by a span
/// directive such as `RUST_LOG=info,[request]=trace`, every event at or
/// below that directive's level is enabled WHATEVER its target, before
/// the static directives are consulted. `add_directive` can only replace
/// same-target STATIC directives, so those `RUST_LOG` values re-enable
/// rig's TRACE wire dumps inside the span. A separate [`Targets`] layer
/// has no span-scope behavior at all, so the veto holds everywhere.
fn filter_stack(rust_log: &str) -> color_eyre::Result<(EnvFilter, Targets)> {
    let env_filter = EnvFilter::builder()
        .parse(rust_log)
        .wrap_err_with(|| color_eyre::eyre::eyre!("Couldn't create env filter from {rust_log}"))?;

    Ok((env_filter, rig_content_cap()))
}

/// The static cap on rig's content-bearing targets: `rig::completions` and
/// `rig::streaming` at DEBUG, `rig_agent::agent::typed` at ERROR, with
/// every other target at TRACE so the cap suppresses nothing else.
///
/// rig 0.45 logs raw model content outside its content-telemetry gate
/// (`record_content_telemetry(false)` does not cover these):
/// - whole request/reply wire JSON at TRACE on `rig::completions`
///   (rig-core-0.45.0 `src/providers/openai/wire/chat.rs:778-782` for the
///   request, `src/driver.rs:563-570` for the reply; the emitter is gated
///   only by `tracing::enabled!(.., TRACE)` — `src/providers/internal/mod.rs:78-90`),
/// - the same on `rig::streaming`,
/// - the raw structured-output text at WARN on `rig_agent::agent::typed`
///   (rig-agent-0.45.0 `src/agent/typed.rs:191-193`: `warn!("... {err:?}")`
///   on a `StructuredOutputError` whose `Deserialization { output }` is the
///   model's raw output).
///
/// The `GenAI` span is an `info_span!` on `rig::completions` (rig-core
/// `src/telemetry/mod.rs:44,68`), so it stays enabled under the cap.
fn rig_content_cap() -> Targets {
    Targets::new()
        .with_default(LevelFilter::TRACE)
        .with_target("rig::completions", LevelFilter::DEBUG)
        .with_target("rig::streaming", LevelFilter::DEBUG)
        .with_target("rig_agent::agent::typed", LevelFilter::ERROR)
}

/// Sets up the Eyes tracing layer if `EYES_ORG_ID` and `EYES_APP_ID` are configured.
fn setup_eyes_layer(
    process: Option<&eyes_subscriber::ProcessIdentity>,
) -> color_eyre::Result<(Option<EyesLayer>, Option<EyesShutdownHandle>)> {
    let org_id = std::env::var("EYES_ORG_ID").ok();
    let app_id = std::env::var("EYES_APP_ID").ok();

    match (org_id, app_id) {
        (Some(org_id_str), Some(app_id_str)) => {
            let org_id = Uuid::parse_str(&org_id_str)
                .wrap_err_with(|| format!("Invalid EYES_ORG_ID: {org_id_str}"))?;
            let app_id = Uuid::parse_str(&app_id_str)
                .wrap_err_with(|| format!("Invalid EYES_APP_ID: {app_id_str}"))?;

            let (mut builder, transport) =
                EyesSubscriberBuilder::from_env_with_transport(org_id, app_id)
                    .wrap_err("Failed to build Eyes subscriber")?;
            if let Some(process) = process {
                builder = builder.with_process_instance_id(process.instance_id());
            }
            let (layer, shutdown_handle) = builder.build_with_transport(transport);

            let eyes_url = std::env::var("EYES_URL")
                .unwrap_or_else(|_| "https://eyes.coreyja.com".to_string());
            println!("Eyes layer configured (org: {org_id}, app: {app_id}, url: {eyes_url})");

            Ok((Some(layer), Some(shutdown_handle)))
        }
        (Some(_), None) => {
            println!("Skipping Eyes layer: EYES_ORG_ID set but EYES_APP_ID missing");
            Ok((None, None))
        }
        (None, Some(_)) => {
            println!("Skipping Eyes layer: EYES_APP_ID set but EYES_ORG_ID missing");
            Ok((None, None))
        }
        (None, None) => {
            println!("Skipping Eyes layer");
            Ok((None, None))
        }
    }
}

#[cfg(test)]
mod tests {
    use tracing::Level;

    use super::*;

    /// Builds the production composition: the `RUST_LOG`-driven `EnvFilter`
    /// with the static `Targets` cap stacked on top of it, exactly as
    /// `initialize_tracing` stacks them.
    fn subscriber_for(rust_log: &str) -> color_eyre::Result<impl tracing::Subscriber> {
        let (env_filter, rig_cap) = filter_stack(rust_log)?;
        Ok(Registry::default().with(env_filter).with(rig_cap))
    }

    /// rig's content-bearing targets stay capped for every `RUST_LOG`
    /// value — including span-scoped directives, whose spans we enter —
    /// both outside any span and inside one. Under the `EnvFilter` alone a
    /// span directive such as `[request]=trace` enables EVERY event at or
    /// below its level while the span is in scope (tracing-subscriber
    /// 0.3.23 `src/filter/env/mod.rs:499-529`); the stacked `Targets` cap
    /// vetoes it (`src/layer/layered.rs:105-108`).
    #[test]
    fn rig_content_targets_are_always_capped() -> color_eyre::Result<()> {
        for rust_log in [
            "trace",
            "info",
            "rig::completions=trace,rig::streaming=trace,rig_agent::agent::typed=trace",
            "info,[request]=trace",
            "info,[chat]=trace",
            "rig::completions[chat]=trace",
        ] {
            let has_global_default = matches!(
                rust_log,
                "trace"
                    | "info"
                    | "rig::completions=trace,rig::streaming=trace,rig_agent::agent::typed=trace"
            );
            let subscriber = subscriber_for(rust_log)?;

            tracing::subscriber::with_default(subscriber, || {
                assert_capped(rust_log, "outside any span", has_global_default);
            });

            // A span whose name matches the `[request]=trace` directive.
            tracing::subscriber::with_default(subscriber_for(rust_log)?, || {
                let span = tracing::info_span!(target: "app", "request");
                let _guard = span.enter();
                assert_capped(
                    rust_log,
                    "inside a span matching [request]",
                    has_global_default,
                );
            });

            // A span on `rig::completions` matching `[chat]=trace` and
            // `rig::completions[chat]=trace`.
            tracing::subscriber::with_default(subscriber_for(rust_log)?, || {
                let span = tracing::info_span!(target: "rig::completions", "chat");
                let _guard = span.enter();
                assert_capped(
                    rust_log,
                    "inside a span matching [chat]",
                    has_global_default,
                );
            });
        }
        Ok(())
    }

    fn assert_capped(rust_log: &str, context: &str, has_global_default: bool) {
        assert!(
            !tracing::enabled!(target: "rig::completions", Level::TRACE),
            "raw wire JSON must stay off for RUST_LOG={rust_log} {context}"
        );
        assert!(
            !tracing::enabled!(target: "rig::streaming", Level::TRACE),
            "raw stream wire JSON must stay off for RUST_LOG={rust_log} {context}"
        );
        assert!(
            !tracing::enabled!(target: "rig_agent::agent::typed", Level::WARN),
            "the raw-output warn must stay off for RUST_LOG={rust_log} {context}"
        );
        // ERROR stays enabled wherever the `RUST_LOG` value itself enables
        // it (a span-directive-only value disables everything outside its
        // spans — that is the EnvFilter, not the cap).
        if has_global_default {
            assert!(
                tracing::enabled!(target: "rig_agent::agent::typed", Level::ERROR),
                "ERROR stays enabled for RUST_LOG={rust_log} {context}"
            );
        }
    }

    /// The cap is narrow: it suppresses nothing else. Another target is
    /// enabled at TRACE under `RUST_LOG=trace`, the capped targets still
    /// pass DEBUG (rig's `GenAI` span on `rig::completions` is an
    /// `info_span!`, which stays enabled), `rig::completions` DEBUG is
    /// enabled under `RUST_LOG=debug`, and `rig_agent::agent::typed` ERROR
    /// is enabled under plain `info`.
    #[test]
    fn the_cap_is_narrow() -> color_eyre::Result<()> {
        let subscriber = subscriber_for("trace")?;
        tracing::subscriber::with_default(subscriber, || {
            assert!(tracing::enabled!(target: "rig::completions", Level::DEBUG));
            assert!(tracing::enabled!(target: "other_crate", Level::TRACE));
            // The GenAI span itself stays enabled.
            let span = tracing::info_span!(target: "rig::completions", "chat");
            assert!(!span.is_disabled());
        });

        tracing::subscriber::with_default(subscriber_for("debug")?, || {
            assert!(tracing::enabled!(target: "rig::completions", Level::DEBUG));
        });

        tracing::subscriber::with_default(subscriber_for("info")?, || {
            assert!(tracing::enabled!(target: "rig_agent::agent::typed", Level::ERROR));
        });
        Ok(())
    }
}
