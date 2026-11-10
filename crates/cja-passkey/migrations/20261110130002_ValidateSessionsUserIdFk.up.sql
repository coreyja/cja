-- Validate in its own transaction so the scan does not block writes
-- (see 20261110130001_SessionsUserIdSetNullOnDelete.up.sql).
ALTER TABLE sessions VALIDATE CONSTRAINT sessions_user_id_fkey;
