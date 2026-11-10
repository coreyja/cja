use std::{fmt::Debug, ops::Deref};

use base64::{DecodeError, Engine};

#[derive(Clone)]
pub struct CookieKey(pub tower_cookies::Key);

impl Deref for CookieKey {
    type Target = tower_cookies::Key;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CookieKeyError {
    #[error("COOKIE_KEY is not valid base64: {0}")]
    InvalidBase64(#[from] DecodeError),
    #[error("COOKIE_KEY decodes to {len} bytes; it must be at least 32")]
    TooShort { len: usize },
}

impl CookieKey {
    /// Resolve the cookie key from the `COOKIE_KEY` environment variable.
    ///
    /// Uses `COOKIE_KEY` when it is set and well-formed; otherwise
    /// generates a fresh key in every build type and logs a warning.
    /// Apps that need sessions to survive restarts and deploys set
    /// `COOKIE_KEY`. Errors only on a malformed value (invalid base64, or
    /// fewer than 32 decoded bytes).
    pub fn from_env_or_generate() -> Result<Self, CookieKeyError> {
        resolve(std::env::var("COOKIE_KEY").ok())
    }

    /// Generate a fresh random key.
    ///
    /// Cookies encrypted with it die with the process.
    pub fn generate() -> Self {
        Self(tower_cookies::Key::generate())
    }
}

/// Testable core of [`CookieKey::from_env_or_generate`]: `value` is the raw
/// env value. Unset, empty, or whitespace-only values generate an ephemeral
/// key (with a warning); anything else is stripped of ASCII whitespace and
/// base64-decoded.
fn resolve(value: Option<String>) -> Result<CookieKey, CookieKeyError> {
    // `openssl rand -base64 64` wraps at 64 characters, and deploy config
    // often spells "unset" as an empty value.
    let value = value
        .map(|value| value.split_ascii_whitespace().collect::<String>())
        .filter(|value| !value.is_empty());
    if let Some(value) = value {
        tracing::info!("Using cookie key from environment");
        let decoded = base64::engine::general_purpose::STANDARD.decode(value.as_bytes())?;
        // `Key::derive_from` panics below 32 bytes.
        if decoded.len() < 32 {
            return Err(CookieKeyError::TooShort { len: decoded.len() });
        }
        Ok(CookieKey(tower_cookies::Key::derive_from(&decoded)))
    } else {
        tracing::warn!(
            "COOKIE_KEY is not set: generated an ephemeral key; sessions will not survive a restart"
        );
        Ok(CookieKey::generate())
    }
}

impl Debug for CookieKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CookieKey")
            .field("value", &"[omitted]")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `cookie::Key` has no `PartialEq`, so compare master-key bytes.
    fn masters_equal(a: &CookieKey, b: &CookieKey) -> bool {
        a.0.master() == b.0.master()
    }

    #[test]
    fn missing_key_generates_a_fresh_ephemeral_key() {
        let first = resolve(None).unwrap();
        let second = resolve(None).unwrap();
        assert!(!masters_equal(&first, &second));
    }

    #[test]
    fn blank_key_generates_like_missing() {
        assert!(resolve(Some(String::new())).is_ok());
        assert!(resolve(Some(" \n".to_string())).is_ok());
    }

    #[test]
    fn env_key_derives_deterministically() {
        use base64::Engine as _;
        let encoded = base64::engine::general_purpose::STANDARD.encode([7u8; 64]);
        let first = resolve(Some(encoded.clone())).unwrap();
        let second = resolve(Some(encoded)).unwrap();
        assert!(masters_equal(&first, &second));
    }

    #[test]
    fn invalid_base64_is_rejected() {
        assert!(matches!(
            resolve(Some("!!!not-base64!!!".to_string())),
            Err(CookieKeyError::InvalidBase64(_))
        ));
    }

    #[test]
    fn wrapped_base64_is_accepted() {
        use base64::Engine as _;
        let encoded = base64::engine::general_purpose::STANDARD.encode([7u8; 64]);
        let (first, rest) = encoded.split_at(64);
        let wrapped = format!("{first}\n{rest}");
        let plain = resolve(Some(encoded.clone())).unwrap();
        let from_wrapped = resolve(Some(wrapped)).unwrap();
        assert!(masters_equal(&plain, &from_wrapped));
    }

    #[test]
    fn short_key_is_an_error_not_a_panic() {
        use base64::Engine as _;
        let short = base64::engine::general_purpose::STANDARD.encode([7u8; 31]);
        assert!(matches!(
            resolve(Some(short)),
            Err(CookieKeyError::TooShort { len: 31 })
        ));
        let minimum = base64::engine::general_purpose::STANDARD.encode([7u8; 32]);
        assert!(resolve(Some(minimum)).is_ok());
    }
}
