ALTER TABLE agent_runs
  DROP CONSTRAINT IF EXISTS failed_has_fail_reason,
  DROP CONSTRAINT IF EXISTS terminal_has_finished_at,
  DROP CONSTRAINT IF EXISTS terminal_releases_lease;
