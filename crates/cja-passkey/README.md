# cja-passkey

`cja-passkey` provides registration and passkey login for cja applications. Run
`cja::db::run_migrations` and `cja_passkey::run_migrations` on the same database,
implement `HasPasskeyConfig` on your `AppState`, and merge
`cja_passkey::passkey_router::<AppState>()` into your Axum router. Add
`tower_cookies::CookieManagerLayer` outside the router. Set `PasskeyConfig`'s RP
ID and origin to the host and origin the browser uses. Use `CurrentUser` to
protect routes and `OptionalUser` when anonymous requests are allowed.

The router mounts these routes at its merge path:

| Method and route | Request | Response |
| --- | --- | --- |
| `POST /register/start` | `{ "username": "alice", "display_name": "Alice" }` (`display_name` optional) | WebAuthn creation challenge |
| `POST /register/finish` | WebAuthn registration credential JSON | `200` on success |
| `POST /auth/start` | `{ "username": "alice" }` | WebAuthn assertion challenge with that user's credential IDs; `404` for unknown username or no credentials |
| `POST /auth/finish` | WebAuthn assertion credential JSON | `200` on success |
| `POST /auth/discoverable/start` | No body required (the JS client sends `{}`) | WebAuthn assertion challenge with empty `allowCredentials` and a top-level `mediation: "conditional"` hint |
| `POST /auth/discoverable/finish` | WebAuthn assertion credential JSON, including `response.userHandle` | `200` on success |

Registration requests `residentKey: "preferred"` and
`requireResidentKey: false`. This encourages discoverable credentials on capable
authenticators while allowing older or nonresident security keys to register.
The discoverable chooser may omit a nonresident credential; users can still sign
in through the username path.

Mount the library manifest with any app manifest in one root asset router call,
before applying state:

```rust
.merge(cja::assets::router::<AppState>(&[
    &app_assets::MANIFEST,
    &cja_passkey::passkey_assets::MANIFEST,
])?)
```

Render `<script src>` with
`cja_passkey::passkey_assets::asset_url("passkey-client.js")`. The resulting
content-hashed JS and its external source map are embedded and served with
immutable cache headers. The `passkey_router` serves only authentication
endpoints. A Cargo build type checks and bundles the client using native tsc
and Rolldown; the first build may download pinned tsc into the shared cja cache.
Node and pnpm are needed only for browser E2E tests.

**Consumer migration:** `cja-passkey` now requires rustc 1.97 or newer because
`cja-build`'s Rolldown dependency brings in `oxc` 0.153.0. Bump the consumer's
`rust-toolchain.toml` and any Docker builder base image. Remove
`CJA_PASSKEY_STRICT_FRONTEND_BUILD` and `CJA_PASSKEY_PREBUILT_JS` from Dockerfiles
and CI; the crate no longer reads them. Remove a Node/pnpm Docker stage or CI
install if passkey was its only user. Code matching `AssetError::Collision` must
replace the old `first`/`second` fields with `first_owner`,
`first_logical_name`, `second_owner`, and `second_logical_name`.

The browser client exposes `cjaPasskey.createPasskeyClient(basePath)`. The
default `basePath` is empty; pass the router's mount prefix when needed. Call
its methods directly in a click handler:

```js
const passkeys = cjaPasskey.createPasskeyClient();
registerButton.onclick = () => passkeys.register({ username: "alice", displayName: "Alice" });
loginButton.onclick = () => passkeys.login();
fallbackButton.onclick = () => passkeys.loginWithUsername({ username: "alice" });
```

The client uses a click-driven browser credential prompt; it does not use
conditional-mediation autofill. The server's `mediation` hint is not passed to
`navigator.credentials.get` by this client. Starting a new challenge replaces
the previous one in the same session; successful finish clears it. The session
ID remains the same across login.

**Breaking JS API change:** existing callers of `.login({ username })` must
change to `.loginWithUsername({ username })` when bumping their pinned cja
revision. Calling `.login({ username })` now silently ignores the argument and
starts discoverable login.

To run the real Chromium test locally, supply a PostgreSQL master database URL
whose role can create databases, then install the E2E pnpm package:

```sh
cd crates/cja-passkey/e2e && pnpm install --frozen-lockfile && pnpm check && pnpm exec playwright install chromium
cd ../../..
DATABASE_URL=postgresql://postgres:postgres@localhost:5432/test_db \
  cargo test -p cja-passkey --test browser_fixture -- --ignored --nocapture
```

The ignored fixture creates and drops its own uniquely named database. Ordinary
`cargo test --all-targets` skips the browser test and needs no Playwright install.
