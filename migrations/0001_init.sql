-- Compact schema: no foreign keys, no extra indexes. Counter tables have the entity first in their primary
-- key (for WhoKnows); per-user queries read the history blobs, not the counters.

CREATE TABLE users (
    id           serial PRIMARY KEY,
    discord_id   bigint  NOT NULL UNIQUE,
    lb_name      text    NOT NULL,
    lb_token     bytea   NOT NULL,          -- nonce(12) || ChaCha20-Poly1305 ciphertext
    last_ts      bigint  NOT NULL DEFAULT 0, -- newest stored listened_at
    listens      int     NOT NULL DEFAULT 0, -- number of stored listens
    count_offset int     NOT NULL DEFAULT 0, -- LB count minus listens at the last full index (same-second duplicates etc.)
    indexed      boolean NOT NULL DEFAULT false,
    private      boolean NOT NULL DEFAULT false
);

CREATE TABLE artists (id serial PRIMARY KEY, name text NOT NULL);
CREATE UNIQUE INDEX artists_name ON artists (lower(name));

CREATE TABLE albums (id serial PRIMARY KEY, artist_id int NOT NULL, name text NOT NULL);
CREATE UNIQUE INDEX albums_name ON albums (artist_id, lower(name));

CREATE TABLE tracks (id serial PRIMARY KEY, artist_id int NOT NULL, name text NOT NULL);
CREATE UNIQUE INDEX tracks_name ON tracks (artist_id, lower(name));

CREATE TABLE user_artists (artist_id int, user_id int, plays int NOT NULL, PRIMARY KEY (artist_id, user_id));
CREATE TABLE user_albums  (album_id  int, user_id int, plays int NOT NULL, PRIMARY KEY (album_id, user_id));
CREATE TABLE user_tracks  (track_id  int, user_id int, plays int NOT NULL, PRIMARY KEY (track_id, user_id));

-- A user's full history: zstd-compressed chunks of up to 4096 listens (see src/history.rs).
CREATE TABLE history (user_id int, seq int, data bytea NOT NULL, PRIMARY KEY (user_id, seq));
-- The data is already zstd-compressed; Postgres doesn't need to try again.
ALTER TABLE history ALTER COLUMN data SET STORAGE EXTERNAL;

-- Only members registered with the bot are stored.
CREATE TABLE guild_members (guild_id bigint, user_id int, name text NOT NULL, PRIMARY KEY (guild_id, user_id));

CREATE TABLE guilds (id bigint PRIMARY KEY, prefix text NOT NULL);
