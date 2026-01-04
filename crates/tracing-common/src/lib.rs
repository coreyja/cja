use std::{collections::HashMap, time::Duration};

use color_eyre::eyre::Context;
use eyes_subscriber::{EyesLayer, EyesSubscriberBuilder};
use opentelemetry_otlp::WithExportConfig;
use sentry::ClientInitGuard;
use tracing_opentelemetry::OpenTelemetryLayer;
use tracing_subscriber::{
    layer::SubscriberExt as _, util::SubscriberInitExt as _, EnvFilter, Layer as _, Registry,
};
use tracing_tree::HierarchicalLayer;
use uuid::Uuid;

// Re-export for consumers
pub use eyes_subscriber::EyesShutdownHandle;

pub fn setup_sentry() -> Option<ClientInitGuard> {
    let git_commit: Option<std::borrow::Cow<_>> =
        option_env!("VERGEN_GIT_SHA").map(std::convert::Into::into);
    let release_name =
        git_commit.unwrap_or_else(|| sentry::release_name!().unwrap_or_else(|| "dev".into()));

    if let Ok(sentry_dsn) = std::env::var("SENTRY_DSN") {
        println!("Sentry enabled");

        Some(sentry::init((
            sentry_dsn,
            sentry::ClientOptions {
                traces_sample_rate: 0.5,
                release: Some(release_name),
                ..Default::default()
            },
        )))
    } else {
        println!("Sentry not configured in this environment");

        None
    }
}

/// Sets up tracing with optional Eyes, Honeycomb, and stdout layers.
///
/// Returns an optional `EyesShutdownHandle` that should be used for graceful shutdown
/// when Eyes is configured (via `EYES_ORG_ID` and `EYES_APP_ID` environment variables).
///
/// # Environment Variables
///
/// - `RUST_LOG`: Log filter (defaults to `info,{crate_name}=trace,tower_http=debug,serenity=error`)
/// - `JSON_LOGS`: If set, outputs JSON logs instead of hierarchical
/// - `HONEYCOMB_API_KEY`: Enables Honeycomb tracing
/// - `EYES_ORG_ID`: Eyes organization ID (UUID)
/// - `EYES_APP_ID`: Eyes application ID (UUID)
/// - `EYES_URL`: Eyes server URL (defaults to `https://eyes.coreyja.com`)
pub fn setup_tracing(crate_name: &str) -> color_eyre::Result<Option<EyesShutdownHandle>> {
    let rust_log = std::env::var("RUST_LOG")
        .unwrap_or_else(|_| format!("info,{crate_name}=trace,tower_http=debug,serenity=error"));

    let env_filter = EnvFilter::builder().parse(&rust_log).wrap_err_with(|| {
        color_eyre::eyre::eyre!("Couldn't create env filter from {}", rust_log)
    })?;

    let opentelemetry_layer = if let Ok(honeycomb_key) = std::env::var("HONEYCOMB_API_KEY") {
        let mut map = HashMap::<String, String>::new();
        map.insert("x-honeycomb-team".to_string(), honeycomb_key);
        map.insert("x-honeycomb-dataset".to_string(), "coreyja.com".to_string());

        let tracer = opentelemetry_otlp::new_pipeline()
            .tracing()
            .with_exporter(
                opentelemetry_otlp::new_exporter()
                    .http()
                    .with_endpoint("https://api.honeycomb.io/v1/traces")
                    .with_timeout(Duration::from_secs(3))
                    .with_headers(map),
            )
            .install_batch(opentelemetry_sdk::runtime::Tokio)?;

        let opentelemetry_layer = OpenTelemetryLayer::new(tracer);
        println!("Honeycomb layer configured");

        Some(opentelemetry_layer)
    } else {
        println!("Skipping Honeycomb layer");

        None
    };

    // Setup Eyes layer if configured
    let (eyes_layer, eyes_shutdown_handle) = setup_eyes_layer()?;

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
        .with(opentelemetry_layer)
        .with(eyes_layer)
        .with(env_filter)
        .try_init()?;

    Ok(eyes_shutdown_handle)
}

/// Sets up the Eyes tracing layer if EYES_ORG_ID and EYES_APP_ID are configured.
fn setup_eyes_layer() -> color_eyre::Result<(Option<EyesLayer>, Option<EyesShutdownHandle>)> {
    let org_id = std::env::var("EYES_ORG_ID").ok();
    let app_id = std::env::var("EYES_APP_ID").ok();

    match (org_id, app_id) {
        (Some(org_id_str), Some(app_id_str)) => {
            let org_id = Uuid::parse_str(&org_id_str)
                .wrap_err_with(|| format!("Invalid EYES_ORG_ID: {org_id_str}"))?;
            let app_id = Uuid::parse_str(&app_id_str)
                .wrap_err_with(|| format!("Invalid EYES_APP_ID: {app_id_str}"))?;

            let (layer, shutdown_handle) = EyesSubscriberBuilder::build_from_env(org_id, app_id)
                .wrap_err("Failed to build Eyes subscriber")?;

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
