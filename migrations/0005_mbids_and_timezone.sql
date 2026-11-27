-- MusicBrainz ids and the facts the stats commands need, stored once per artist or track (not per listen).
-- They come from LB's mapping when listens are stored, else from a lookup the first time a command needs them.
ALTER TABLE artists ADD COLUMN mbid uuid;
-- Recording id.
ALTER TABLE tracks ADD COLUMN mbid uuid;
-- From MusicBrainz, else from the player that sent the listen. 0 = looked up, unknown.
ALTER TABLE tracks ADD COLUMN length_ms int;
-- First release as yyyymmdd (yyyy0000 when only the year is known). NULL = not looked up yet; 0 = looked up, unknown.
ALTER TABLE tracks ADD COLUMN released int;

-- IANA time zone name from `/timezone`; NULL = UTC.
ALTER TABLE users ADD COLUMN tz text;
-- The user's last `,obscurity` score (0-100), for the ranking among server members who ran it.
ALTER TABLE users ADD COLUMN obscurity smallint;
