# cja-passport

Confidential OIDC relying party for human users of cja apps. Requires tsidp
**v0.0.21 or newer** so refresh checks Tailscale WhoIs and device/user removal
revokes sessions. tsidp is experimental; pin its deployed version separately.

## Setup

Implement both `cja::app_state::AppState` and `HasPassportConfig`. Merge
`cja_passport::passport_router::<AppState>()` at the **root** of the app router.
The routes are fixed:

- `GET /auth/login?return_to=/path` — starts browser OIDC
- `GET /auth/callback` — handles OIDC success and access denial
- `POST /auth/logout` — deletes a cookie or bearer session
- `GET /auth/native/start?redirect_uri=scheme://callback` — starts iOS OIDC
- `POST /auth/native/exchange` with `{"code":"..."}` — exchanges one handoff code

Run `cja::db::run_migrations(&pool)` and then
`cja_passport::run_migrations(&pool)` at boot, followed by app migrations with
`set_ignore_missing(true)`. Schedule `prune_auth_artifacts(&pool)` alongside
`cja::server::session::prune`; each request enforces attempt and handoff
expiration independently of the scheduled cleanup.

Load the confidential client secret from setec at runtime in the app with
`SecretStore::load`, obtain a `Secret<String>`, call `.watch()`, and pass its
`watch::Receiver<Arc<String>>` to `PassportConfig::new`. The crate reads the
current secret for each token request, so rotation needs no restart. Set a
stable `COOKIE_KEY` across deploys. cja's generated fallback key invalidates
web cookies on restart; native bearer tokens remain database backed.

```rust,ignore
#[derive(cja_secret::Secrets)]
struct AuthSecrets {
    #[secret("attic/my-app/passport-client-secret")]
    client_secret: cja_secret::Secret<String>,
}

let (secrets, refresher) = cja_secret::SecretStore::connect(
    cja_secret::SetecConfig::from_env()?
).await?.load::<AuthSecrets>().await?;
let passport = cja_passport::PassportConfig::new(
    issuer, callback, client_id, secrets.client_secret.watch(),
    allowlist, native_schemes, cja_passport::SessionTimings::default(),
)?;
// Run refresher with the app's supervisor so rotations reach the watch receiver.
```

Use `AllowedSubject::new("userid:123")` for the per-app allowlist. The number
is the Tailscale user ID from a user-owned node or can be verified at
`/userinfo`. It is **not** an email or display name. The identity key is
`(issuer, subject)`, and the local `user_id` remains stable across logins.
Tagged nodes have no human identity and are rejected. Defaults are an eight
hour hard cap, refresh every hour, and a five minute retry after a transient
IdP failure; override with `SessionTimings`.

The refresh token is plaintext in `passport_sessions` and is a sensitive
credential. Protect the database and backups. The app also needs the
confidential client secret to redeem it. tsidp rotates one-use refresh tokens
on every refresh. A transport failure after tsidp consumes one can lead to
`invalid_grant` at the next check; that ends the session. A connection error,
timeout, or server error otherwise leaves the session usable only until its
original hard cap. Remove a user or device in the Tailscale admin console to
revoke access at the next refresh.

## iOS

Register a unique custom URL scheme for the app. Start
`ASWebAuthenticationSession` at `/auth/native/start` with that redirect URI
and `prefersEphemeralWebBrowserSession = true`. cja-passport acts as the
confidential OIDC client, owns the browser callback, and sends a 60 second,
single-use handoff code to the custom URI. Exchange the code from the app for
an opaque cja bearer token. Send `Authorization: Bearer <token>` on API
requests; `BearerUser` validates it against the same database session,
refresh policy, revocation, and eight hour cap as web cookies. The app never
receives a tsidp access token, refresh token, or client secret. tsidp access
tokens are opaque, so they are not JWTs for the app to validate. A custom
scheme can be claimed by another app; keep the scheme unique.

## Migrating from cja-passkey

1. Replace `HasPasskeyConfig` with `HasPassportConfig`, the passkey router
   with the root `passport_router`, and the old `CurrentUser` /
   `OptionalUser` imports with cja-passport's equivalents.
2. Remove passkey assets, JavaScript, registration and challenge routes, and
   passkey migrations from the consuming app. Run core migrations then passport
   migrations. Remove per-app sign-in-link and shared-token routes as each app
   migrates.
3. Explicitly map legacy local users to `(issuer, subject)` after verifying
   the Tailscale user ID. Do not infer this mapping from email.
4. Change native clients to the backend handoff above. Use a stable cookie key
   for web sessions. Serialize navigation across login: a stale pre-login
   response can set a new anonymous cookie after the callback rotates the old
   session, although the old row remains deleted.

## MCP OAuth for Claude connectors

An app with a public `/mcp` endpoint can enable the shared authorization server
without implementing OAuth routes itself. The public base URL must be an HTTPS
origin and the resource URL must be that origin's exact `/mcp` path. Loopback
HTTP is accepted for local tests. `OAuthOptions::default()` permits only
`https://claude.ai/api/mcp/auth_callback` and
`https://claude.com/api/mcp/auth_callback`, with `mcp offline_access` scopes.

```rust,ignore
use cja_passport::{HasMcpOAuthConfig, McpOAuthConfig, McpUser, OAuthOptions};

// Construct beside the app's existing PassportConfig.
let oauth = McpOAuthConfig::new(
    url::Url::parse("https://my-app.example/")?,
    url::Url::parse("https://my-app.example/mcp")?,
    OAuthOptions::default(),
)?;
// Store oauth in AppState; implement HasMcpOAuthConfig::mcp_oauth_config.
let router = axum::Router::new()
    .route("/mcp", axum::routing::post(mcp_handler))
    .merge(cja_passport::passport_router::<AppState>())
    .merge(cja_passport::oauth_router::<AppState>());

async fn mcp_handler(cja_passport::McpUser(user): cja_passport::McpUser) {
    // user is the allowlisted passport user for this bearer token.
}
```

`run_migrations` creates the `passport_oauth_*` tables. Schedule
`prune_auth_artifacts` for expired transactions, pending decisions, tokens,
grants, and unused clients. Registration is limited to 30 requests per process
per minute and 1,000 clients globally by default. The cap prunes unreferenced
clients idle at least one hour when full, so fresh clients can still fill it for
an hour. Override the redirect allowlist, scopes, lifetimes, and limits through
`OAuthOptions`. Codes last five minutes, access tokens one hour, and grants
slide to 180 days. A connector refresh checks the upstream device/user status;
transient passport outages are tolerated for at most eight hours since the
last successful check.

The authorization request must include the exact configured `/mcp` resource.
To preserve mull-site's current token behavior for Claude connectors, the token
endpoint accepts an omitted `resource` parameter on code exchange and refresh;
when supplied, it must match that resource exactly. The stored grant is always
checked against the configured resource.

The MCP security specification's confused-deputy guidance requires consent
because the app uses one upstream client but accepts dynamically registered
clients. Corey sees one **Allow** click per new connector and user after the
silent passport sign-in. The page shows the self-reported client name, resource,
scopes, and exact redirect URI. If Claude reuses its `client_id`, later
reconnects are silent. Tokens and codes are stored as hashes; each grant keeps
its own plaintext upstream refresh token in the database and backups.

In tsidp v0.0.21, upstream refresh tokens are memory-only. Restarting passport
invalidates every grant at its next refresh; reconnect in Claude once. The
passport sign-in itself remains silent. Removing a Tailscale device or user
revokes at the next upstream refresh; a previously issued MCP access token can
remain valid for up to its configured lifetime.

CJA's existing `server.request` telemetry records `/auth/callback?code=…` in
`uri`/`url.query`. Callback-query redaction is separate follow-up work; do not
add secret-bearing query parameters or application-level secret logs here.
