use std::{future::Future, pin::Pin, time::Duration};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use openidconnect::{
    AccessTokenHash, AsyncHttpClient, AuthorizationCode, ClaimsVerificationError, ClientSecret,
    HttpRequest, HttpResponse, Nonce, OAuth2TokenResponse as _, PkceCodeVerifier, RefreshToken,
    SignatureVerificationError, TokenResponse as _,
    core::{CoreClient, CoreErrorResponseType, CoreJwsSigningAlgorithm, CoreProviderMetadata},
};
use serde::Deserialize;

use crate::config::PassportConfig;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum TokenFailure {
    InvalidGrant,
    Transient,
    Rejected,
}

#[derive(Clone)]
pub struct VerifiedIdentity {
    pub issuer: String,
    pub subject: String,
    pub email: Option<String>,
    pub preferred_username: Option<String>,
    pub picture: Option<String>,
}

#[derive(Debug, thiserror::Error)]
enum TokenHttpError {
    #[error("token endpoint unavailable")]
    Server,
    #[error("token transport failed")]
    Transport(#[from] openidconnect::HttpClientError<reqwest::Error>),
}

struct TokenHttpClient(reqwest::Client);

impl<'c> AsyncHttpClient<'c> for TokenHttpClient {
    type Error = TokenHttpError;
    type Future =
        Pin<Box<dyn Future<Output = Result<HttpResponse, Self::Error>> + Send + Sync + 'c>>;

    fn call(&'c self, request: HttpRequest) -> Self::Future {
        Box::pin(async move {
            let response = self.0.call(request).await?;
            if response.status().is_server_error() {
                return Err(TokenHttpError::Server);
            }
            Ok(response)
        })
    }
}

pub struct VerifiedTokens {
    pub identity: Option<VerifiedIdentity>,
    pub refresh_token: String,
}

pub(crate) async fn metadata(
    config: &PassportConfig,
    force: bool,
) -> Result<CoreProviderMetadata, TokenFailure> {
    if !force && let Some(meta) = config.discovery.read().await.as_ref() {
        return Ok(meta.clone());
    }
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        CoreProviderMetadata::discover_async(config.issuer.clone(), &config.http),
    )
    .await
    .map_err(|_| TokenFailure::Transient)?
    .map_err(|_| TokenFailure::Transient)?;
    if result.issuer() != &config.issuer {
        return Err(TokenFailure::Rejected);
    }
    *config.discovery.write().await = Some(result.clone());
    Ok(result)
}

fn build_client(
    config: &PassportConfig,
    meta: CoreProviderMetadata,
) -> CoreClient<
    openidconnect::EndpointSet,
    openidconnect::EndpointNotSet,
    openidconnect::EndpointNotSet,
    openidconnect::EndpointNotSet,
    openidconnect::EndpointMaybeSet,
    openidconnect::EndpointMaybeSet,
> {
    let secret = config.client_secret.borrow().clone();
    CoreClient::from_provider_metadata(
        meta,
        config.client_id.clone(),
        Some(ClientSecret::new((*secret).clone())),
    )
    .set_redirect_uri(config.callback.clone())
}

#[derive(Deserialize)]
struct RequiredClaims {
    nbf: i64,
    jti: String,
    iat: i64,
    exp: i64,
}

fn check_required(
    id_token: &openidconnect::core::CoreIdToken,
    now: i64,
) -> Result<(), TokenFailure> {
    let encoded = id_token.to_string();
    let payload = encoded.split('.').nth(1).ok_or(TokenFailure::Rejected)?;
    let bytes = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| TokenFailure::Rejected)?;
    let required: RequiredClaims =
        serde_json::from_slice(&bytes).map_err(|_| TokenFailure::Rejected)?;
    // tsidp and the relying party run on different hosts. Allow small clock skew
    // for issuance claims while keeping expiration strict.
    const CLOCK_SKEW_SECONDS: i64 = 60;
    if required.nbf > now + CLOCK_SKEW_SECONDS
        || required.iat > now + CLOCK_SKEW_SECONDS
        || required.exp <= now
        || required.jti.is_empty()
    {
        return Err(TokenFailure::Rejected);
    }
    Ok(())
}

#[derive(Debug)]
enum VerifyFailure {
    UnknownKid,
    Rejected,
}

fn verify(
    client: &impl ClientVerification,
    token: &openidconnect::core::CoreTokenResponse,
    nonce: Option<&Nonce>,
) -> Result<Option<VerifiedIdentity>, VerifyFailure> {
    let Some(id_token) = token.id_token() else {
        return Ok(None);
    };
    if *id_token
        .signing_alg()
        .map_err(|_| VerifyFailure::Rejected)?
        != CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha256
    {
        return Err(VerifyFailure::Rejected);
    }
    let verifier = client
        .id_verifier()
        .set_allowed_algs([CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha256]);
    let claims = match nonce {
        Some(nonce) => id_token.claims(&verifier, nonce),
        None => id_token.claims(&verifier, |_: Option<&Nonce>| Ok(())),
    }
    .map_err(|error| match error {
        ClaimsVerificationError::SignatureVerification(
            SignatureVerificationError::NoMatchingKey,
        ) => VerifyFailure::UnknownKid,
        _ => VerifyFailure::Rejected,
    })?;
    check_required(id_token, jiff::Timestamp::now().as_second())
        .map_err(|_| VerifyFailure::Rejected)?;
    if let Some(expected) = claims.access_token_hash() {
        let actual = AccessTokenHash::from_token(
            token.access_token(),
            id_token
                .signing_alg()
                .map_err(|_| VerifyFailure::Rejected)?,
            id_token
                .signing_key(&verifier)
                .map_err(|_| VerifyFailure::Rejected)?,
        )
        .map_err(|_| VerifyFailure::Rejected)?;
        if actual != *expected {
            return Err(VerifyFailure::Rejected);
        }
    }
    Ok(Some(VerifiedIdentity {
        issuer: claims.issuer().as_str().to_owned(),
        subject: claims.subject().as_str().to_owned(),
        email: claims.email().map(|x| x.as_str().to_owned()),
        preferred_username: claims.preferred_username().map(|x| x.as_str().to_owned()),
        picture: claims
            .picture()
            .and_then(|x| x.get(None))
            .map(|x| x.as_str().to_owned()),
    }))
}

trait ClientVerification {
    fn id_verifier(&self) -> openidconnect::core::CoreIdTokenVerifier<'_>;
}
impl ClientVerification
    for CoreClient<
        openidconnect::EndpointSet,
        openidconnect::EndpointNotSet,
        openidconnect::EndpointNotSet,
        openidconnect::EndpointNotSet,
        openidconnect::EndpointMaybeSet,
        openidconnect::EndpointMaybeSet,
    >
{
    fn id_verifier(&self) -> openidconnect::core::CoreIdTokenVerifier<'_> {
        self.id_token_verifier()
    }
}

pub async fn exchange_code(
    config: &PassportConfig,
    code: &str,
    verifier: &str,
    nonce: &str,
) -> Result<(VerifiedIdentity, String), TokenFailure> {
    let meta = metadata(config, false).await?;
    let client = build_client(config, meta);
    let request = client
        .exchange_code(AuthorizationCode::new(code.to_owned()))
        .map_err(|_| TokenFailure::Rejected)?
        .set_pkce_verifier(PkceCodeVerifier::new(verifier.to_owned()));
    let http = TokenHttpClient(config.http.clone());
    let token = tokio::time::timeout(Duration::from_secs(5), request.request_async(&http))
        .await
        .map_err(|_| TokenFailure::Transient)?
        .map_err(|error| match error {
            openidconnect::RequestTokenError::ServerResponse(response)
                if *response.error() == CoreErrorResponseType::InvalidGrant =>
            {
                TokenFailure::InvalidGrant
            }
            openidconnect::RequestTokenError::Request(
                TokenHttpError::Server | TokenHttpError::Transport(_),
            ) => TokenFailure::Transient,
            _ => TokenFailure::Rejected,
        })?;
    let login_nonce = Nonce::new(nonce.to_owned());
    let identity = match verify(&client, &token, Some(&login_nonce)) {
        Ok(value) => value,
        Err(VerifyFailure::UnknownKid) => {
            let fresh = build_client(config, metadata(config, true).await?);
            verify(&fresh, &token, Some(&login_nonce)).map_err(|_| TokenFailure::Rejected)?
        }
        Err(VerifyFailure::Rejected) => return Err(TokenFailure::Rejected),
    }
    .ok_or(TokenFailure::Rejected)?;
    let refresh = token
        .refresh_token()
        .ok_or(TokenFailure::Rejected)?
        .secret()
        .to_owned();
    Ok((identity, refresh))
}

pub async fn refresh(
    config: &PassportConfig,
    token: &str,
    expected: (&str, &str),
) -> Result<VerifiedTokens, TokenFailure> {
    let meta = metadata(config, false).await?;
    let client = build_client(config, meta);
    let current_refresh = RefreshToken::new(token.to_owned());
    let request = client
        .exchange_refresh_token(&current_refresh)
        .map_err(|_| TokenFailure::Rejected)?;
    let http = TokenHttpClient(config.http.clone());
    let response = tokio::time::timeout(Duration::from_secs(5), request.request_async(&http))
        .await
        .map_err(|_| TokenFailure::Transient)?
        .map_err(|error| match error {
            openidconnect::RequestTokenError::ServerResponse(response)
                if *response.error() == CoreErrorResponseType::InvalidGrant =>
            {
                TokenFailure::InvalidGrant
            }
            openidconnect::RequestTokenError::Request(
                TokenHttpError::Server | TokenHttpError::Transport(_),
            ) => TokenFailure::Transient,
            _ => TokenFailure::Rejected,
        })?;
    let refresh = response
        .refresh_token()
        .ok_or(TokenFailure::Rejected)?
        .secret()
        .to_owned();
    let identity = match verify(&client, &response, None) {
        Ok(value) => value,
        Err(VerifyFailure::UnknownKid) => {
            let fresh = build_client(config, metadata(config, true).await?);
            verify(&fresh, &response, None).map_err(|_| TokenFailure::Rejected)?
        }
        Err(VerifyFailure::Rejected) => return Err(TokenFailure::Rejected),
    };
    if identity
        .as_ref()
        .is_some_and(|id| (id.issuer.as_str(), id.subject.as_str()) != expected)
    {
        return Err(TokenFailure::Rejected);
    }
    Ok(VerifiedTokens {
        identity,
        refresh_token: refresh,
    })
}
