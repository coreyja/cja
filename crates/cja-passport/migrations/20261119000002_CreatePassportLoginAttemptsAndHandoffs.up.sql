CREATE TABLE passport_login_attempts (
    state_hash BYTEA PRIMARY KEY CHECK (octet_length(state_hash) = 32),
    session_id UUID NOT NULL REFERENCES sessions(session_id) ON DELETE CASCADE,
    nonce TEXT NOT NULL,
    pkce_verifier TEXT NOT NULL,
    return_to TEXT,
    native_redirect_uri TEXT,
    expires_at TIMESTAMPTZ NOT NULL,
    CHECK ((return_to IS NOT NULL) <> (native_redirect_uri IS NOT NULL))
);
CREATE INDEX passport_login_attempts_expires_at_idx ON passport_login_attempts(expires_at);

CREATE TABLE passport_handoffs (
    code_hash BYTEA PRIMARY KEY CHECK (octet_length(code_hash) = 32),
    session_id UUID NOT NULL REFERENCES sessions(session_id) ON DELETE CASCADE,
    expires_at TIMESTAMPTZ NOT NULL
);
CREATE INDEX passport_handoffs_expires_at_idx ON passport_handoffs(expires_at);
