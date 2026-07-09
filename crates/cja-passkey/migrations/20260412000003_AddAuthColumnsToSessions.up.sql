ALTER TABLE sessions ADD COLUMN IF NOT EXISTS user_id UUID REFERENCES passkey_users(user_id);
ALTER TABLE sessions ADD COLUMN IF NOT EXISTS challenge_state JSONB;
