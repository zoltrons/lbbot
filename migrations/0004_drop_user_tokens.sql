-- Users' LB tokens are kept only until their first index finishes (see `Db::finish_index`).
-- Metadata lookups use the operator's LB_TOKEN instead. Tokens of users already indexed are deleted.
ALTER TABLE users ALTER COLUMN lb_token DROP NOT NULL;
UPDATE users SET lb_token = NULL WHERE indexed;
