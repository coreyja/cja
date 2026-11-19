#[cfg(feature = "testing")]
pub mod mock_oauth;
pub mod test_db;

/// Remove inherited telemetry credentials and TLS test settings from a child.
pub fn scrub_telemetry_env(cmd: &mut std::process::Command) -> &mut std::process::Command {
    for (key, _) in std::env::vars_os() {
        if ["POSTHOG_", "EYES_", "OTEL_", "SSL_CERT_"]
            .iter()
            .any(|prefix| key.to_string_lossy().starts_with(prefix))
        {
            cmd.env_remove(key);
        }
    }
    cmd
}
