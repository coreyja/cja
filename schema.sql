-- Combined schema for cja framework + applications
-- This file represents the final state of the database schema for compile-time SQL validation.
-- It is derived from the migrations in crates/cja/migrations/

-- Jobs table - Persistent background job queue
CREATE TABLE IF NOT EXISTS jobs (
    job_id UUID PRIMARY KEY NOT NULL,
    name TEXT NOT NULL,
    payload JSONB NOT NULL,
    priority INT NOT NULL,
    run_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    locked_at TIMESTAMPTZ,
    locked_by TEXT,
    context TEXT NOT NULL,
    -- Added by 20251109000000_AddJobErrorTracking.sql
    error_count INTEGER NOT NULL DEFAULT 0,
    last_error_message TEXT,
    last_failed_at TIMESTAMPTZ
);

-- Crons table - Cron execution tracking
CREATE TABLE IF NOT EXISTS crons (
    cron_id UUID PRIMARY KEY,
    name TEXT NOT NULL,
    last_run_at TIMESTAMPTZ NOT NULL,
    created_at TIMESTAMPTZ NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_crons_name ON crons (name);

-- Sessions table - Database-backed HTTP sessions
CREATE TABLE IF NOT EXISTS sessions (
    session_id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    created_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP NOT NULL,
    updated_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP NOT NULL
);
