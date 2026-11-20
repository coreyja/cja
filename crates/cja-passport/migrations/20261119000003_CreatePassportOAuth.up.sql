CREATE TABLE passport_oauth_clients (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(), client_id TEXT UNIQUE NOT NULL,
    client_name TEXT NOT NULL, redirect_uris TEXT[] NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(), last_used_at TIMESTAMPTZ
);
CREATE TABLE passport_oauth_login_transactions (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    oauth_client_id UUID NOT NULL REFERENCES passport_oauth_clients(id) ON DELETE CASCADE,
    redirect_uri TEXT NOT NULL, resource TEXT NOT NULL, scopes TEXT[] NOT NULL,
    client_state TEXT NOT NULL, code_challenge TEXT NOT NULL,
    code_ttl_secs DOUBLE PRECISION NOT NULL CHECK (code_ttl_secs > 0),
    grant_idle_ttl_secs DOUBLE PRECISION NOT NULL CHECK (grant_idle_ttl_secs > 0),
    expires_at TIMESTAMPTZ NOT NULL
);
CREATE INDEX passport_oauth_login_transactions_expires_idx ON passport_oauth_login_transactions(expires_at);
CREATE INDEX passport_oauth_login_transactions_client_idx ON passport_oauth_login_transactions(oauth_client_id);
CREATE TABLE passport_oauth_consents (
    oauth_client_id UUID NOT NULL REFERENCES passport_oauth_clients(id) ON DELETE CASCADE,
    user_id UUID NOT NULL REFERENCES passport_users(user_id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY(oauth_client_id,user_id)
);
CREATE TABLE passport_oauth_pending_consents (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    oauth_client_id UUID NOT NULL REFERENCES passport_oauth_clients(id) ON DELETE CASCADE,
    user_id UUID NOT NULL REFERENCES passport_users(user_id) ON DELETE CASCADE,
    session_id UUID NOT NULL REFERENCES sessions(session_id) ON DELETE CASCADE,
    redirect_uri TEXT NOT NULL, resource TEXT NOT NULL, scopes TEXT[] NOT NULL,
    client_state TEXT NOT NULL, code_challenge TEXT NOT NULL,
    code_ttl_secs DOUBLE PRECISION NOT NULL CHECK (code_ttl_secs > 0),
    grant_idle_ttl_secs DOUBLE PRECISION NOT NULL CHECK (grant_idle_ttl_secs > 0),
    upstream_refresh_token TEXT NOT NULL CHECK (upstream_refresh_token <> ''),
    last_upstream_check_at TIMESTAMPTZ NOT NULL, csrf_hash BYTEA NOT NULL CHECK (octet_length(csrf_hash)=32),
    expires_at TIMESTAMPTZ NOT NULL
);
CREATE INDEX passport_oauth_pending_consents_expires_idx ON passport_oauth_pending_consents(expires_at);
CREATE INDEX passport_oauth_pending_consents_client_idx ON passport_oauth_pending_consents(oauth_client_id);
CREATE TABLE passport_oauth_grants (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    oauth_client_id UUID NOT NULL REFERENCES passport_oauth_clients(id) ON DELETE CASCADE,
    user_id UUID NOT NULL REFERENCES passport_users(user_id) ON DELETE CASCADE,
    resource TEXT NOT NULL, scopes TEXT[] NOT NULL,
    upstream_refresh_token TEXT NOT NULL CHECK (upstream_refresh_token <> ''),
    last_upstream_check_at TIMESTAMPTZ NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(), expires_at TIMESTAMPTZ NOT NULL,
    last_used_at TIMESTAMPTZ,
    UNIQUE(oauth_client_id,user_id,resource)
);
CREATE INDEX passport_oauth_grants_expires_idx ON passport_oauth_grants(expires_at);
CREATE TABLE passport_oauth_authorization_codes (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    code_hash BYTEA UNIQUE NOT NULL CHECK (octet_length(code_hash)=32),
    grant_id UUID NOT NULL REFERENCES passport_oauth_grants(id) ON DELETE CASCADE,
    redirect_uri TEXT NOT NULL, code_challenge TEXT NOT NULL,
    expires_at TIMESTAMPTZ NOT NULL, consumed_at TIMESTAMPTZ
);
CREATE INDEX passport_oauth_codes_expires_idx ON passport_oauth_authorization_codes(expires_at);
CREATE TABLE passport_oauth_access_tokens (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    token_hash BYTEA UNIQUE NOT NULL CHECK (octet_length(token_hash)=32),
    grant_id UUID NOT NULL REFERENCES passport_oauth_grants(id) ON DELETE CASCADE,
    expires_at TIMESTAMPTZ NOT NULL, created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX passport_oauth_access_expires_idx ON passport_oauth_access_tokens(expires_at);
CREATE TABLE passport_oauth_refresh_tokens (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    token_hash BYTEA UNIQUE NOT NULL CHECK (octet_length(token_hash)=32),
    grant_id UUID NOT NULL REFERENCES passport_oauth_grants(id) ON DELETE CASCADE,
    expires_at TIMESTAMPTZ NOT NULL, created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    consumed_at TIMESTAMPTZ
);
CREATE INDEX passport_oauth_refresh_expires_idx ON passport_oauth_refresh_tokens(expires_at);
ALTER TABLE passport_login_attempts ADD COLUMN oauth_transaction_id UUID REFERENCES passport_oauth_login_transactions(id) ON DELETE CASCADE;
ALTER TABLE passport_login_attempts DROP CONSTRAINT passport_login_attempts_check;
ALTER TABLE passport_login_attempts ADD CONSTRAINT passport_login_attempts_destination_check CHECK (
    (return_to IS NOT NULL)::int + (native_redirect_uri IS NOT NULL)::int + (oauth_transaction_id IS NOT NULL)::int = 1
);
