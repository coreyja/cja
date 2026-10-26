CREATE TABLE IF NOT EXISTS agent_runs (
    id             UUID PRIMARY KEY,
    kind           TEXT NOT NULL,
    status         TEXT NOT NULL CHECK (status IN ('queued','running','waiting','succeeded','failed','cancelled')),
    input          TEXT NOT NULL,
    output         TEXT,
    error          TEXT,
    fail_reason    TEXT,
    model          TEXT NOT NULL,
    app            TEXT NOT NULL,
    turns          INTEGER NOT NULL DEFAULT 0,
    input_tokens   BIGINT NOT NULL DEFAULT 0,
    output_tokens  BIGINT NOT NULL DEFAULT 0,
    run_state      JSONB NOT NULL,
    pending_call_id   TEXT,
    pending_question  TEXT,
    cancel_requested BOOLEAN NOT NULL DEFAULT FALSE,
    locked_by      TEXT,
    locked_at      TIMESTAMPTZ,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    started_at     TIMESTAMPTZ,
    finished_at    TIMESTAMPTZ,
    CONSTRAINT waiting_has_pending_ask
        CHECK ((status = 'waiting') = (pending_call_id IS NOT NULL AND pending_question IS NOT NULL))
);
CREATE INDEX idx_agent_runs_status ON agent_runs(status);
CREATE INDEX idx_agent_runs_kind ON agent_runs(kind);
