-- sessions.user_id references passkey_users with the default NO ACTION, so
-- deleting a user with live sessions fails. Sessions outliving their user
-- become anonymous instead. Added NOT VALID so this file holds its stronger
-- lock only for the catalog change; the next migration validates in its own
-- transaction under SHARE UPDATE EXCLUSIVE (same split as the cja#87
-- jobs_lock_pair_check precedent, 20261107120000/20261107120001).
ALTER TABLE sessions DROP CONSTRAINT IF EXISTS sessions_user_id_fkey;
ALTER TABLE sessions
    ADD CONSTRAINT sessions_user_id_fkey
    FOREIGN KEY (user_id) REFERENCES passkey_users(user_id)
    ON DELETE SET NULL
    NOT VALID;
