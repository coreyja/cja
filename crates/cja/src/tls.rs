use color_eyre::eyre::{Result, WrapErr as _};
use rustls_platform_verifier::BuilderVerifierExt as _;
use std::sync::Arc;

pub(crate) fn ring_client_config() -> Result<rustls::ClientConfig> {
    Ok(rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .wrap_err("selecting TLS versions")?
    .with_platform_verifier()
    .wrap_err("loading platform verifier")?
    .with_no_client_auth())
}
