-- One row per heartbeat cron fire, rendered on /status.
CREATE TABLE heartbeats (
  id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
  started_at TIMESTAMPTZ NOT NULL,
  finished_at TIMESTAMPTZ,
  -- Calendar day of started_at in the site's display time zone.
  local_date DATE NOT NULL
);

CREATE INDEX idx_heartbeats_started_at ON heartbeats (started_at DESC);
