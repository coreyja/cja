CREATE TABLE IF NOT EXISTS passkey_credentials (
    credential_id_pk UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id         UUID NOT NULL REFERENCES passkey_users(user_id) ON DELETE CASCADE,
    credential_json JSONB NOT NULL,
    credential_id   BYTEA NOT NULL UNIQUE,
    name            TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_used_at    TIMESTAMPTZ
);

CREATE INDEX idx_passkey_credentials_user_id ON passkey_credentials(user_id);
