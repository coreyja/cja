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
/// - `RUST_LOG`: Log filter (defaults to `info,{crate_name}=trace,tower_http=debug,serenity=error`)
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

    let env_filter = EnvFilter::builder().parse(&rust_log).wrap_err_with(|| {
        color_eyre::eyre::eyre!("Couldn't create env filter from {}", rust_log)
    })?;

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
        .try_init()?;

    Ok(eyes_shutdown_handle)
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
