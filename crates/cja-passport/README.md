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
