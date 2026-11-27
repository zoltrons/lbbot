//! All SQL lives here. Bulk operations run as one query over unnest arrays.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use anyhow::Result;
use sqlx::{AssertSqlSafe, PgPool, Postgres, Transaction, postgres::PgPoolOptions};

use crate::crypto::Crypto;
use crate::history::{self, CHUNK, Entry};
use crate::lb::Listen;

pub type Tx<'a> = Transaction<'a, Postgres>;

/// Methods take raw Discord ids and hash them here (see `Crypto::id`); the database never sees a raw id.
#[derive(Clone)]
pub struct Db(pub PgPool, pub Arc<Crypto>);

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct User {
    pub id: i32,
    pub discord_hash: i64,
    pub lb_name: String,
    /// Only set between /login and the end of the first index.
    pub lb_token: Option<Vec<u8>>,
    pub last_ts: i64,
    pub listens: i32,
    pub count_offset: i32,
    pub indexed: bool,
    /// IANA time zone name; `None` = UTC.
    pub tz: Option<String>,
}

/// What the stats commands know about a track (see migration 0005).
#[derive(Clone, Debug, Default, PartialEq, sqlx::FromRow)]
pub struct TrackFacts {
    pub mbid: Option<String>,
    pub length_ms: Option<i32>,
    /// yyyymmdd; `None` = not looked up yet, 0 = unknown.
    pub released: Option<i32>,
}

#[derive(Debug, sqlx::FromRow)]
pub struct WkRow {
    pub name: String,
    pub lb_name: String,
    pub discord_hash: i64,
    pub plays: i32,
    pub listeners: i64,
    pub total: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    Artist,
    Album,
    Track,
}

pub async fn connect(url: &str, crypto: Arc<Crypto>) -> Result<Db> {
    let pool = PgPoolOptions::new().max_connections(8).connect(url).await?;
    sqlx::migrate!().run(&pool).await?;
    let db = Db(pool, crypto);
    db.migrate_hashed_ids().await?;
    Ok(db)
}

impl Db {
    /// One-time conversion for databases from before migration 0003 (see `hash_ids`), recorded in `app_state`.
    async fn migrate_hashed_ids(&self) -> Result<()> {
        let mut tx = self.0.begin().await?;
        // Two processes starting together must not both convert (and hash the ids twice).
        sqlx::query("SELECT pg_advisory_xact_lock(7262871)").execute(&mut *tx).await?;
        // Hashes made with a different key match nobody; refuse to start instead of orphaning every user.
        let check = format!("key:{}", self.1.key_check());
        let keys: Vec<String> = sqlx::query_scalar("SELECT name FROM app_state WHERE name LIKE 'key:%'").fetch_all(&mut *tx).await?;
        anyhow::ensure!(keys.iter().all(|k| *k == check), "TOKEN_KEY differs from the key this database was set up with");
        if keys.is_empty() {
            sqlx::query("INSERT INTO app_state VALUES ($1)").bind(&check).execute(&mut *tx).await?;
        }
        let done: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM app_state WHERE name = 'hashed_ids')").fetch_one(&mut *tx).await?;
        if done {
            return Ok(tx.commit().await?);
        }
        let (users, guilds) = self.hash_ids(&mut tx).await?;
        sqlx::query("INSERT INTO app_state VALUES ('hashed_ids')").execute(&mut *tx).await?;
        tx.commit().await?;
        tracing::info!(users, guilds, "hashed discord ids");
        Ok(())
    }

    /// Raw Discord ids become keyed hashes, and tokens move to the current format, bound to their owner.
    async fn hash_ids(&self, tx: &mut Tx<'_>) -> Result<(usize, usize)> {
        let users: Vec<(i32, i64, Option<Vec<u8>>)> = sqlx::query_as("SELECT id, discord_hash, lb_token FROM users").fetch_all(&mut **tx).await?;
        for (id, discord_id, token) in &users {
            let hash = self.1.id(*discord_id);
            let token = token.as_ref().map(|t| self.1.decrypt_legacy(t).map(|t| self.1.encrypt(&t, hash))).transpose()?;
            sqlx::query("UPDATE users SET discord_hash = $2, lb_token = $3 WHERE id = $1")
                .bind(id).bind(hash).bind(token).execute(&mut **tx).await?;
        }
        let guilds: Vec<i64> = sqlx::query_scalar("SELECT guild_hash FROM guild_members UNION SELECT guild_hash FROM guilds")
            .fetch_all(&mut **tx).await?;
        for guild in &guilds {
            let hash = self.1.id(*guild);
            sqlx::query("UPDATE guild_members SET guild_hash = $2 WHERE guild_hash = $1").bind(guild).bind(hash).execute(&mut **tx).await?;
            sqlx::query("UPDATE guilds SET guild_hash = $2 WHERE guild_hash = $1").bind(guild).bind(hash).execute(&mut **tx).await?;
        }
        Ok((users.len(), guilds.len()))
    }

    pub async fn user_by_discord(&self, discord_id: i64) -> Result<Option<User>> {
        Ok(sqlx::query_as("SELECT * FROM users WHERE discord_hash = $1").bind(self.1.id(discord_id)).fetch_optional(&self.0).await?)
    }

    pub fn token(&self, user: &User) -> Result<Option<String>> {
        user.lb_token.as_ref().map(|t| self.1.decrypt(t, user.discord_hash)).transpose()
    }

    pub async fn user(&self, id: i32) -> Result<User> {
        Ok(sqlx::query_as("SELECT * FROM users WHERE id = $1").bind(id).fetch_one(&self.0).await?)
    }

    pub async fn unindexed_users(&self) -> Result<Vec<i32>> {
        Ok(sqlx::query_scalar("SELECT id FROM users WHERE NOT indexed AND lb_token IS NOT NULL").fetch_all(&self.0).await?)
    }

    /// New user, or a switch to another LB account: the history is indexed from scratch.
    pub async fn upsert_user(&self, discord_id: i64, lb_name: &str, token: &str) -> Result<i32> {
        let hash = self.1.id(discord_id);
        let token = self.1.encrypt(token, hash);
        if let Some(old) = self.user_by_discord(discord_id).await? {
            let mut tx = self.0.begin().await?;
            if !old.lb_name.eq_ignore_ascii_case(lb_name) {
                clear_listening_data(&mut tx, old.id).await?;
                sqlx::query("UPDATE users SET last_ts = 0, indexed = false WHERE id = $1").bind(old.id).execute(&mut *tx).await?;
            }
            sqlx::query("UPDATE users SET lb_name = $2, lb_token = $3 WHERE id = $1")
                .bind(old.id).bind(lb_name).bind(&token).execute(&mut *tx).await?;
            tx.commit().await?;
            return Ok(old.id);
        }
        Ok(sqlx::query_scalar("INSERT INTO users (discord_hash, lb_name, lb_token) VALUES ($1, $2, $3) RETURNING id")
            .bind(hash).bind(lb_name).bind(&token).fetch_one(&self.0).await?)
    }

    pub async fn delete_user(&self, id: i32) -> Result<()> {
        let mut tx = self.0.begin().await?;
        clear_listening_data(&mut tx, id).await?;
        sqlx::query("DELETE FROM guild_members WHERE user_id = $1").bind(id).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM users WHERE id = $1").bind(id).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Indexed users who listened in the last 30 days, for the twice-daily refresh. Uses the newest stored
    /// listen as the activity signal, so the bot doesn't track when people use it.
    pub async fn active_users(&self) -> Result<Vec<User>> {
        Ok(sqlx::query_as("SELECT * FROM users WHERE indexed AND last_ts > extract(epoch FROM now())::bigint - 30 * 86400")
            .fetch_all(&self.0).await?)
    }

    /// A random user for the bot's avatar: indexed, not private, listened in the last 30 days.
    /// Also returns a random number 0..4 for the pick mode.
    pub async fn featured_user(&self) -> Result<Option<(String, i32)>> {
        Ok(sqlx::query_as(
            "SELECT lb_name, floor(random() * 4)::int FROM users
             WHERE indexed AND NOT private AND last_ts > extract(epoch FROM now())::bigint - 30 * 86400
             ORDER BY random() LIMIT 1",
        )
        .fetch_optional(&self.0).await?)
    }

    pub async fn featured_blocked(&self) -> Result<HashSet<String>> {
        let rows: Vec<String> = sqlx::query_scalar("SELECT release_group FROM featured_block").fetch_all(&self.0).await?;
        Ok(rows.into_iter().collect())
    }

    pub async fn block_featured(&self, release_group: &str) -> Result<()> {
        sqlx::query("INSERT INTO featured_block VALUES ($1) ON CONFLICT DO NOTHING").bind(release_group).execute(&self.0).await?;
        Ok(())
    }

    pub async fn toggle_private(&self, id: i32) -> Result<bool> {
        Ok(sqlx::query_scalar("UPDATE users SET private = NOT private WHERE id = $1 RETURNING private")
            .bind(id).fetch_one(&self.0).await?)
    }

    /// Turns names into ids and inserts the missing ones. The result is in the same order as `listens`.
    pub async fn intern(&self, listens: &[Listen]) -> Result<Vec<Entry>> {
        let mut artist_names: Vec<String> = listens.iter().map(|l| l.track_metadata.artist_name.clone()).collect();
        artist_names.sort_unstable();
        artist_names.dedup();
        sqlx::query("INSERT INTO artists (name) SELECT * FROM unnest($1::text[]) ON CONFLICT DO NOTHING")
            .bind(&artist_names).execute(&self.0).await?;
        let artists: HashMap<String, i32> = sqlx::query_as(
            "SELECT x, a.id FROM unnest($1::text[]) x JOIN artists a ON lower(a.name) = lower(x)",
        )
        .bind(&artist_names).fetch_all(&self.0).await?.into_iter().collect();

        let artist_of = |l: &Listen| artists[&l.track_metadata.artist_name];
        let albums = self
            .intern_child("albums", listens.iter().filter_map(|l| {
                l.track_metadata.release_name.as_deref().filter(|n| !n.trim().is_empty()).map(|n| (artist_of(l), n))
            }))
            .await?;
        let tracks = self.intern_child("tracks", listens.iter().map(|l| (artist_of(l), l.track_metadata.track_name.as_str()))).await?;
        self.save_mapping(listens, &artists, &tracks).await?;

        Ok(listens
            .iter()
            .map(|l| {
                let a = artist_of(l);
                let m = &l.track_metadata;
                Entry {
                    ts: l.listened_at as i64,
                    track: tracks[&(a, m.track_name.clone())],
                    album: m.release_name.as_ref().and_then(|n| albums.get(&(a, n.clone()))).copied().unwrap_or(0),
                }
            })
            .collect())
    }

    /// MusicBrainz ids and lengths that came with the listens, for entities that don't have them yet. A track that
    /// gets its id here is looked up again for its facts (`released` back to NULL).
    async fn save_mapping(&self, listens: &[Listen], artists: &HashMap<String, i32>, tracks: &HashMap<(i32, String), i32>) -> Result<()> {
        let mut artist_ids: HashMap<i32, &str> = HashMap::new();
        let mut track_rows: HashMap<i32, (Option<&str>, Option<i32>)> = HashMap::new();
        for l in listens {
            let m = &l.track_metadata;
            let a = artists[&m.artist_name];
            if let Some(mbid) = &m.artist_mbid {
                artist_ids.entry(a).or_insert(mbid);
            }
            if m.recording_mbid.is_some() || m.length_ms.is_some() {
                let row = track_rows.entry(tracks[&(a, m.track_name.clone())]).or_default();
                row.0 = row.0.or(m.recording_mbid.as_deref());
                row.1 = row.1.or(m.length_ms);
            }
        }
        if !artist_ids.is_empty() {
            let (ids, mbids): (Vec<i32>, Vec<&str>) = artist_ids.into_iter().unzip();
            self.set_artist_mbids(&ids, &mbids).await?;
        }
        if track_rows.is_empty() {
            return Ok(());
        }
        let ids: Vec<i32> = track_rows.keys().copied().collect();
        let mbids: Vec<Option<&str>> = ids.iter().map(|i| track_rows[i].0).collect();
        let lengths: Vec<Option<i32>> = ids.iter().map(|i| track_rows[i].1).collect();
        sqlx::query(
            "UPDATE tracks t SET mbid = coalesce(t.mbid, x.m::uuid),
                 released = CASE WHEN t.mbid IS NULL AND x.m IS NOT NULL THEN NULL ELSE t.released END,
                 length_ms = CASE WHEN coalesce(t.length_ms, 0) = 0 THEN coalesce(x.l, t.length_ms) ELSE t.length_ms END
             FROM unnest($1::int[], $2::text[], $3::int[]) x(id, m, l)
             WHERE t.id = x.id AND ((t.mbid IS NULL AND x.m IS NOT NULL) OR (coalesce(t.length_ms, 0) = 0 AND x.l IS NOT NULL))",
        )
        .bind(&ids).bind(&mbids).bind(&lengths).execute(&self.0).await?;
        Ok(())
    }

    /// Sets the MusicBrainz id of artists that have none.
    pub async fn set_artist_mbids(&self, ids: &[i32], mbids: &[&str]) -> Result<()> {
        sqlx::query(
            "UPDATE artists a SET mbid = x.m::uuid FROM unnest($1::int[], $2::text[]) x(id, m) WHERE a.id = x.id AND a.mbid IS NULL",
        )
        .bind(ids).bind(mbids).execute(&self.0).await?;
        Ok(())
    }

    pub async fn artist_mbids(&self, ids: &[i32]) -> Result<HashMap<i32, String>> {
        Ok(sqlx::query_as("SELECT id, mbid::text FROM artists WHERE id = ANY($1) AND mbid IS NOT NULL")
            .bind(ids).fetch_all(&self.0).await?.into_iter().collect())
    }

    pub async fn track_facts(&self, ids: &[i32]) -> Result<HashMap<i32, TrackFacts>> {
        type Row = (i32, Option<String>, Option<i32>, Option<i32>);
        let rows: Vec<Row> = sqlx::query_as("SELECT id, mbid::text, length_ms, released FROM tracks WHERE id = ANY($1)").bind(ids).fetch_all(&self.0).await?;
        Ok(rows.into_iter().map(|(id, mbid, length_ms, released)| (id, TrackFacts { mbid, length_ms, released })).collect())
    }

    /// Results of name lookups for tracks without an id: the id when one was found, else the track is marked as
    /// looked up (`released` = 0), so it isn't looked up again.
    pub async fn set_track_mbids(&self, ids: &[i32], mbids: &[Option<String>]) -> Result<()> {
        sqlx::query(
            "UPDATE tracks t SET mbid = x.m::uuid, released = CASE WHEN x.m IS NULL THEN 0 END,
                 length_ms = CASE WHEN x.m IS NULL THEN coalesce(t.length_ms, 0) ELSE t.length_ms END
             FROM unnest($1::int[], $2::text[]) x(id, m) WHERE t.id = x.id AND t.mbid IS NULL",
        )
        .bind(ids).bind(mbids).execute(&self.0).await?;
        Ok(())
    }

    /// Facts read from MusicBrainz. A known length from MusicBrainz replaces the player's.
    pub async fn set_track_facts(&self, ids: &[i32], facts: &[crate::lb::RecordingFacts]) -> Result<()> {
        let lengths: Vec<i32> = facts.iter().map(|f| f.length_ms).collect();
        let released: Vec<i32> = facts.iter().map(|f| f.released).collect();
        sqlx::query(
            "UPDATE tracks t SET released = x.r, length_ms = CASE WHEN x.l > 0 THEN x.l ELSE coalesce(t.length_ms, 0) END
             FROM unnest($1::int[], $2::int[], $3::int[]) x(id, l, r) WHERE t.id = x.id",
        )
        .bind(ids).bind(&lengths).bind(&released).execute(&self.0).await?;
        Ok(())
    }

    /// Each artist's most played track across the bot, the key for a name lookup of the artist.
    pub async fn artist_top_tracks(&self, artists: &[i32]) -> Result<HashMap<i32, i32>> {
        Ok(sqlx::query_as(
            "SELECT DISTINCT ON (t.artist_id) t.artist_id, t.id FROM tracks t JOIN user_tracks c ON c.track_id = t.id
             WHERE t.artist_id = ANY($1) GROUP BY t.artist_id, t.id ORDER BY t.artist_id, sum(c.plays) DESC, t.id",
        )
        .bind(artists).fetch_all(&self.0).await?.into_iter().collect())
    }

    /// The server's most played artists, from the members' counters: (artist, plays).
    // shortcut: the counters have no user index, so this reads all of `user_artists`; add one if servers get big.
    pub async fn server_top_artists(&self, guild: i64, limit: i64) -> Result<Vec<(i32, i64)>> {
        Ok(sqlx::query_as(
            "SELECT c.artist_id, sum(c.plays)::bigint FROM user_artists c JOIN guild_members gm ON gm.user_id = c.user_id AND gm.guild_hash = $1
             GROUP BY c.artist_id ORDER BY 2 DESC, 1 LIMIT $2",
        )
        .bind(self.1.id(guild)).bind(limit).fetch_all(&self.0).await?)
    }

    pub async fn set_tz(&self, user_id: i32, tz: Option<&str>) -> Result<()> {
        sqlx::query("UPDATE users SET tz = $2 WHERE id = $1").bind(user_id).bind(tz).execute(&self.0).await?;
        Ok(())
    }

    pub async fn set_obscurity(&self, user_id: i32, score: i16) -> Result<()> {
        sqlx::query("UPDATE users SET obscurity = $2 WHERE id = $1").bind(user_id).bind(score).execute(&self.0).await?;
        Ok(())
    }

    /// Server members who ran `,obscurity`, most underground first: (user id, name, score).
    pub async fn member_obscurity(&self, guild: i64) -> Result<Vec<(i32, String, i16)>> {
        Ok(sqlx::query_as(
            "SELECT u.id, gm.name, u.obscurity FROM guild_members gm JOIN users u ON u.id = gm.user_id
             WHERE gm.guild_hash = $1 AND u.obscurity IS NOT NULL ORDER BY u.obscurity DESC, u.id",
        )
        .bind(self.1.id(guild)).fetch_all(&self.0).await?)
    }

    async fn intern_child<'a>(
        &self,
        table: &str,
        pairs: impl Iterator<Item = (i32, &'a str)>,
    ) -> Result<HashMap<(i32, String), i32>> {
        let set: HashSet<(i32, &str)> = pairs.collect();
        let (ids, names): (Vec<i32>, Vec<&str>) = set.into_iter().unzip();
        sqlx::query(AssertSqlSafe(format!(
            "INSERT INTO {table} (artist_id, name) SELECT * FROM unnest($1::int[], $2::text[]) ON CONFLICT DO NOTHING"
        )))
        .bind(&ids).bind(&names).execute(&self.0).await?;
        let rows: Vec<(i32, String, i32)> = sqlx::query_as(AssertSqlSafe(format!(
            "SELECT x.a, x.n, t.id FROM unnest($1::int[], $2::text[]) x(a, n)
             JOIN {table} t ON t.artist_id = x.a AND lower(t.name) = lower(x.n)"
        )))
        .bind(&ids).bind(&names).fetch_all(&self.0).await?;
        Ok(rows.into_iter().map(|(a, n, id)| ((a, n), id)).collect())
    }

    /// Adds new listens (all after `last_ts`) to the history and counters. `replace` = delete the old data first.
    pub async fn store(&self, user_id: i32, mut entries: Vec<Entry>, replace: bool) -> Result<()> {
        entries.sort_unstable_by_key(|e| e.ts);
        let mut tx = self.0.begin().await?;
        if replace {
            clear_listening_data(&mut tx, user_id).await?;
        }
        if entries.is_empty() {
            return Ok(tx.commit().await?);
        }

        // Open the last chunk and write it back together with the new listens.
        let last: Option<(i32, Vec<u8>)> =
            sqlx::query_as("SELECT seq, data FROM history WHERE user_id = $1 ORDER BY seq DESC LIMIT 1")
                .bind(user_id).fetch_optional(&mut *tx).await?;
        let (mut seq, mut pending) = match last {
            Some((seq, data)) => (seq, history::decode(&data)?),
            None => (0, Vec::new()),
        };
        // Delta encoding needs ascending order; listens at a time that is already stored are dropped.
        let newest_stored = pending.last().map_or(i64::MIN, |e| e.ts);
        entries.retain(|e| e.ts > newest_stored);
        if entries.is_empty() {
            return Ok(tx.commit().await?);
        }
        pending.extend_from_slice(&entries);
        for chunk in pending.chunks(CHUNK) {
            sqlx::query("INSERT INTO history (user_id, seq, data) VALUES ($1, $2, $3)
                         ON CONFLICT (user_id, seq) DO UPDATE SET data = EXCLUDED.data")
                .bind(user_id).bind(seq).bind(history::encode(chunk)).execute(&mut *tx).await?;
            seq += 1;
        }

        let mut tracks: HashMap<i32, i32> = HashMap::new();
        let mut albums: HashMap<i32, i32> = HashMap::new();
        for e in &entries {
            *tracks.entry(e.track).or_default() += 1;
            if e.album != 0 {
                *albums.entry(e.album).or_default() += 1;
            }
        }
        bump(&mut tx, "user_tracks", "track_id", user_id, &tracks).await?;
        bump(&mut tx, "user_albums", "album_id", user_id, &albums).await?;
        let track_ids: Vec<i32> = tracks.keys().copied().collect();
        let track_plays: Vec<i32> = track_ids.iter().map(|t| tracks[t]).collect();
        sqlx::query(
            "INSERT INTO user_artists (artist_id, user_id, plays)
             SELECT t.artist_id, $1, sum(x.p) FROM unnest($2::int[], $3::int[]) x(t, p) JOIN tracks t ON t.id = x.t
             GROUP BY t.artist_id
             ON CONFLICT (artist_id, user_id) DO UPDATE SET plays = user_artists.plays + EXCLUDED.plays",
        )
        .bind(user_id).bind(&track_ids).bind(&track_plays).execute(&mut *tx).await?;

        let newest = entries.last().map(|e| e.ts).unwrap_or(0);
        sqlx::query("UPDATE users SET last_ts = greatest(last_ts, $2), listens = listens + $3 WHERE id = $1")
            .bind(user_id).bind(newest).bind(entries.len() as i32).execute(&mut *tx).await?;
        Ok(tx.commit().await?)
    }

    /// `count_offset`: the lasting difference between LB's count and what we store.
    pub async fn finish_index(&self, user_id: i32, lb_count: i64) -> Result<()> {
        // The token was only needed for the export; the bot keeps no user credentials after this.
        sqlx::query("UPDATE users SET indexed = true, count_offset = $2 - listens, lb_token = NULL WHERE id = $1")
            .bind(user_id).bind(lb_count as i32).execute(&self.0).await?;
        Ok(())
    }

    /// So that an index cut off by a restart is queued again.
    pub async fn mark_unindexed(&self, user_id: i32) -> Result<()> {
        sqlx::query("UPDATE users SET indexed = false WHERE id = $1").bind(user_id).execute(&self.0).await?;
        Ok(())
    }

    /// Artist id for `artist`; for an album or track, the id of the (artist, name) pair.
    /// A typed query is made canonical the same way as listens (see `lb::TrackMetadata`).
    pub async fn find(&self, kind: Kind, artist: &str, name: Option<&str>) -> Result<Option<(i32, String)>> {
        let artist = &crate::lb::artist_key(artist);
        let name = name.map(|n| if matches!(kind, Kind::Album) { crate::lb::album_key(n) } else { crate::lb::title_key(n) });
        let name = name.as_deref();
        let row = match (kind, name) {
            (Kind::Artist, _) => {
                sqlx::query_as("SELECT id, name FROM artists WHERE lower(name) = lower($1)").bind(artist).fetch_optional(&self.0).await?
            }
            (Kind::Album | Kind::Track, Some(name)) => {
                let table = if matches!(kind, Kind::Album) { "albums" } else { "tracks" };
                sqlx::query_as(AssertSqlSafe(format!(
                    "SELECT t.id, a.name || ' - ' || t.name FROM {table} t JOIN artists a ON a.id = t.artist_id
                     WHERE lower(a.name) = lower($1) AND t.artist_id = a.id AND lower(t.name) = lower($2)"
                )))
                .bind(artist).bind(name).fetch_optional(&self.0).await?
            }
            _ => None,
        };
        Ok(row)
    }

    /// Server WhoKnows when `guild` is given, otherwise bot-wide (private users excluded).
    pub async fn who_knows(&self, kind: Kind, id: i32, guild: Option<i64>) -> Result<Vec<WkRow>> {
        let (table, col) = counter_table(kind);
        let sql = match guild {
            Some(_) => format!(
                "SELECT gm.name, u.lb_name, u.discord_hash, c.plays, count(*) OVER () AS listeners, sum(c.plays) OVER () AS total
                 FROM {table} c JOIN guild_members gm ON gm.user_id = c.user_id AND gm.guild_hash = $2
                 JOIN users u ON u.id = c.user_id
                 WHERE c.{col} = $1 ORDER BY c.plays DESC, u.id LIMIT 15"
            ),
            None => format!(
                "SELECT u.lb_name AS name, u.lb_name, u.discord_hash, c.plays, count(*) OVER () AS listeners, sum(c.plays) OVER () AS total
                 FROM {table} c JOIN users u ON u.id = c.user_id
                 WHERE c.{col} = $1 AND (NOT u.private OR $2::bigint IS NOT NULL) ORDER BY c.plays DESC, u.id LIMIT 15"
            ),
        };
        Ok(sqlx::query_as(AssertSqlSafe(sql)).bind(id).bind(guild.map(|g| self.1.id(g))).fetch_all(&self.0).await?)
    }

    pub async fn plays(&self, kind: Kind, id: i32, user_id: i32) -> Result<i32> {
        let (table, col) = counter_table(kind);
        Ok(sqlx::query_scalar(AssertSqlSafe(format!("SELECT plays FROM {table} WHERE {col} = $1 AND user_id = $2")))
            .bind(id).bind(user_id).fetch_optional(&self.0).await?.unwrap_or(0))
    }

    /// A user's whole history, oldest first.
    // shortcut: decodes every chunk on each command (~25 for 100k listens); read only the newest chunks for short periods if this gets slow.
    pub async fn history(&self, user_id: i32) -> Result<Vec<Entry>> {
        let chunks: Vec<Vec<u8>> = sqlx::query_scalar("SELECT data FROM history WHERE user_id = $1 ORDER BY seq")
            .bind(user_id).fetch_all(&self.0).await?;
        let mut out = Vec::with_capacity(chunks.len() * CHUNK);
        for c in &chunks {
            out.extend(history::decode(c)?);
        }
        Ok(out)
    }

    /// The artist of each track.
    pub async fn track_artists(&self, tracks: &[i32]) -> Result<HashMap<i32, i32>> {
        Ok(sqlx::query_as("SELECT id, artist_id FROM tracks WHERE id = ANY($1)").bind(tracks).fetch_all(&self.0).await?.into_iter().collect())
    }

    /// Display names: the artist, or `Artist - Name` for albums and tracks.
    pub async fn names(&self, kind: Kind, ids: &[i32]) -> Result<HashMap<i32, String>> {
        if kind == Kind::Artist {
            return Ok(sqlx::query_as("SELECT id, name FROM artists WHERE id = ANY($1)").bind(ids).fetch_all(&self.0).await?.into_iter().collect());
        }
        Ok(self.name_parts(kind, ids).await?.into_iter().map(|(id, (artist, name))| (id, format!("{artist} - {name}"))).collect())
    }

    /// (artist, name) of albums or tracks.
    pub async fn name_parts(&self, kind: Kind, ids: &[i32]) -> Result<HashMap<i32, (String, String)>> {
        let table = if kind == Kind::Album { "albums" } else { "tracks" };
        let sql = format!("SELECT t.id, a.name, t.name FROM {table} t JOIN artists a ON a.id = t.artist_id WHERE t.id = ANY($1)");
        let rows: Vec<(i32, String, String)> = sqlx::query_as(AssertSqlSafe(sql)).bind(ids).fetch_all(&self.0).await?;
        Ok(rows.into_iter().map(|(id, artist, name)| (id, (artist, name))).collect())
    }

    /// Server members' plays of the given artists, with their name and total listens: (user id, name, total, artist, plays).
    /// Reads the counters by artist (their key), since they have no user index.
    pub async fn member_artist_plays(&self, guild: i64, artists: &[i32]) -> Result<Vec<(i32, String, i64, i32, i64)>> {
        Ok(sqlx::query_as(
            "SELECT u.id, gm.name, u.listens::bigint, c.artist_id, c.plays::bigint
             FROM user_artists c JOIN guild_members gm ON gm.user_id = c.user_id AND gm.guild_hash = $1
             JOIN users u ON u.id = c.user_id WHERE c.artist_id = ANY($2)",
        )
        .bind(self.1.id(guild)).bind(artists).fetch_all(&self.0).await?)
    }

    /// Whether the bot has this registered user in the server's member list.
    pub async fn is_member(&self, guild: i64, user_id: i32) -> Result<bool> {
        Ok(sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM guild_members WHERE guild_hash = $1 AND user_id = $2)")
            .bind(self.1.id(guild)).bind(user_id).fetch_one(&self.0).await?)
    }

    /// Stores one chunk of a server's members (only users registered with the bot are kept).
    pub async fn add_members(&self, guild: i64, members: &[(i64, String)]) -> Result<()> {
        let (ids, names): (Vec<i64>, Vec<&str>) = members.iter().map(|(i, n)| (self.1.id(*i), n.as_str())).unzip();
        sqlx::query(
            "INSERT INTO guild_members (guild_hash, user_id, name)
             SELECT $1, u.id, m.n FROM unnest($2::bigint[], $3::text[]) m(d, n) JOIN users u ON u.discord_hash = m.d
             ON CONFLICT (guild_hash, user_id) DO UPDATE SET name = EXCLUDED.name WHERE guild_members.name <> EXCLUDED.name",
        )
        .bind(self.1.id(guild)).bind(&ids).bind(&names).execute(&self.0).await?;
        Ok(())
    }

    pub async fn remove_member(&self, guild: i64, discord_id: i64) -> Result<()> {
        sqlx::query("DELETE FROM guild_members WHERE guild_hash = $1 AND user_id = (SELECT id FROM users WHERE discord_hash = $2)")
            .bind(self.1.id(guild)).bind(self.1.id(discord_id)).execute(&self.0).await?;
        Ok(())
    }

    pub async fn clear_guild(&self, guild: i64) -> Result<()> {
        sqlx::query("DELETE FROM guild_members WHERE guild_hash = $1").bind(self.1.id(guild)).execute(&self.0).await?;
        Ok(())
    }

    /// The bot was removed from a server: drop its member list and prefix.
    pub async fn forget_guild(&self, guild: i64) -> Result<()> {
        let mut tx = self.0.begin().await?;
        sqlx::query("DELETE FROM guild_members WHERE guild_hash = $1").bind(self.1.id(guild)).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM guilds WHERE guild_hash = $1").bind(self.1.id(guild)).execute(&mut *tx).await?;
        Ok(tx.commit().await?)
    }

    pub async fn prefix(&self, guild: i64) -> Result<Option<String>> {
        Ok(sqlx::query_scalar("SELECT prefix FROM guilds WHERE guild_hash = $1").bind(self.1.id(guild)).fetch_optional(&self.0).await?)
    }

    pub async fn set_prefix(&self, guild: i64, prefix: &str) -> Result<()> {
        sqlx::query("INSERT INTO guilds (guild_hash, prefix) VALUES ($1, $2) ON CONFLICT (guild_hash) DO UPDATE SET prefix = $2")
            .bind(self.1.id(guild)).bind(prefix).execute(&self.0).await?;
        Ok(())
    }
}

fn counter_table(kind: Kind) -> (&'static str, &'static str) {
    match kind {
        Kind::Artist => ("user_artists", "artist_id"),
        Kind::Album => ("user_albums", "album_id"),
        Kind::Track => ("user_tracks", "track_id"),
    }
}

async fn bump(tx: &mut Tx<'_>, table: &str, col: &str, user_id: i32, counts: &HashMap<i32, i32>) -> Result<()> {
    let (ids, plays): (Vec<i32>, Vec<i32>) = counts.iter().map(|(k, v)| (*k, *v)).unzip();
    sqlx::query(AssertSqlSafe(format!(
        "INSERT INTO {table} ({col}, user_id, plays) SELECT x.i, $1, x.p FROM unnest($2::int[], $3::int[]) x(i, p)
         ON CONFLICT ({col}, user_id) DO UPDATE SET plays = {table}.plays + EXCLUDED.plays"
    )))
    .bind(user_id).bind(&ids).bind(&plays).execute(&mut **tx).await?;
    Ok(())
}

/// The counter tables have no user_id index (to save space); the keys to delete come from the user's history.
async fn clear_listening_data(tx: &mut Tx<'_>, user_id: i32) -> Result<()> {
    let chunks: Vec<Vec<u8>> = sqlx::query_scalar("SELECT data FROM history WHERE user_id = $1")
        .bind(user_id).fetch_all(&mut **tx).await?;
    let (mut tracks, mut albums) = (HashSet::new(), HashSet::new());
    for c in &chunks {
        for e in history::decode(c)? {
            tracks.insert(e.track);
            albums.insert(e.album);
        }
    }
    let tracks: Vec<i32> = tracks.into_iter().collect();
    let albums: Vec<i32> = albums.into_iter().collect();
    sqlx::query("DELETE FROM user_tracks WHERE user_id = $1 AND track_id = ANY($2)").bind(user_id).bind(&tracks).execute(&mut **tx).await?;
    sqlx::query("DELETE FROM user_albums WHERE user_id = $1 AND album_id = ANY($2)").bind(user_id).bind(&albums).execute(&mut **tx).await?;
    sqlx::query(
        "DELETE FROM user_artists WHERE user_id = $1 AND artist_id IN (SELECT DISTINCT artist_id FROM tracks WHERE id = ANY($2))",
    )
    .bind(user_id).bind(&tracks).execute(&mut **tx).await?;
    sqlx::query("DELETE FROM history WHERE user_id = $1").bind(user_id).execute(&mut **tx).await?;
    sqlx::query("UPDATE users SET listens = 0, obscurity = NULL WHERE id = $1").bind(user_id).execute(&mut **tx).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lb::{Listen, TrackMetadata};

    fn listen(ts: i64, artist: &str, track: &str, album: Option<&str>) -> Listen {
        Listen {
            listened_at: ts as f64,
            recording_msid: None,
            track_metadata: TrackMetadata {
                artist_name: artist.into(),
                track_name: track.into(),
                release_name: album.map(Into::into),
                ..Default::default()
            },
        }
    }

    /// Needs a real Postgres: skipped when `TEST_DATABASE_URL` is not set.
    #[tokio::test]
    async fn store_who_knows_and_clear() -> Result<()> {
        let Ok(url) = std::env::var("TEST_DATABASE_URL") else { return Ok(()) };
        let db = connect(&url, Arc::new(crate::crypto::test_crypto())).await?;
        let a = db.upsert_user(1, "alice", "token-a").await?;
        let b = db.upsert_user(2, "bob", "token-b").await?;
        assert_eq!(db.token(&db.user(a).await?)?.as_deref(), Some("token-a"));
        let raw: i64 = sqlx::query_scalar("SELECT count(*) FROM users WHERE discord_hash IN (1, 2)").fetch_one(&db.0).await?;
        assert_eq!(raw, 0, "raw discord ids must not be stored");
        db.add_members(10, &[(1, "Alice".into()), (2, "Bob".into())]).await?;

        let first = vec![listen(100, "Radiohead", "Creep", Some("Pablo Honey")), listen(200, "radiohead", "creep", None)];
        db.store(a, db.intern(&first).await?, true).await?;
        db.store(a, db.intern(&[listen(300, "Radiohead", "Airbag", Some("OK Computer"))]).await?, false).await?;
        // A repeated timestamp is not counted.
        db.store(a, db.intern(&[listen(300, "Radiohead", "Airbag", Some("OK Computer"))]).await?, false).await?;
        db.store(b, db.intern(&[listen(150, "Radiohead", "Creep", None)]).await?, true).await?;

        let (artist, _) = db.find(Kind::Artist, "RADIOHEAD", None).await?.unwrap();
        let rows = db.who_knows(Kind::Artist, artist, Some(10)).await?;
        assert_eq!(rows.iter().map(|r| (r.name.as_str(), r.plays)).collect::<Vec<_>>(), [("Alice", 3), ("Bob", 1)]);
        assert_eq!((rows[0].listeners, rows[0].total), (2, 4));
        let (track, _) = db.find(Kind::Track, "radiohead", Some("CREEP")).await?.unwrap();
        assert_eq!(db.plays(Kind::Track, track, a).await?, 2);
        // History commands read the blobs back and name what they count.
        assert_eq!(db.history(a).await?.iter().map(|e| e.ts).collect::<Vec<_>>(), [100, 200, 300]);
        assert_eq!(db.track_artists(&[track]).await?[&track], artist);
        assert!(db.names(Kind::Track, &[track]).await?[&track].eq_ignore_ascii_case("Radiohead - Creep"));
        assert!(db.names(Kind::Artist, &[artist]).await?[&artist].eq_ignore_ascii_case("Radiohead"));
        let mut plays = db.member_artist_plays(10, &[artist]).await?;
        plays.sort();
        assert_eq!(plays, [(a, "Alice".into(), 3, artist, 3), (b, "Bob".into(), 1, artist, 1)]);
        assert!(db.is_member(10, b).await? && !db.is_member(11, b).await?);
        // A hand-typed query is cleaned the same way as stored listens.
        assert_eq!(db.find(Kind::Track, "Radiohead feat. X", Some("Creep (Remastered)")).await?.map(|r| r.0), Some(track));
        assert_eq!(db.user(a).await?.last_ts, 300);
        assert_eq!(db.user(a).await?.listens, 3, "duplicate timestamp must not be counted");
        db.finish_index(a, 5).await?;
        assert_eq!(db.user(a).await?.count_offset, 2);
        assert_eq!(db.user(a).await?.lb_token, None, "the token is deleted once the first index is done");
        // Only users who listened in the last 30 days get the background refresh.
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs() as i64;
        sqlx::query("UPDATE users SET indexed = true WHERE id = $1").bind(b).execute(&db.0).await?;
        db.store(b, db.intern(&[listen(now, "Radiohead", "Creep", None)]).await?, false).await?;
        let active: Vec<i32> = db.active_users().await?.iter().map(|u| u.id).collect();
        assert!(active.contains(&b) && !active.contains(&a), "{active:?}");

        db.delete_user(a).await?;
        let rows = db.who_knows(Kind::Artist, artist, None).await?;
        assert_eq!(rows.iter().map(|r| r.plays).collect::<Vec<_>>(), [2], "only bob is left, with his two listens");
        let left: i64 = sqlx::query_scalar("SELECT count(*) FROM user_tracks WHERE user_id = $1").bind(a).fetch_one(&db.0).await?;
        assert_eq!(left, 0);
        Ok(())
    }

    /// Ids and lengths that come with listens are kept; lookups and facts fill in the rest. Needs Postgres.
    #[tokio::test]
    async fn mapping_facts_and_settings() -> Result<()> {
        let Ok(url) = std::env::var("TEST_DATABASE_URL") else { return Ok(()) };
        let db = connect(&url, Arc::new(crate::crypto::test_crypto())).await?;
        // Names are new on every run: the name tables are shared and keep what earlier runs found.
        let tag = std::process::id();
        let (mapped_artist, other_artist) = (format!("Mapped {tag}"), format!("Unmapped {tag}"));
        let u = db.upsert_user(201, "carol", "token-c").await?;
        db.add_members(210, &[(201, "Carol".into())]).await?;
        let (artist_mbid, recording_mbid) = ("8f6bd1e4-fbe1-4f50-aa9b-94c450ec0f11", "e97f805a-ab48-4c52-855e-07049142113d");
        let mut mapped = listen(100, &mapped_artist, "Song", None);
        mapped.track_metadata.artist_mbid = Some(artist_mbid.into());
        mapped.track_metadata.recording_mbid = Some(recording_mbid.into());
        let mut timed = listen(200, &other_artist, "Tune", None);
        timed.track_metadata.length_ms = Some(180_000);
        let listens = [mapped, timed, listen(300, &other_artist, "Tune", None), listen(400, &other_artist, "Other", None)];
        db.store(u, db.intern(&listens).await?, true).await?;

        let (a, _) = db.find(Kind::Artist, &mapped_artist, None).await?.unwrap();
        let (o, _) = db.find(Kind::Artist, &other_artist, None).await?.unwrap();
        let (song, _) = db.find(Kind::Track, &mapped_artist, Some("Song")).await?.unwrap();
        let (tune, _) = db.find(Kind::Track, &other_artist, Some("Tune")).await?.unwrap();
        let (lone, _) = db.find(Kind::Track, &other_artist, Some("Other")).await?.unwrap();
        assert_eq!(db.artist_mbids(&[a, o]).await?, HashMap::from([(a, artist_mbid.to_owned())]));
        let facts = db.track_facts(&[song, tune, lone]).await?;
        assert_eq!(facts[&song], TrackFacts { mbid: Some(recording_mbid.into()), length_ms: None, released: None });
        assert_eq!(facts[&tune], TrackFacts { mbid: None, length_ms: Some(180_000), released: None });

        // A lookup that finds nothing marks the track as looked up; one that finds an id leaves its facts to fetch.
        let found = "13dd61c7-0000-4000-8000-000000000001";
        db.set_track_mbids(&[tune, lone], &[None, Some(found.into())]).await?;
        db.set_track_facts(&[song], &[crate::lb::RecordingFacts { length_ms: 200_000, released: 19_940_822 }]).await?;
        let facts = db.track_facts(&[song, tune, lone]).await?;
        assert_eq!(facts[&song], TrackFacts { mbid: Some(recording_mbid.into()), length_ms: Some(200_000), released: Some(19_940_822) });
        assert_eq!(facts[&tune], TrackFacts { mbid: None, length_ms: Some(180_000), released: Some(0) });
        assert_eq!(facts[&lone], TrackFacts { mbid: Some(found.into()), length_ms: None, released: None });

        assert_eq!(db.artist_top_tracks(&[o]).await?[&o], tune, "two plays beat one");
        assert_eq!(db.server_top_artists(210, 5).await?, [(o, 3), (a, 1)]);
        db.set_tz(u, Some("Europe/Istanbul")).await?;
        assert_eq!(db.user(u).await?.tz.as_deref(), Some("Europe/Istanbul"));
        db.set_obscurity(u, 63).await?;
        assert_eq!(db.member_obscurity(210).await?, [(u, "Carol".to_owned(), 63)]);
        db.delete_user(u).await?;
        Ok(())
    }

    /// Rows written before migration 0003 (raw ids, old token format) are converted at startup and still work.
    /// Uses its own database, because the conversion rewrites every row.
    #[tokio::test]
    async fn old_rows_get_hashed_ids_and_new_tokens() -> Result<()> {
        let Ok(url) = std::env::var("TEST_DATABASE_URL") else { return Ok(()) };
        let name = format!("lbbot_migration_test_{}", std::process::id());
        let admin = PgPoolOptions::new().max_connections(1).connect(&url).await?;
        sqlx::query(AssertSqlSafe(format!("DROP DATABASE IF EXISTS {name}"))).execute(&admin).await?;
        sqlx::query(AssertSqlSafe(format!("CREATE DATABASE {name}"))).execute(&admin).await?;
        let (base, _) = url.rsplit_once('/').expect("database url has a path");
        let db = connect(&format!("{base}/{name}"), Arc::new(crate::crypto::test_crypto())).await?;

        let (discord, guild) = (555_000_000_000_000_001i64, 555_000_000_000_000_002i64);
        let id: i32 = sqlx::query_scalar("INSERT INTO users (discord_hash, lb_name, lb_token) VALUES ($1, 'old', $2) RETURNING id")
            .bind(discord).bind(db.1.encrypt_legacy("old-token")).fetch_one(&db.0).await?;
        sqlx::query("INSERT INTO guild_members VALUES ($1, $2, 'Old')").bind(guild).bind(id).execute(&db.0).await?;
        sqlx::query("INSERT INTO guilds VALUES ($1, '!')").bind(guild).execute(&db.0).await?;
        sqlx::query("DELETE FROM app_state").execute(&db.0).await?;

        db.migrate_hashed_ids().await?;
        db.migrate_hashed_ids().await?; // runs once: a second start must not hash the ids again
        let other = Db(db.0.clone(), Arc::new(crate::crypto::Crypto::from_base64("HxwdHh8AAQIDBAUGBwgJCgsMDQ4PEBESExQVFhcYGRo=")?));
        assert!(other.migrate_hashed_ids().await.is_err(), "a different TOKEN_KEY must be refused");
        let user = db.user_by_discord(discord).await?.expect("user found by its raw discord id");
        assert_eq!((user.id, user.discord_hash), (id, db.1.id(discord)));
        assert_eq!(db.token(&user)?.as_deref(), Some("old-token"));
        assert_eq!(db.prefix(guild).await?.as_deref(), Some("!"));
        let members: i64 = sqlx::query_scalar("SELECT count(*) FROM guild_members WHERE guild_hash = $1").bind(db.1.id(guild)).fetch_one(&db.0).await?;
        assert_eq!(members, 1);

        db.0.close().await;
        sqlx::query(AssertSqlSafe(format!("DROP DATABASE {name}"))).execute(&admin).await?;
        Ok(())
    }
}
