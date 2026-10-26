CREATE TABLE IF NOT EXISTS agent_messages (
    run_id     UUID NOT NULL REFERENCES agent_runs(id) ON DELETE CASCADE,
    seq        BIGINT NOT NULL,
    role       TEXT NOT NULL CHECK (role IN ('system','user','assistant')),
    message    JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (run_id, seq)
);
