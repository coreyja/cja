//! The one rustls client configuration every cja-owned HTTPS client uses.
use std::sync::Arc;

use rustls_platform_verifier::BuilderVerifierExt as _;

/// Why [`ring_client_config`] could not build a configuration.
#[derive(Debug, thiserror::Error)]
pub(crate) enum TlsConfigError {
    #[error("selecting TLS protocol versions")]
    ProtocolVersions(#[source] rustls::Error),
    #[error("loading the platform certificate verifier")]
    PlatformVerifier(#[source] rustls::Error),
}

/// A rustls client configuration on ring with the platform verifier (system
/// roots), for `reqwest_012`'s `use_preconfigured_tls`.
///
/// The provider is passed explicitly, so building this installs no
/// process-wide `CryptoProvider` and leaves the app's own TLS setup alone.
/// Why ring and reqwest 0.12: see `reqwest_012` in `crates/cja/Cargo.toml`.
pub(crate) fn ring_client_config() -> Result<rustls::ClientConfig, TlsConfigError> {
    Ok(rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(TlsConfigError::ProtocolVersions)?
    .with_platform_verifier()
    .map_err(TlsConfigError::PlatformVerifier)?
    .with_no_client_auth())
}
