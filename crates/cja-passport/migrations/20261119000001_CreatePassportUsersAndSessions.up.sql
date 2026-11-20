CREATE TABLE passport_users (
    user_id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    issuer TEXT NOT NULL,
    subject TEXT NOT NULL,
    email TEXT,
    preferred_username TEXT,
    picture TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (issuer, subject)
);

CREATE TABLE passport_sessions (
    session_id UUID PRIMARY KEY REFERENCES sessions(session_id) ON DELETE CASCADE,
    user_id UUID NOT NULL REFERENCES passport_users(user_id) ON DELETE CASCADE,
    authenticated_at TIMESTAMPTZ NOT NULL,
    last_refresh_at TIMESTAMPTZ NOT NULL,
    next_refresh_at TIMESTAMPTZ NOT NULL,
    refresh_token TEXT NOT NULL CHECK (refresh_token <> ''),
    bearer_hash BYTEA UNIQUE CHECK (bearer_hash IS NULL OR octet_length(bearer_hash) = 32)
);
CREATE INDEX passport_sessions_user_id_idx ON passport_sessions(user_id);
