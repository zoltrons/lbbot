//! MusicBrainz ids, track facts and ListenBrainz popularity for the stats commands. Ids usually come with the
//! listens (`Db::intern`); what's missing is looked up by name the first time a command needs it, and stored.
//! Popularity changes, so it is only cached in memory, for a day like the card images.

use std::collections::HashMap;

use anyhow::Result;

use crate::{
    bot::{Bot, cache_get, cache_put},
    db::{Kind, TrackFacts},
    lb::uuid,
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
            let artists = db.track_artists(&unknown).await?;
            for chunk in unknown.chunks(50) {
                let items: Vec<(&str, &str, Option<&str>)> =
                    chunk.iter().map(|t| names.get(t).map_or(("", "", None), |(a, n)| (a.as_str(), n.as_str(), None))).collect();
                let found = lb.lookup_many(&items).await?;
                let mbids: Vec<Option<String>> = found.iter().map(|r| uuid(&r["recording_mbid"])).collect();
                db.set_track_mbids(chunk, &mbids).await?;
                let (ids, artist_mbids): (Vec<i32>, Vec<String>) =
                    chunk.iter().zip(&found).filter_map(|(t, r)| Some((*artists.get(t)?, uuid(&r["artist_mbids"][0])?))).unzip();
                db.set_artist_mbids(&ids, &artist_mbids.iter().map(String::as_str).collect::<Vec<_>>()).await?;
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
            let ids: Vec<i32> = chunk.iter().map(|c| c.0).collect();
            // A recording LB doesn't return (merged into another) is stored as unknown, so it isn't asked again.
            let rows: Vec<crate::lb::RecordingFacts> = chunk.iter().map(|c| got.get(c.1).copied().unwrap_or_default()).collect();
            db.set_track_facts(&ids, &rows).await?;
        }
        db.track_facts(tracks).await
    }

    /// MusicBrainz ids of `artists`. Up to `LOOKUPS` artists without one are looked up by their most played track;
    /// the ones found are stored. Artists LB can't match are left out (and tried again next time).
    pub(crate) async fn artist_mbids(&self, artists: &[i32]) -> Result<HashMap<i32, String>> {
        let (db, lb) = (self.db(), &self.ingest.lb);
        let mut known = db.artist_mbids(artists).await?;
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
            let (mut artist_rows, mut track_rows) = ((Vec::new(), Vec::new()), (Vec::new(), Vec::new()));
            for (&(a, t), r) in chunk.iter().zip(&found) {
                if let Some(m) = uuid(&r["artist_mbids"][0]) {
                    known.insert(a, m.clone());
                    artist_rows.0.push(a);
                    artist_rows.1.push(m);
                }
                // A miss here doesn't mark the track: it was looked up for its artist, not for itself.
                if let Some(m) = uuid(&r["recording_mbid"]) {
                    track_rows.0.push(t);
                    track_rows.1.push(Some(m));
                }
            }
            db.set_artist_mbids(&artist_rows.0, &artist_rows.1.iter().map(String::as_str).collect::<Vec<_>>()).await?;
            db.set_track_mbids(&track_rows.0, &track_rows.1).await?;
        }
        Ok(known)
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
