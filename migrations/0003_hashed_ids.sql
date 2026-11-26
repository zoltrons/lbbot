-- Discord user and server ids are stored as keyed hashes (see src/crypto.rs). Turning the old raw ids into
-- hashes needs TOKEN_KEY, so `Db::migrate_hashed_ids` does it in Rust at startup and records it in `app_state`.
ALTER TABLE users RENAME COLUMN discord_id TO discord_hash;
ALTER TABLE guild_members RENAME COLUMN guild_id TO guild_hash;
ALTER TABLE guilds RENAME COLUMN id TO guild_hash;

CREATE TABLE app_state (name text PRIMARY KEY);
