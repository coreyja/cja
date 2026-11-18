#[tokio::main]
async fn main() -> std::process::ExitCode {
    match cja_site::observability::declarations() {
        Ok(d) => cja::observability::posthog::run_from_env("cja-site", &d).await,
        Err(e) => {
            eprintln!("invalid observability declarations: {e}");
            std::process::ExitCode::from(2)
        }
    }
}
