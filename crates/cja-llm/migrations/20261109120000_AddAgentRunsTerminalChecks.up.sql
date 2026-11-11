-- Terminal rows are internally consistent: a failed run always carries a
-- machine-readable fail_reason (and only failed runs do), every terminal
-- row carries finished_at, and no terminal row holds a lease. Every
-- existing writer in src already maintains these; the CHECKs make a
-- regression fail loudly instead of storing a lying row. No consumer has
-- an agent_runs table yet (checked 2026-11-09), so a plain ADD CONSTRAINT
-- cannot strand bad data.
ALTER TABLE agent_runs
  ADD CONSTRAINT failed_has_fail_reason
    CHECK ((status = 'failed') = (fail_reason IS NOT NULL)),
  ADD CONSTRAINT terminal_has_finished_at
    CHECK (status NOT IN ('succeeded', 'failed', 'cancelled') OR finished_at IS NOT NULL),
  ADD CONSTRAINT terminal_releases_lease
    CHECK (status NOT IN ('succeeded', 'failed', 'cancelled') OR locked_by IS NULL);
