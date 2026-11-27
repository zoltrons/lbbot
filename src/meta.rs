//! MusicBrainz ids, track facts and ListenBrainz popularity for the stats commands. Ids usually come with the
//! listens (`Db::intern`); what's missing is looked up by name the first time a command needs it, and stored.
//! Popularity changes, so it is only cached in memory, for a day like the card images.

use std::collections::HashMap;

use anyhow::Result;
use serde_json::Value;

use crate::{
    bot::{Bot, cache_get, cache_put},
    db::{Kind, TrackFacts},
    lb::{artist_key, uuid},
};

/// Tracks or artists looked up by name per command: LB takes 50 per request.
const LOOKUPS: usize = 200;
/// Tracks whose facts are fetched per command: LB takes 1000 per request.
const FACTS: usize = 5000;

impl Bot {
    /// Facts of `tracks`, most important first. Looks up at most `LOOKUPS` tracks without an id by name and
    /// fetches facts for at most `FACTS` tracks; the rest stay unknown until a later command.
    pub(crate) async fn track_facts(&self, tracks: &[i32]) -> Result<HashMap<i32, TrackFacts>> {
        let (db, lb) = (self.db(), &self.ingest.lb);
        let facts = db.track_facts(tracks).await?;
        let unknown: Vec<i32> =
            tracks.iter().copied().filter(|t| facts.get(t).is_some_and(|f| f.mbid.is_none() && f.released.is_none())).take(LOOKUPS).collect();
        if !unknown.is_empty() {
            let names = db.name_parts(Kind::Track, &unknown).await?;
            for chunk in unknown.chunks(50) {
                let items: Vec<(&str, &str, Option<&str>)> =
                    chunk.iter().map(|t| names.get(t).map_or(("", "", None), |(a, n)| (a.as_str(), n.as_str(), None))).collect();
                let found = lb.lookup_many(&items).await?;
                self.save_found(chunk, &found).await?;
                // A track LB can't match is marked as looked up, so it isn't asked again.
                let missed: Vec<i32> = chunk.iter().zip(&found).filter(|(_, r)| uuid(&r["recording_mbid"]).is_none()).map(|(t, _)| *t).collect();
                db.set_track_mbids(&missed, &vec![None; missed.len()]).await?;
            }
        }
        let facts = if unknown.is_empty() { facts } else { db.track_facts(tracks).await? };
        let todo: Vec<(i32, &str)> = tracks
            .iter()
            .filter_map(|t| facts.get(t).filter(|f| f.released.is_none()).and_then(|f| Some((*t, f.mbid.as_deref()?))))
            .take(FACTS)
            .collect();
        if todo.is_empty() {
            return Ok(facts);
        }
        for chunk in todo.chunks(1000) {
            let mbids: Vec<&str> = chunk.iter().map(|c| c.1).collect();
            let got = lb.recording_facts(&mbids).await?;
            // A recording LB doesn't return stays unasked-for and is asked again by a later command: LB may not have
            // a new release yet. (A merged recording keeps being asked; it rides along in requests made anyway.)
            let (ids, rows): (Vec<i32>, Vec<crate::lb::RecordingFacts>) = chunk.iter().filter_map(|c| Some((c.0, *got.get(c.1)?))).unzip();
            db.set_track_facts(&ids, &rows).await?;
        }
        db.track_facts(tracks).await
    }

    /// MusicBrainz ids of `artists`. Up to `LOOKUPS` artists without one are looked up by their most played track;
    /// the ones found are stored. Artists LB can't match are left out (and tried again next time).
    pub(crate) async fn artist_mbids(&self, artists: &[i32]) -> Result<HashMap<i32, String>> {
        let (db, lb) = (self.db(), &self.ingest.lb);
        let known = db.artist_mbids(artists).await?;
        let missing: Vec<i32> = artists.iter().copied().filter(|a| !known.contains_key(a)).take(LOOKUPS).collect();
        if missing.is_empty() {
            return Ok(known);
        }
        let top = db.artist_top_tracks(&missing).await?;
        let pairs: Vec<(i32, i32)> = missing.iter().filter_map(|a| Some((*a, *top.get(a)?))).collect();
        let names = db.name_parts(Kind::Track, &pairs.iter().map(|p| p.1).collect::<Vec<_>>()).await?;
        for chunk in pairs.chunks(50) {
            let items: Vec<(&str, &str, Option<&str>)> =
                chunk.iter().map(|(_, t)| names.get(t).map_or(("", "", None), |(a, n)| (a.as_str(), n.as_str(), None))).collect();
            let found = lb.lookup_many(&items).await?;
            // A miss doesn't mark the track: it was looked up for its artist, not for itself.
            self.save_found(&chunk.iter().map(|p| p.1).collect::<Vec<_>>(), &found).await?;
        }
        // What the database kept: an id stored meanwhile (by a listen) wins over this lookup's.
        db.artist_mbids(artists).await
    }

    /// Stores what a name lookup found for `tracks` (`found` in the same order, from `Lb::lookup_many`): each
    /// track's recording id and its artist's id, where they have none yet. Every lookup in the bot comes through
    /// here, so the stats, charts and cards share what any of them found.
    pub(crate) async fn save_found(&self, tracks: &[i32], found: &[Value]) -> Result<()> {
        let db = self.db();
        let artists = db.track_artists(tracks).await?;
        let names = db.names(Kind::Artist, &artists.values().copied().collect::<Vec<_>>()).await?;
        let (mut track_ids, mut recordings, mut artist_ids, mut artist_mbids) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for (t, r) in tracks.iter().zip(found) {
            if let Some(m) = uuid(&r["recording_mbid"]) {
                track_ids.push(*t);
                recordings.push(Some(m));
            }
            // An entry named like a whole shared credit ("A & B" from a player without a mapping) is no single
            // artist, so it gets no artist id; the first artist's would lend it A's popularity.
            let credit = artist_key(r["artist_credit_name"].as_str().unwrap_or_default()).to_lowercase();
            let shared = |a: &i32| r["artist_mbids"].as_array().is_some_and(|m| m.len() > 1) && names.get(a).is_some_and(|n| n.to_lowercase() == credit);
            if let (Some(a), Some(m)) = (artists.get(t), uuid(&r["artist_mbids"][0]))
                && !shared(a)
            {
                artist_ids.push(*a);
                artist_mbids.push(m);
            }
        }
        db.set_track_mbids(&track_ids, &recordings).await?;
        db.set_artist_mbids(&artist_ids, &artist_mbids.iter().map(String::as_str).collect::<Vec<_>>()).await
    }

    /// The mapping an artist card finds its artist by: the listen's own, else a lookup by the playing track (the
    /// first id is the main artist, "A feat. B" → A), else the id stored for the name. Namesakes share a stored id,
    /// so the playing track goes first; the stored id saves a name search for a typed name.
    pub(crate) async fn card_artist_mapping(&self, artist: &str, track: Option<&str>, album: Option<&str>, mut mapping: Value) -> Value {
        if mapping["artist_mbids"].get(0).is_none()
            && let Some(t) = track
        {
            match self.ingest.lb.lookup(artist, t, album).await {
                Ok(m) => mapping = m,
                Err(e) => tracing::warn!(error = ?e, "artist lookup failed"),
            }
        }
        if mapping["artist_mbids"].get(0).is_none()
            && let Some(m) = self.stored_artist_mbid(artist).await
        {
            mapping["artist_mbids"] = serde_json::json!([m]);
        }
        mapping
    }

    /// The stored id of an artist, by name as typed or played. `None` when there is none (or the database fails:
    /// callers fall back to searching by name).
    pub(crate) async fn stored_artist_mbid(&self, artist: &str) -> Option<String> {
        let (id, _) = self.db().find(Kind::Artist, artist, None).await.ok()??;
        self.db().artist_mbids(&[id]).await.ok()?.remove(&id)
    }

    /// The stored recording id of a track, by names as typed or played.
    pub(crate) async fn stored_track_mbid(&self, artist: &str, track: &str) -> Option<String> {
        let (id, _) = self.db().find(Kind::Track, artist, Some(track)).await.ok()??;
        self.db().track_facts(&[id]).await.ok()?.remove(&id)?.mbid
    }

    /// LB-wide (listeners, listens) of artists or recordings (`entity`: "artist", "recording") by id. Ids LB has
    /// no stats for yet are left out; that is cached too.
    pub(crate) async fn popularity(&self, entity: &'static str, mbids: &[&str]) -> Result<HashMap<String, (i64, i64)>> {
        let mut out = HashMap::new();
        let mut missing = Vec::new();
        {
            let cache = self.popularity_cache.lock().unwrap();
            for m in mbids {
                match cache_get(&cache, &(entity, (*m).to_owned())) {
                    Some(Some(v)) => {
                        out.insert((*m).to_owned(), v);
                    }
                    Some(None) => {}
                    None => missing.push(*m),
                }
            }
        }
        missing.sort_unstable();
        missing.dedup();
        for chunk in missing.chunks(1000) {
            let got = self.ingest.lb.popularity(entity, chunk).await?;
            let now = std::time::Instant::now();
            let mut cache = self.popularity_cache.lock().unwrap();
            for m in chunk {
                let v = got.get(*m).copied();
                cache_put(&mut cache, (entity, (*m).to_owned()), v, now);
                if let Some(v) = v {
                    out.insert((*m).to_owned(), v);
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use twilight_model::id::Id;

    use super::*;
    use crate::lb::{Listen, TrackMetadata};

    fn leak(s: String) -> &'static str {
        Box::leak(s.into_boxed_str())
    }

    fn bot(db: crate::db::Db, lb: crate::lb::Lb) -> Bot {
        Bot {
            http: Arc::new(twilight_http::Client::new(String::new())),
            app_id: Id::new(1),
            ingest: crate::ingest::Ingest::new(db, lb),
            prefixes: Default::default(),
            guild_names: Default::default(),
            art_cache: Default::default(),
            popularity_cache: Default::default(),
            chart_uses: Default::default(),
            featured: Default::default(),
            owner: None,
        }
    }

    /// Two artists can share a name (and so a stored id). A card for a playing track finds its artist by that
    /// track first; the id stored for the name is for a typed name, or when the track doesn't match.
    #[tokio::test]
    async fn card_prefers_the_playing_track_over_the_name() -> Result<()> {
        let Ok(url) = std::env::var("TEST_DATABASE_URL") else { return Ok(()) };
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let db = crate::db::connect(&url, Arc::new(crate::crypto::test_crypto())).await?;
        let artist = format!("Namesake {}", std::process::id());
        let (stored, playing) = ("8f6bd1e4-fbe1-4f50-aa9b-94c450ec0f11", "5082a11f-7203-4ff3-ae04-2a0150d3bbb6");
        let mut first = Listen { listened_at: 1.0, recording_msid: None, track_metadata: TrackMetadata { artist_name: artist.clone(), track_name: "Theirs".into(), ..Default::default() } };
        first.track_metadata.artist_mbid = Some(stored.into());
        db.intern(&[first]).await?;
        let lookup = leak(format!(r#"{{"artist_credit_name": "{artist}", "artist_mbids": ["{playing}"], "recording_mbid": "e97f805a-ab48-4c52-855e-07049142113d"}}"#));
        let (addr, server) = crate::lb::tests::fake_lb(Box::leak(vec![(200, 10, lookup)].into_boxed_slice())).await;
        let bot = bot(db, crate::lb::Lb::with_base(&format!("http://{addr}"), String::new()));

        let by_track = bot.card_artist_mapping(&artist, Some("Ours"), None, Value::Null).await;
        assert_eq!(by_track["artist_mbids"][0], playing, "the playing track tells which of the two it is");
        let by_name = bot.card_artist_mapping(&artist, None, None, Value::Null).await;
        assert_eq!(by_name["artist_mbids"][0], stored, "a typed name uses the stored id, with no request");
        assert_eq!(server.await?.len(), 1);
        Ok(())
    }

    /// Needs Postgres. The first call looks names up and fetches facts once; after that everything comes from the
    /// database or the cache, with no requests.
    #[tokio::test]
    async fn looks_up_once_then_reads_what_it_stored() -> Result<()> {
        let Ok(url) = std::env::var("TEST_DATABASE_URL") else { return Ok(()) };
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let db = crate::db::connect(&url, Arc::new(crate::crypto::test_crypto())).await?;
        // A new name on every run: the name tables keep what earlier runs stored.
        let artist = format!("Lookup {}", std::process::id());
        let listen = |track: &str| Listen {
            listened_at: 1.0,
            recording_msid: None,
            track_metadata: TrackMetadata { artist_name: artist.clone(), track_name: track.into(), ..Default::default() },
        };
        let entries = db.intern(&[listen("Found"), listen("Missing"), listen("Unknown to LB")]).await?;
        let (found, missing, new) = (entries[0].track, entries[1].track, entries[2].track);
        let (artist_mbid, rec) = ("8f6bd1e4-fbe1-4f50-aa9b-94c450ec0f11", "e97f805a-ab48-4c52-855e-07049142113d");
        let new_rec = "13dd61c7-0000-4000-8000-000000000002";
        let responses = vec![
            // LB matches the first and third track; it has no facts yet for the third.
            (200, 10, leak(format!(
                r#"[{{"index": 0, "artist_credit_name": "{artist}", "artist_mbids": ["{artist_mbid}"], "recording_mbid": "{rec}"}},
                    {{"index": 2, "artist_credit_name": "{artist}", "artist_mbids": ["{artist_mbid}"], "recording_mbid": "{new_rec}"}}]"#
            ))),
            (200, 10, leak(format!(r#"{{"{rec}": {{"recording": {{"length": 238000, "first_release_date": "1994-08-22"}}, "release": {{}}}}}}"#))),
            (200, 10, leak(format!(r#"[{{"recording_mbid": "{rec}", "total_listen_count": 1000, "total_user_count": 10}}]"#))),
        ];
        let (addr, server) = crate::lb::tests::fake_lb(Box::leak(responses.into_boxed_slice())).await;
        let lb = crate::lb::Lb::with_base(&format!("http://{addr}"), String::new());
        let bot = bot(db, lb);

        let mut facts = bot.track_facts(&[found, missing, new]).await?;
        assert_eq!(facts[&found], TrackFacts { mbid: Some(rec.into()), length_ms: Some(238_000), released: Some(19_940_822) });
        assert_eq!(facts[&missing], TrackFacts { mbid: None, length_ms: Some(0), released: Some(0) }, "marked as looked up");
        assert_eq!(facts.remove(&new), Some(TrackFacts { mbid: Some(new_rec.into()), length_ms: None, released: None }), "left to ask again");
        let artist_id = bot.db().track_artists(&[found]).await?[&found];
        assert_eq!(bot.artist_mbids(&[artist_id]).await?[&artist_id], artist_mbid, "stored by the track lookup");
        assert_eq!(bot.popularity("recording", &[rec]).await?[rec], (10, 1000));
        // A fourth request would find the fake server gone and fail.
        assert_eq!(bot.track_facts(&[found, missing]).await?, facts);
        assert_eq!(bot.popularity("recording", &[rec]).await?[rec], (10, 1000));
        assert_eq!(server.await?.len(), 3);

        // Cards and genres read the stored ids by name; a chart's lookup stores what it found.
        assert_eq!(bot.stored_artist_mbid(&artist.to_uppercase()).await.as_deref(), Some(artist_mbid));
        assert_eq!(bot.stored_track_mbid(&artist, "found").await.as_deref(), Some(rec));
        assert_eq!(bot.stored_track_mbid(&artist, "Missing").await, None);
        let chart_found = "13dd61c7-0000-4000-8000-000000000003";
        bot.save_found(&[missing, new], &[serde_json::json!({"recording_mbid": chart_found}), serde_json::Value::Null]).await?;
        assert_eq!(bot.stored_track_mbid(&artist, "Missing").await.as_deref(), Some(chart_found));
        assert_eq!(bot.stored_track_mbid(&artist, "Unknown to LB").await.as_deref(), Some(new_rec), "a miss changes nothing");

        // An entry named like a whole shared credit gets the recording id, but not the first artist's id.
        let combined = format!("{artist} & Someone");
        let duet = bot.db().intern(&[Listen { listened_at: 1.0, recording_msid: None, track_metadata: TrackMetadata { artist_name: combined.clone(), track_name: "Duet".into(), ..Default::default() } }]).await?[0].track;
        let duet_rec = "13dd61c7-0000-4000-8000-000000000004";
        let shared = serde_json::json!({"artist_credit_name": combined, "artist_mbids": [artist_mbid, "5082a11f-7203-4ff3-ae04-2a0150d3bbb6"], "recording_mbid": duet_rec});
        bot.save_found(&[duet], &[shared]).await?;
        assert_eq!(bot.stored_track_mbid(&combined, "Duet").await.as_deref(), Some(duet_rec));
        assert_eq!(bot.stored_artist_mbid(&combined).await, None);
        Ok(())
    }
}
