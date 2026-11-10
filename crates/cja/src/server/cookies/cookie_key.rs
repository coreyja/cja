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
    #[error(
        "COOKIE_KEY is not set: refusing to generate an ephemeral cookie key in a release build — every deploy would invalidate all session cookies. Set COOKIE_KEY (e.g. `openssl rand -base64 64`) for apps with sessions, or use CookieKey::generate() for apps without"
    )]
    Missing,
    #[error("COOKIE_KEY is not valid base64: {0}")]
    InvalidBase64(#[from] DecodeError),
    #[error("COOKIE_KEY decodes to {len} bytes; it must be at least 32")]
    TooShort { len: usize },
}

impl CookieKey {
    /// Resolve the cookie key from the `COOKIE_KEY` environment variable.
    ///
    /// This is the constructor for apps whose servers use sessions (the
    /// `Session` extractor): release builds refuse to boot without
    /// `COOKIE_KEY`, because a regenerated ephemeral key would invalidate
    /// every session cookie on each deploy. Debug builds warn and generate
    /// an ephemeral key so `cargo run` dev servers and `cargo test` work
    /// with zero configuration.
    ///
    /// Apps that do not use sessions should call [`CookieKey::generate`]
    /// instead and skip the env var entirely.
    pub fn from_env_or_generate() -> Result<Self, CookieKeyError> {
        resolve(std::env::var("COOKIE_KEY").ok(), cfg!(debug_assertions))
    }

    /// Generate a fresh random key, for apps that do not use sessions.
    ///
    /// Session cookies encrypted with a generated key die with the process,
    /// so session-using apps must use [`CookieKey::from_env_or_generate`]
    /// with `COOKIE_KEY` set instead.
    pub fn generate() -> Self {
        Self(tower_cookies::Key::generate())
    }
}

/// Testable core of [`CookieKey::from_env_or_generate`]: `value` is the raw
/// env value, `allow_ephemeral` whether a missing key may be replaced by a
/// generated ephemeral one (the constructor passes `cfg!(debug_assertions)`).
fn resolve(value: Option<String>, allow_ephemeral: bool) -> Result<CookieKey, CookieKeyError> {
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
    } else if allow_ephemeral {
        tracing::warn!(
            "COOKIE_KEY is not set: generated an ephemeral key; sessions will \
             not survive a restart. Set COOKIE_KEY."
        );
        Ok(CookieKey::generate())
    } else {
        Err(CookieKeyError::Missing)
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
    fn release_build_without_key_refuses() {
        assert!(matches!(resolve(None, false), Err(CookieKeyError::Missing)));
    }

    #[test]
    fn debug_build_without_key_generates_ephemeral() {
        assert!(resolve(None, true).is_ok());
    }

    #[test]
    fn invalid_base64_is_rejected() {
        assert!(matches!(
            resolve(Some("!!!not-base64!!!".to_string()), true),
            Err(CookieKeyError::InvalidBase64(_))
        ));
    }

    #[test]
    fn env_key_derives_identically_in_both_modes() {
        use base64::Engine as _;
        let encoded = base64::engine::general_purpose::STANDARD.encode([7u8; 64]);
        let release = resolve(Some(encoded.clone()), false).unwrap();
        let debug = resolve(Some(encoded), true).unwrap();
        assert!(masters_equal(&release, &debug));
    }

    #[test]
    fn wrapped_base64_is_accepted() {
        use base64::Engine as _;
        let encoded = base64::engine::general_purpose::STANDARD.encode([7u8; 64]);
        let (first, rest) = encoded.split_at(64);
        let wrapped = format!("{first}\n{rest}");
        let plain = resolve(Some(encoded.clone()), false).unwrap();
        let from_wrapped = resolve(Some(wrapped), false).unwrap();
        assert!(masters_equal(&plain, &from_wrapped));
    }

    #[test]
    fn blank_key_counts_as_missing() {
        assert!(matches!(
            resolve(Some(String::new()), false),
            Err(CookieKeyError::Missing)
        ));
        assert!(matches!(
            resolve(Some(" \n".to_string()), false),
            Err(CookieKeyError::Missing)
        ));
        assert!(resolve(Some(String::new()), true).is_ok());
    }

    #[test]
    fn short_key_is_an_error_not_a_panic() {
        use base64::Engine as _;
        let short = base64::engine::general_purpose::STANDARD.encode([7u8; 31]);
        assert!(matches!(
            resolve(Some(short), false),
            Err(CookieKeyError::TooShort { len: 31 })
        ));
        let minimum = base64::engine::general_purpose::STANDARD.encode([7u8; 32]);
        assert!(resolve(Some(minimum), false).is_ok());
    }
}
