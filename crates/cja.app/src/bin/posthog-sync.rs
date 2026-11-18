use cja::{
    color_eyre::{self, eyre::WrapErr as _},
    observability::posthog,
};
use std::process::ExitCode;

#[tokio::main]
async fn main() -> color_eyre::Result<ExitCode> {
    let declarations = match cja_site::observability::declarations() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("invalid observability declarations: {e}");
            return Ok(ExitCode::from(2));
        }
    };
    let http = posthog::http_client().wrap_err("initialize the PostHog HTTP client")?;
    Ok(posthog::run_from_env("cja-site", &declarations, &http).await)
}
