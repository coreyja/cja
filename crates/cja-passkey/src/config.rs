use std::sync::Arc;
use url::Url;
use webauthn_rs::prelude::*;

#[derive(Clone)]
pub struct PasskeyConfig {
    pub webauthn: Arc<Webauthn>,
}

impl PasskeyConfig {
    pub fn new(rp_id: &str, rp_origin: &Url) -> cja::Result<Self> {
        let builder = WebauthnBuilder::new(rp_id, rp_origin)
            .map_err(|e| cja::color_eyre::eyre::eyre!("WebAuthn builder error: {e}"))?;
        let webauthn = Arc::new(
            builder
                .build()
                .map_err(|e| cja::color_eyre::eyre::eyre!("WebAuthn build error: {e}"))?,
        );
        Ok(Self { webauthn })
    }
}

pub trait HasPasskeyConfig {
    fn passkey_config(&self) -> &PasskeyConfig;
}
