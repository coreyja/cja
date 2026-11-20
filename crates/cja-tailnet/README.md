# cja-tailnet

`cja-tailnet` joins an in-process, ephemeral Tailscale node at boot. It gets a
platform OIDC token from GCP, Fly Machines, or a mounted lanyard socket, exchanges it through Tailscale
workload identity federation (WIF), then mints a single-use, preauthorized auth
key. No Tailscale key is stored. Offline ephemeral nodes are reaped by Tailscale.

## Attic / lanyard setup

Set `TS_WIF_PROVIDER=lanyard`, `TS_WIF_CLIENT_ID` to the Tailscale federated
identity client ID, `TS_TAG` to its allowed tag, and `TS_HOSTNAME` explicitly.
`TS_WIF_AUDIENCE` defaults to `api.tailscale.com/<client ID>`.
`LANYARD_SOCKET` defaults to `/run/lanyard/lanyard.sock`.
`TS_JOIN_TIMEOUT_SECS` defaults to 120 for lanyard (30 for GCP and Fly).
The host bind-mounts only this app's socket **directory** at `/run/lanyard`;
that socket fixes the app's identity and is its only credential. The crate
stores no key, JWT, or other secret on disk or in the environment.

The crate sends HTTP/1.1 `POST /token` over the socket with JSON
`{"audience":"<aud>"}`. Lanyard returns `200 {"token":"<jwt>"}`.
Non-success responses surface their status and bounded body, while malformed
success responses report a lanyard ID-token error. Tokens last five minutes;
each WIF attempt requests a fresh one. After a Tailscale WIF 403 (for example,
while Tailscale refreshes JWKS after lanyard key rotation), the crate retries
with backoff for up to 90 seconds. It exposes the final 403 and retry budget
if the attempts fail. An explicit join timeout below 90 seconds can end the
retry early as `Error::JoinTimeout`.

## Fly Machines setup

Infra DEV-2539 owns the home-server federated identity and ACL. Its Tailscale
identity uses issuer `https://oidc.fly.io/coreyja-studio`, subject
`coreyja-studio:home-coreyja:*`, scope `auth_keys`, and allowed tag `tag:home`.
The wildcard covers changing machine IDs while pinning the app. For another
workload, use `<org>:<app>:*`, never `<org>:*`. Follow infra's
[Adding a workload](https://github.com/coreyja-studio/infra/blob/main/stacks/tailnet/README.md#adding-a-workload)
pattern; this crate does not create identities or ACL grants. See also
[Fly Machines OIDC](https://fly.io/blog/oidc-cloud-roles/) and
[Tailscale WIF](https://tailscale.com/docs/features/workload-identity-federation).

Set `TS_WIF_CLIENT_ID` to the federated identity's client ID and `TS_TAG=tag:home`.
`TS_WIF_AUDIENCE` defaults to `api.tailscale.com/<client ID>`; set it explicitly
if infra configures a different audience. `TS_WIF_PROVIDER=fly` is optional on
Fly when `FLY_APP_NAME` or `FLY_MACHINE_ID` is present. `TS_HOSTNAME` overrides
the default Fly app name (or machine ID when the app name is absent).

At runtime the crate requests a plain-text OIDC token over the `/.fly/api`
Unix socket. It uses neither a stored Tailscale auth key nor
`/.fly/oidc_token` (the latter depends on `AWS_ROLE_ARN` and has an AWS
audience).

The credential creating a Tailscale federated identity needs `federated_keys`
and the `auth_keys` scope it grants. It must directly own `tag:home` in
`tagOwners`. A create-only Terraform plan does not exercise the credential.
If first boot reports “tags are invalid or not permitted,” inspect the
federated identity's allowed tag before its generated key expires.
