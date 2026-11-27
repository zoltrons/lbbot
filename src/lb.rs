//! ListenBrainz API client. Every request goes through one gate and follows the X-RateLimit headers:
//! normal user tokens don't raise the limit, which is 30 requests per 10 s per IP.

use std::{
    collections::HashMap,
    path::Path,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use reqwest::{Method, RequestBuilder, Response, StatusCode, Url};
use serde::Deserialize;
use serde_json::Value;
use tokio::{io::AsyncWriteExt, sync::Mutex, time::Instant};

const API: &str = "https://api.listenbrainz.org";
pub const PAGE: usize = 1000;

pub struct Lb {
    http: reqwest::Client,
    base: Url,
    /// Requests left in the current window, and when the window ends.
    gate: Mutex<(u32, Instant)>,
    commands_waiting: AtomicUsize,
    /// The operator's own LB token (`LB_TOKEN`), for metadata endpoints that require one. Users' tokens
    /// are only used for their own export.
    service_token: String,
    /// MusicBrainz asks for 1 request/s per IP: the earliest time for the next request.
    mb_next: Mutex<Instant>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Listen {
    #[serde(default)]
    pub listened_at: f64,
    #[serde(default)]
    pub recording_msid: Option<String>,
    pub track_metadata: TrackMetadata,
}

/// Canonical names, so the same song gets the same key for every user (see `From<RawMetadata>`): the main
/// artist only, the MusicBrainz track title, no feat./remaster/deluxe suffixes. Raw names are not stored.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(from = "RawMetadata")]
pub struct TrackMetadata {
    pub artist_name: String,
    pub track_name: String,
    pub release_name: Option<String>,
    /// MusicBrainz ids from LB's mapping, only when the name above came from the same mapping.
    pub artist_mbid: Option<String>,
    pub recording_mbid: Option<String>,
    /// Track length the player sent.
    pub length_ms: Option<i32>,
}

#[derive(Deserialize)]
struct RawMetadata {
    #[serde(deserialize_with = "clamped")]
    artist_name: String,
    #[serde(deserialize_with = "clamped")]
    track_name: String,
    #[serde(default, deserialize_with = "clamped_opt")]
    release_name: Option<String>,
    /// LB's mapping and the client's free-form field: their shape isn't guaranteed, so they are read as `Value`.
    #[serde(default)]
    mbid_mapping: Value,
    #[serde(default)]
    additional_info: Value,
}

impl From<RawMetadata> for TrackMetadata {
    fn from(r: RawMetadata) -> Self {
        let named = |v: &Value| v.as_str().map(str::trim).filter(|s| !s.is_empty()).map(|s| clamp(s.to_owned()));
        // Priority: the first credit in LB's mapping, then the client's artist list, then the raw credit before "feat.".
        let artist = named(&r.mbid_mapping["artists"][0]["artist_credit_name"])
            .or_else(|| named(&r.additional_info["artist_names"][0]))
            .unwrap_or_else(|| r.artist_name.clone());
        let mapped_track = named(&r.mbid_mapping["recording_name"]);
        let artist_mbid = named(&r.mbid_mapping["artists"][0]["artist_credit_name"]).and_then(|_| uuid(&r.mbid_mapping["artists"][0]["artist_mbid"]));
        let recording_mbid = mapped_track.as_ref().and_then(|_| uuid(&r.mbid_mapping["recording_mbid"]));
        let track = mapped_track.unwrap_or(r.track_name);
        // Players send `duration_ms`, or `duration` in seconds.
        let info = &r.additional_info;
        let ms = info["duration_ms"].as_f64().or_else(|| info["duration"].as_f64().map(|s| s * 1000.0));
        Self {
            artist_name: artist_key(&artist),
            track_name: title_key(&track),
            release_name: r.release_name.map(|a| album_key(&a)),
            artist_mbid,
            recording_mbid,
            length_ms: ms.filter(|ms| (1000.0..=86_400_000.0).contains(ms)).map(|ms| ms as i32),
        }
    }
}

/// A MusicBrainz id in canonical form, lowercase. Anything else is `None`: one bad id would make Postgres refuse
/// the whole batch of listens it is stored with.
pub fn uuid(v: &Value) -> Option<String> {
    let s = v.as_str()?.to_ascii_lowercase();
    let ok = s.len() == 36
        && s.bytes().enumerate().all(|(i, b)| if matches!(i, 8 | 13 | 18 | 23) { b == b'-' } else { b.is_ascii_hexdigit() });
    ok.then_some(s)
}

/// "A feat. B" → "A". `&` and `,` are not split: "Simon & Garfunkel", "Tyler, The Creator".
pub fn artist_key(credit: &str) -> String {
    const FEAT: [&str; 6] = [" feat. ", " feat ", " ft. ", " featuring ", " (feat", " (ft."];
    let credit = apostrophes(credit);
    let lower = credit.to_ascii_lowercase();
    match FEAT.iter().filter_map(|s| lower.find(s)).min() {
        Some(0) | None => credit.trim().to_owned(),
        Some(i) => credit[..i].trim().to_owned(),
    }
}

/// MusicBrainz writes typographic apostrophes ("Don’t"); clients and users type a straight '.
fn apostrophes(s: &str) -> String {
    s.replace(['’', '‘'], "'")
}

/// Strips feat. and remaster suffixes from a track title: "Song (feat. X) - 2011 Remaster" → "Song". Live/remix stay.
pub fn title_key(title: &str) -> String {
    strip_tags(title, &["feat.", "feat ", "ft.", "ft ", "featuring ", "with "], &["remaster"])
}

/// Strips edition suffixes from an album title: "Album (Deluxe Edition)" → "Album".
pub fn album_key(album: &str) -> String {
    strip_tags(album, &[], &["remaster", "deluxe", "edition", "expanded", "anniversary", "bonus track"])
}

/// Removes `(...)`/`[...]` groups and a trailing ` - ...` suffix when the inside starts with `prefixes` or contains `words`.
/// A `words` tag mentioning live/remix/acoustic/demo is never removed, so "Song - Live / Remastered" stays apart from the studio take.
fn strip_tags(name: &str, prefixes: &[&str], words: &[&str]) -> String {
    const KEEP: [&str; 4] = ["live", "remix", "acoustic", "demo"];
    let drop = |inner: &str| {
        let l = inner.trim().to_lowercase();
        // A feat. group only names artists, so the guard applies to `words` alone ("feat. Clive" holds "live").
        prefixes.iter().any(|p| l.starts_with(p)) || (!KEEP.iter().any(|k| l.contains(k)) && words.iter().any(|w| l.contains(w)))
    };
    let name = &apostrophes(name);
    let mut out = String::with_capacity(name.len());
    let mut rest = name.as_str();
    while let Some(i) = rest.find(['(', '[']) {
        let (open, close) = if rest.as_bytes()[i] == b'(' { (b'(', b')') } else { (b'[', b']') };
        // Match the closing bracket at the same depth: "Song (feat. A (B))".
        let mut depth = 0;
        let Some(j) = rest.bytes().enumerate().skip(i).find_map(|(k, b)| {
            depth += (b == open) as i32 - (b == close) as i32;
            (depth == 0).then_some(k)
        }) else {
            break;
        };
        out.push_str(&rest[..i]);
        if !drop(&rest[i + 1..j]) {
            out.push_str(&rest[i..=j]);
        }
        rest = &rest[j + 1..];
    }
    out.push_str(rest);
    if let Some((head, tail)) = out.rsplit_once(" - ")
        && drop(tail)
    {
        out.truncate(head.len());
    }
    let out = out.split_whitespace().collect::<Vec<_>>().join(" ");
    if out.is_empty() { name.trim().to_owned() } else { out }
}

/// LB accepts names up to 10 KB; a Postgres btree index row can't exceed ~2.7 KB.
const MAX_NAME_CHARS: usize = 512;

fn clamp(s: String) -> String {
    match s.char_indices().nth(MAX_NAME_CHARS) {
        Some((cut, _)) => s[..cut].to_owned(),
        None => s,
    }
}

fn clamped<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    String::deserialize(d).map(clamp)
}

fn clamped_opt<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Option::<String>::deserialize(d).map(|o| o.map(clamp))
}

#[derive(Debug, Deserialize)]
pub struct Export {
    pub export_id: i64,
    pub status: String,
    /// Another app may have asked for a date-ranged export; both `None` = full history.
    pub start_time: Option<i64>,
    pub end_time: Option<i64>,
}

/// While a command request waits, background work (index, export) gives it the turn.
#[derive(Clone, Copy, PartialEq)]
pub enum Prio {
    Command,
    Background,
}

/// Counts waiting command requests; decremented when the request ends.
struct Waiting<'a>(&'a AtomicUsize);
impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

impl Lb {
    pub fn new(service_token: String) -> Self {
        Self::with_base(API, service_token)
    }

    pub(crate) fn with_base(base: &str, service_token: String) -> Self {
        let http = reqwest::Client::builder()
            // MusicBrainz asks for a contact address in the User-Agent.
            .user_agent(concat!("lbbot/", env!("CARGO_PKG_VERSION"), " ( https://github.com/zoltrons/lbbot )"))
            .timeout(Duration::from_secs(30))
            .build()
            .expect("reqwest client");
        let base = Url::parse(base).expect("valid base url");
        Self { http, base, gate: Mutex::new((1, Instant::now())), commands_waiting: AtomicUsize::new(0), service_token, mb_next: Mutex::new(Instant::now()) }
    }

    /// Path segments are encoded one by one: MusicBrainz usernames can contain `/`, `?` and `#`.
    fn url(&self, segments: &[&str], query: &[(&str, i64)]) -> Url {
        let mut url = self.base.clone();
        url.path_segments_mut().expect("base url has a path").extend(segments);
        for (k, v) in query {
            url.query_pairs_mut().append_pair(k, &v.to_string());
        }
        url
    }

    fn req(&self, method: Method, segments: &[&str], token: Option<&str>) -> RequestBuilder {
        self.req_query(method, segments, &[], token)
    }

    fn req_query(&self, method: Method, segments: &[&str], query: &[(&str, i64)], token: Option<&str>) -> RequestBuilder {
        let r = self.http.request(method, self.url(segments, query));
        match token {
            Some(t) => r.header("Authorization", format!("Token {t}")),
            None => r,
        }
    }

    /// Waits for quota and sends the request; retries on 429 and transient 5xx errors.
    async fn send(&self, req: RequestBuilder, prio: Prio) -> Result<Response> {
        let _waiting = (prio == Prio::Command).then(|| {
            self.commands_waiting.fetch_add(1, Ordering::Relaxed);
            Waiting(&self.commands_waiting)
        });
        for attempt in 0..5u32 {
            // The lock is FIFO. When the window is full, waiting happens with the lock released; a background job
            // that gets its turn while a command is waiting releases the lock and goes to the back.
            let mut gate = loop {
                let gate = self.gate.lock().await;
                if prio == Prio::Background && self.commands_waiting.load(Ordering::Relaxed) > 0 {
                    drop(gate);
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
                if gate.0 == 0 && Instant::now() < gate.1 {
                    let until = gate.1;
                    drop(gate);
                    tokio::time::sleep_until(until).await;
                    continue;
                }
                break gate;
            };
            let resp = req.try_clone().context("request not cloneable")?.send().await;
            let resp = match resp {
                Ok(r) => r,
                Err(e) if attempt < 4 => {
                    tracing::warn!(error = %e, "listenbrainz request failed, retrying");
                    drop(gate);
                    tokio::time::sleep(Duration::from_secs(2u64 << attempt)).await;
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            let header = |name| resp.headers().get(name).and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok());
            let remaining = header("x-ratelimit-remaining").unwrap_or(1);
            let reset_in = header("x-ratelimit-reset-in").unwrap_or(10);
            *gate = (remaining as u32, Instant::now() + Duration::from_secs(reset_in.max(1)));
            match resp.status() {
                StatusCode::TOO_MANY_REQUESTS => {
                    gate.0 = 0;
                    continue;
                }
                s if s.is_server_error() && attempt < 4 => {
                    drop(gate);
                    tokio::time::sleep(Duration::from_secs(2u64 << attempt)).await;
                    continue;
                }
                _ => return Ok(resp),
            }
        }
        bail!("listenbrainz: retries exhausted")
    }

    async fn json(&self, req: RequestBuilder, prio: Prio) -> Result<Value> {
        for attempt in 0..4u32 {
            let resp = self.send(req.try_clone().context("request not cloneable")?, prio).await?;
            let status = resp.status();
            // LB answers 204 with an empty body when it has no stats for the user yet.
            if status == StatusCode::NO_CONTENT {
                return Ok(Value::Null);
            }
            let text = resp.text().await?;
            match serde_json::from_str::<Value>(&text) {
                Ok(body) if status.is_success() => return Ok(body),
                Ok(body) => bail!("listenbrainz {status}: {}", body["error"].as_str().unwrap_or("unknown error")),
                // The bot protection in front of LB sometimes returns a "Verifying your browser" HTML page with 200; it is temporary.
                // HTML with an error status (502 etc.) was already retried in `send`, so it isn't retried here.
                Err(_) if status.is_success() && attempt < 3 => {
                    tracing::warn!(%status, attempt, "listenbrainz returned non-json (bot check page?), retrying");
                    tokio::time::sleep(Duration::from_secs(2u64 << attempt)).await;
                }
                Err(e) => return Err(e).context(format!("listenbrainz {status} returned non-json")),
            }
        }
        unreachable!("last attempt returns")
    }

    /// Albums with a cover for the bot's avatar, best first.
    /// `range` is an LB stats range (week, month, all_time); `None` = the user's 50 newest listens.
    pub async fn featured_albums(&self, user: &str, range: Option<&str>) -> Result<Vec<FeaturedAlbum>> {
        let req = match range {
            None => self.req_query(Method::GET, &["1", "user", user, "listens"], &[("count", 50)], None),
            Some(r) => {
                let mut url = self.url(&["1", "stats", "user", user, "release-groups"], &[("count", 50)]);
                url.query_pairs_mut().append_pair("range", r);
                self.http.get(url)
            }
        };
        Ok(featured_candidates(&self.json(req, Prio::Background).await?))
    }

    /// Downloads a JPEG or PNG image (at most 8 MB) and returns it as a `data:` URI, the form Discord takes for
    /// avatars and resvg for pictures in an SVG.
    pub async fn image_data_uri(&self, url: &str) -> Result<String> {
        let resp = self.http.get(url).send().await?.error_for_status()?;
        let kind = resp.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or_default().to_owned();
        if !matches!(kind.as_str(), "image/jpeg" | "image/png") {
            bail!("cover {url} has content-type {kind:?}");
        }
        let bytes = resp.bytes().await?;
        if bytes.len() > 8 << 20 {
            bail!("cover {url} is {} bytes", bytes.len());
        }
        use base64::Engine;
        Ok(format!("data:{kind};base64,{}", base64::engine::general_purpose::STANDARD.encode(&bytes)))
    }

    /// Returns the owner's username when the token is valid.
    pub async fn validate_token(&self, token: &str) -> Result<Option<String>> {
        let body = self.json(self.req(Method::GET, &["1", "validate-token"], Some(token)), Prio::Command).await?;
        Ok(if body["valid"].as_bool() == Some(true) { body["user_name"].as_str().map(str::to_owned) } else { None })
    }

    /// The oldest `PAGE` listens after second `after` (order not guaranteed; the caller sorts).
    pub async fn listens_after(&self, user: &str, after: i64, prio: Prio) -> Result<Vec<Listen>> {
        // min_ts=0 counts as "not given" on LB and returns the newest listens; 1 covers the whole history.
        let query = [("count", PAGE as i64), ("min_ts", after.max(1))];
        let req = self.req_query(Method::GET, &["1", "user", user, "listens"], &query, None);
        let body = self.json(req, prio).await?;
        Ok(serde_json::from_value(body["payload"]["listens"].clone())?)
    }

    pub async fn listen_count(&self, user: &str, prio: Prio) -> Result<i64> {
        let body = self.json(self.req(Method::GET, &["1", "user", user, "listen-count"], None), prio).await?;
        body["payload"]["count"].as_i64().context("listen-count: missing count")
    }

    /// The track playing now, or else the last one listened to. `true` = playing now.
    /// The third value is the listen's `mbid_mapping` (`Null` when missing; playing-now usually has none).
    pub async fn now_or_last(&self, user: &str) -> Result<Option<(TrackMetadata, bool, Value)>> {
        let body = self.json(self.req(Method::GET, &["1", "user", user, "playing-now"], None), Prio::Command).await?;
        let first = |b: &Value| {
            b["payload"]["listens"].get(0).map(|l| {
                serde_json::from_value::<Listen>(l.clone()).map(|x| (x.track_metadata, l["track_metadata"]["mbid_mapping"].clone()))
            })
        };
        if let Some(l) = first(&body) {
            let (t, m) = l?;
            return Ok(Some((t, true, m)));
        }
        let req = self.req_query(Method::GET, &["1", "user", user, "listens"], &[("count", 1)], None);
        let body = self.json(req, Prio::Command).await?;
        Ok(first(&body).transpose()?.map(|(t, m)| (t, false, m)))
    }

    /// MusicBrainz mapping from names (same fields as `mbid_mapping`); an empty object when nothing is found.
    pub async fn lookup(&self, artist: &str, recording: &str, release: Option<&str>) -> Result<Value> {
        let mut url = self.url(&["1", "metadata", "lookup", ""], &[]);
        url.query_pairs_mut().append_pair("artist_name", artist).append_pair("recording_name", recording);
        if let Some(r) = release {
            url.query_pairs_mut().append_pair("release_name", r);
        }
        let req = self.http.get(url).header("Authorization", format!("Token {}", self.service_token));
        self.json(req, Prio::Command).await
    }

    /// MusicBrainz ws/2 request; keeps to the 1 request/s per IP queue. Reading needs no credentials.
    async fn mb_get(&self, path: &[&str], query: &[(&str, &str)]) -> Result<Value> {
        // MusicBrainz returns 503 now and then when busy; one more try is usually enough.
        match self.mb_get_once(path, query).await {
            Err(e) if e.to_string().starts_with("musicbrainz 503") => self.mb_get_once(path, query).await,
            r => r,
        }
    }

    async fn mb_get_once(&self, path: &[&str], query: &[(&str, &str)]) -> Result<Value> {
        {
            let mut next = self.mb_next.lock().await;
            // Under load, don't hold the card for an image; return an error so the cache doesn't store "none".
            if *next > Instant::now() + Duration::from_secs(2) {
                bail!("musicbrainz queue full");
            }
            let at = (*next).max(Instant::now());
            *next = at + Duration::from_secs(1);
            drop(next);
            tokio::time::sleep_until(at).await;
        }
        let mut url = Url::parse("https://musicbrainz.org/ws/2/").expect("static url");
        url.path_segments_mut().expect("has path").pop_if_empty().extend(path);
        url.query_pairs_mut().extend_pairs(query).append_pair("fmt", "json");
        let resp = self.http.get(url).send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            bail!("musicbrainz {status}: {}", body.chars().take(200).collect::<String>());
        }
        Ok(resp.json().await?)
    }

    /// First reliable result of a MusicBrainz search (`entity`: artist, release-group…).
    pub async fn mb_search(&self, entity: &str, query: &str) -> Result<Option<Value>> {
        let body = self.mb_get(&[entity], &[("query", query), ("limit", "1")]).await?;
        // Plural key: artists, release-groups.
        let first = body[format!("{entity}s")].get(0).cloned();
        Ok(first.filter(|v| v["score"].as_i64().unwrap_or(0) >= 90))
    }

    /// Record of an artist with a known id: tags and links (including the Apple Music page).
    pub async fn mb_artist(&self, mbid: &str) -> Result<Value> {
        self.mb_get(&["artist", mbid], &[("inc", "url-rels tags")]).await
    }

    /// Apple Music photo from the name alone, used when the id is unknown. Ignored unless the name matches exactly.
    pub async fn itunes_artist_image(&self, artist: &str) -> Result<Option<String>> {
        let mut url = Url::parse("https://itunes.apple.com/search").expect("static url");
        url.query_pairs_mut().append_pair("term", artist).append_pair("entity", "musicArtist").append_pair("limit", "1");
        let resp = self.http.get(url).send().await?;
        if !resp.status().is_success() {
            bail!("itunes search {}", resp.status());
        }
        let body: Value = resp.json().await?;
        let hit = &body["results"][0];
        let (Some(name), Some(page)) = (hit["artistName"].as_str(), hit["artistLinkUrl"].as_str()) else { return Ok(None) };
        if name.to_lowercase() != artist.to_lowercase() {
            return Ok(None);
        }
        self.page_photo(page).await
    }

    /// Photo of an Apple Music or Tidal artist page (`og:image`); their APIs don't return photos.
    pub async fn page_photo(&self, page: &str) -> Result<Option<String>> {
        let resp = self.http.get(page).send().await?;
        if !resp.status().is_success() {
            bail!("artist page {page}: {}", resp.status());
        }
        Ok(og_image(&resp.text().await?).and_then(|img| artist_photo(&img)))
    }

    /// The release the track's ListenBrainz page shows (including `caa_id` and `caa_release_mbid`). A lookup or the
    /// listen's mapping may pick another release (single, deluxe); taking it from here keeps the cover in line with the page.
    pub async fn recording_release(&self, recording_mbid: &str) -> Result<Value> {
        Ok(self.recording_releases(&[recording_mbid]).await?.remove(recording_mbid).unwrap_or_default())
    }

    /// `recording_release` for up to 50 recordings in one request, by recording id. The release also carries
    /// `release_group_mbid` and `year`.
    pub async fn recording_releases(&self, mbids: &[&str]) -> Result<HashMap<String, Value>> {
        if mbids.is_empty() {
            return Ok(HashMap::new());
        }
        let mut url = self.url(&["1", "metadata", "recording", ""], &[]);
        url.query_pairs_mut().append_pair("recording_mbids", &mbids.join(",")).append_pair("inc", "release");
        let req = self.http.get(url).header("Authorization", format!("Token {}", self.service_token));
        let body = self.json(req, Prio::Command).await?;
        Ok(body.as_object().into_iter().flatten().map(|(k, v)| (k.clone(), v["release"].clone())).collect())
    }

    /// Release groups (`type` such as Album or Single, first release `date`, cover) for up to 50 ids, by id.
    pub async fn release_groups(&self, mbids: &[&str]) -> Result<HashMap<String, Value>> {
        if mbids.is_empty() {
            return Ok(HashMap::new());
        }
        let mut url = self.url(&["1", "metadata", "release_group", ""], &[]);
        url.query_pairs_mut().append_pair("release_group_mbids", &mbids.join(","));
        let req = self.http.get(url).header("Authorization", format!("Token {}", self.service_token));
        let body = self.json(req, Prio::Command).await?;
        Ok(body.as_object().into_iter().flatten().map(|(k, v)| (k.clone(), v["release_group"].clone())).collect())
    }

    /// MusicBrainz mappings of up to 50 `(artist, track, album)` names in one request, in the same order. `Null` when
    /// unmatched, or when the match is credited to another artist (a wrong mapping).
    pub async fn lookup_many(&self, items: &[(&str, &str, Option<&str>)]) -> Result<Vec<Value>> {
        let len = |s: &str| s.chars().count();
        // LB refuses the whole request if one item is empty or over 250 characters; a long album is left out, the rest stay unmatched.
        let sent: Vec<(usize, Value)> = items
            .iter()
            .enumerate()
            .filter(|(_, (a, t, _))| !a.is_empty() && !t.is_empty() && len(a) + len(t) <= 250)
            .map(|(i, &(a, t, album))| {
                let mut v = serde_json::json!({"artist_name": a, "recording_name": t});
                if let Some(r) = album.filter(|r| !r.is_empty() && len(a) + len(t) + len(r) <= 250) {
                    v["release_name"] = r.into();
                }
                (i, v)
            })
            .collect();
        let mut out = vec![Value::Null; items.len()];
        if sent.is_empty() {
            return Ok(out);
        }
        let recordings: Vec<&Value> = sent.iter().map(|(_, v)| v).collect();
        let req = self
            .http
            .post(self.url(&["1", "metadata", "lookup", ""], &[]))
            .header("Authorization", format!("Token {}", self.service_token))
            .json(&serde_json::json!({ "recordings": recordings }));
        // Results carry the request's `index`.
        for r in self.json(req, Prio::Command).await?.as_array().into_iter().flatten() {
            let Some(&(i, _)) = r["index"].as_u64().and_then(|i| sent.get(i as usize)) else { continue };
            let credit = artist_key(r["artist_credit_name"].as_str().unwrap_or_default()).to_lowercase();
            let name = items[i].0.to_lowercase();
            // The whole credit, or the first of several credited artists followed by a separator: "Eden & X" is
            // Eden's and "Earth, Wind & Fire with X" is theirs; "Edenbridge", "Eden Project" and "Florence and the
            // Machine" (one artist each) aren't Eden's or Florence's.
            let several = r["artist_mbids"].as_array().is_some_and(|a| a.len() > 1);
            let first = |rest: &str| SEPARATORS.iter().any(|s| rest.starts_with(s));
            if credit == name || (several && credit.strip_prefix(&name).is_some_and(first)) {
                out[i] = r.clone();
            }
        }
        Ok(out)
    }

    /// Length and first release of up to 1000 recordings, by id. Recordings LB doesn't know are left out.
    pub async fn recording_facts(&self, mbids: &[&str]) -> Result<HashMap<String, RecordingFacts>> {
        let req = self
            .http
            .post(self.url(&["1", "metadata", "recording", ""], &[]))
            .header("Authorization", format!("Token {}", self.service_token))
            .json(&serde_json::json!({ "recording_mbids": mbids, "inc": "release" }));
        let body = self.json(req, Prio::Command).await?;
        Ok(body.as_object().into_iter().flatten().map(|(k, v)| (k.to_ascii_lowercase(), RecordingFacts::from(v))).collect())
    }

    /// LB-wide `(listeners, listens)` of up to 1000 artists or recordings (`entity`: "artist", "recording"), by id.
    /// Ids LB has no stats for yet are left out.
    pub async fn popularity(&self, entity: &str, mbids: &[&str]) -> Result<HashMap<String, (i64, i64)>> {
        let key = format!("{entity}_mbid");
        let req = self.http.post(self.url(&["1", "popularity", entity], &[])).json(&serde_json::json!({ format!("{key}s"): mbids }));
        let body = self.json(req, Prio::Command).await?;
        Ok(body
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|c| Some((c[&key].as_str()?.to_ascii_lowercase(), (c["total_user_count"].as_i64()?, c["total_listen_count"].as_i64()?))))
            .collect())
    }

    /// Cover of the artist's most popular album; LB/MB have no artist photos.
    pub async fn artist_cover(&self, artist_mbid: &str) -> Result<Option<String>> {
        let req = self.req(Method::GET, &["1", "popularity", "top-release-groups-for-artist", artist_mbid], Some(&self.service_token));
        let body = self.json(req, Prio::Command).await?;
        Ok(body.as_array().into_iter().flatten().find_map(|rg| cover_url(&rg["release_group"])))
    }

    /// A full-history export still being built, or a new one. Completed exports aren't reused: they can predate
    /// an import, and `/login` is how a user asks for a fresh download.
    pub async fn export(&self, token: &str) -> Result<i64> {
        let list = self.json(self.req(Method::GET, &["1", "export", "list"], Some(token)), Prio::Background).await?;
        let list: Vec<Export> = serde_json::from_value(list)?;
        let full = |e: &&Export| matches!(e.status.as_str(), "waiting" | "in_progress") && e.start_time.is_none() && e.end_time.is_none();
        if let Some(e) = list.iter().find(full) {
            return Ok(e.export_id);
        }
        let created = self.json(self.req(Method::POST, &["1", "export", ""], Some(token)), Prio::Background).await?;
        Ok(serde_json::from_value::<Export>(created)?.export_id)
    }

    pub async fn export_status(&self, token: &str, id: i64) -> Result<String> {
        let id = id.to_string();
        let e = self.json(self.req(Method::GET, &["1", "export", &id], Some(token)), Prio::Background).await?;
        Ok(serde_json::from_value::<Export>(e)?.status)
    }

    /// Streams the archive to a file without holding it in memory.
    pub async fn download_export(&self, token: &str, id: i64, to: &Path) -> Result<()> {
        let id = id.to_string();
        let req = self.req(Method::GET, &["1", "export", &id, "download"], Some(token)).timeout(Duration::from_secs(1800));
        let mut resp = self.send(req, Prio::Background).await?;
        let kind = resp.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_owned();
        if !resp.status().is_success() || !kind.starts_with("application/zip") {
            bail!("export download failed: {} ({kind})", resp.status());
        }
        // The archive holds the user's whole history: readable only by the bot's own account.
        let mut file = tokio::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(to).await?;
        while let Some(chunk) = resp.chunk().await? {
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        Ok(())
    }
}

/// The `og:image` URL in an HTML page.
fn og_image(html: &str) -> Option<String> {
    let tag = &html[html.find(r#"property="og:image""#)?..];
    let start = tag.find(r#"content=""#)? + r#"content=""#.len();
    let img = &tag[start..start + tag[start..].find('"')?];
    img.starts_with("https://").then(|| img.to_owned())
}

/// Pages with photos in a MusicBrainz artist record, in order of preference: Apple Music (1200px), Tidal (750px).
pub fn photo_pages(artist: &Value) -> Vec<String> {
    let urls: Vec<&str> = artist["relations"].as_array().into_iter().flatten().filter_map(|r| r["url"]["resource"].as_str()).collect();
    let pick = |prefix: &str| urls.iter().find(|u| u.starts_with(prefix) && u.contains("/artist/")).map(|u| u.to_string());
    pick("https://music.apple.com/").into_iter().chain(pick("https://tidal.com/")).collect()
}

/// Artist photo from a page's `og:image`. The Apple thumbnail is turned into a 1200x1200 square; Tidal's is
/// already square (nothing above 750px). Any other URL (e.g. a generic logo) gives `None`, falling back to the album cover.
fn artist_photo(img: &str) -> Option<String> {
    if img.starts_with("https://resources.tidal.com/images/") {
        return Some(img.to_owned());
    }
    if !(img.contains(".mzstatic.com/") && img.contains("/image/thumb/")) {
        return None;
    }
    let (base, _) = img.rsplit_once('/')?;
    Some(format!("{base}/1200x1200cc.jpg"))
}

/// Title link when no id is found: a ListenBrainz search (`search_type`: artist, album, track).
pub fn search_url(search_type: &str, term: &str) -> String {
    let mut url = Url::parse("https://listenbrainz.org/search/").expect("static url");
    url.query_pairs_mut().append_pair("search_term", term).append_pair("search_type", search_type);
    url.into()
}

/// Main (first) artist of a multi-artist credit: "A feat. B", "A & B", "A, B", "A x B" → "A". Unchanged without a separator.
pub fn main_artist(credit: &str) -> &str {
    // ASCII lowercasing keeps byte positions, so the cut point is valid in the original too.
    let lower = credit.to_ascii_lowercase();
    match SEPARATORS.iter().filter_map(|s| lower.find(s)).min() {
        Some(0) | None => credit,
        Some(i) => credit[..i].trim(),
    }
}

/// What joins artists in a credit, lowercase.
const SEPARATORS: [&str; 12] = [" feat. ", " feat ", " ft. ", " ft ", " featuring ", " & ", ", ", " x ", " and ", " with ", " vs. ", " + "];

/// Quoted phrase for a Lucene query.
pub fn phrase(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// The 4 most voted tags of a MusicBrainz result, joined with ` - ` like fmbot.
pub fn tags(v: &Value) -> Option<String> {
    let mut tags: Vec<(i64, &str)> =
        v["tags"].as_array()?.iter().filter_map(|t| Some((t["count"].as_i64()?, t["name"].as_str()?))).collect();
    tags.sort_by_key(|t| std::cmp::Reverse(t.0));
    let names: Vec<&str> = tags.iter().take(4).map(|t| t.1).collect();
    (!names.is_empty()).then(|| names.join(" - "))
}

/// A smaller copy of a cover or photo URL made in this file: the Cover Art Archive has 250 and 500px thumbnails,
/// Apple any size. Other URLs (Tidal's 750px photos) stay as they are.
pub fn thumb(url: &str, px: u32) -> String {
    if url.starts_with("https://coverartarchive.org/")
        && let Some((head, tail)) = url.rsplit_once("-1200")
    {
        return format!("{head}-{}{tail}", if px <= 250 { 250 } else { 500 });
    }
    match url.strip_suffix("/1200x1200cc.jpg") {
        Some(base) if url.contains(".mzstatic.com/") => format!("{base}/{px}x{px}cc.jpg"),
        _ => url.to_owned(),
    }
}

/// 1200px cover URL from an object with `caa_id` + `caa_release_mbid`; otherwise the front cover of `release_mbid`.
pub fn cover_url(v: &Value) -> Option<String> {
    match (v["caa_release_mbid"].as_str(), v["caa_id"].as_i64()) {
        (Some(mbid), Some(id)) => Some(format!("https://coverartarchive.org/release/{mbid}/{id}-1200.jpg")),
        _ => v["release_mbid"].as_str().map(|mbid| format!("https://coverartarchive.org/release/{mbid}/front-1200")),
    }
}

/// What the stats commands read about a recording. 0 = unknown.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RecordingFacts {
    pub length_ms: i32,
    /// First release as yyyymmdd, yyyy0000 when only the year is known.
    pub released: i32,
}

impl From<&Value> for RecordingFacts {
    fn from(v: &Value) -> Self {
        let rec = &v["recording"];
        let length_ms = rec["length"].as_i64().filter(|l| (1..=86_400_000).contains(l)).unwrap_or(0) as i32;
        let year = v["release"]["year"].as_i64().filter(|y| (1000..=9999).contains(y)).map_or(0, |y| y as i32 * 10_000);
        Self { length_ms, released: rec["first_release_date"].as_str().and_then(date_number).unwrap_or(year) }
    }
}

/// "1994-08-22" → 19940822, "1994-08" → 19940800, "1994" → 19940000.
pub fn date_number(s: &str) -> Option<i32> {
    let mut parts = s.split('-').map(|p| p.parse::<i32>().ok());
    let y = parts.next()??;
    let m = parts.next().unwrap_or(Some(0))?;
    let d = parts.next().unwrap_or(Some(0))?;
    ((1000..=9999).contains(&y) && (0..=12).contains(&m) && (0..=31).contains(&d)).then_some(y * 10_000 + m * 100 + d)
}

#[derive(Debug, PartialEq)]
pub struct FeaturedAlbum {
    /// One album has many releases, each with its own cover URL; repeats are checked by release group.
    pub release_group: String,
    pub cover: String,
    /// "Album - Artist", for the log.
    pub label: String,
}

/// Candidates from a listens page or a release-group stats page; entries without a cover or release group are skipped.
fn featured_candidates(body: &Value) -> Vec<FeaturedAlbum> {
    // Only the cover LB itself picked (caa_id); `release_mbid` alone may have no front image.
    let album = |v: &Value, album: &Value, artist: &Value| {
        v["caa_id"].as_i64()?;
        Some(FeaturedAlbum {
            release_group: v["release_group_mbid"].as_str()?.to_owned(),
            cover: cover_url(v)?,
            label: format!("{} - {}", album.as_str()?, artist.as_str()?),
        })
    };
    let listens = body["payload"]["listens"].as_array().into_iter().flatten().filter_map(|l| {
        let m = &l["track_metadata"];
        album(&m["mbid_mapping"], &m["release_name"], &m["artist_name"])
    });
    let stats = body["payload"]["release_groups"].as_array().into_iter().flatten()
        .filter_map(|g| album(g, &g["release_group_name"], &g["artist_name"]));
    listens.chain(stats).collect()
}

/// ListenBrainz entity page: `artist`/mbid, `album`/release-group mbid, `track`/recording mbid.
pub fn page_url(kind: &str, mbid: &str) -> String {
    let mut url = Url::parse("https://listenbrainz.org").expect("static url");
    url.path_segments_mut().expect("has path").extend([kind, mbid, ""]);
    url.into()
}

/// The user's ListenBrainz profile URL.
pub fn profile_url(user: &str) -> String {
    let mut url = Url::parse("https://listenbrainz.org").expect("static url");
    url.path_segments_mut().expect("has path").extend(["user", user, ""]);
    url.into()
}

#[cfg(test)]
pub(crate) mod tests {
    #[test]
    fn cover_urls() {
        let caa = serde_json::json!({"caa_release_mbid": "r1", "caa_id": 5, "release_mbid": "r2"});
        assert_eq!(super::cover_url(&caa).unwrap(), "https://coverartarchive.org/release/r1/5-1200.jpg");
        let rel = serde_json::json!({"release_mbid": "r2"});
        assert_eq!(super::cover_url(&rel).unwrap(), "https://coverartarchive.org/release/r2/front-1200");
        assert!(super::cover_url(&serde_json::json!({})).is_none());
        assert_eq!(super::thumb("https://coverartarchive.org/release/a-1200b/5-1200.jpg", 300), "https://coverartarchive.org/release/a-1200b/5-500.jpg");
        assert_eq!(super::thumb("https://coverartarchive.org/release-group/r/front-1200", 200), "https://coverartarchive.org/release-group/r/front-250");
        assert_eq!(super::thumb("https://is1.mzstatic.com/image/thumb/x/1200x1200cc.jpg", 300), "https://is1.mzstatic.com/image/thumb/x/300x300cc.jpg");
        assert_eq!(super::thumb("https://resources.tidal.com/images/a/750x750.jpg", 300), "https://resources.tidal.com/images/a/750x750.jpg");
        let html = r#"<meta property="og:image" content="https://is1.mzstatic.com/x/a.png/1200x630cw.png">"#;
        assert_eq!(super::og_image(html).unwrap(), "https://is1.mzstatic.com/x/a.png/1200x630cw.png");
        assert!(super::og_image("<meta>").is_none());
        assert_eq!(super::artist_photo("https://is1.mzstatic.com/image/thumb/x/a.png/1200x630cw.png").unwrap(), "https://is1.mzstatic.com/image/thumb/x/a.png/1200x1200cc.jpg");
        assert!(super::artist_photo("https://music.apple.com/assets/meta/apple-music.png").is_none());
        let tidal = "https://resources.tidal.com/images/7b/93/750x750.jpg";
        assert_eq!(super::artist_photo(tidal).unwrap(), tidal);
        let mb = serde_json::json!({"relations": [
            {"url": {"resource": "https://tidal.com/artist/2"}},
            {"url": {"resource": "https://itunes.apple.com/us/artist/id1"}},
            {"url": {"resource": "https://music.apple.com/us/artist/1"}}]});
        assert_eq!(super::photo_pages(&mb), ["https://music.apple.com/us/artist/1", "https://tidal.com/artist/2"]);
        assert_eq!(super::search_url("artist", "a b"), "https://listenbrainz.org/search/?search_term=a+b&search_type=artist");
        assert_eq!(super::main_artist("Travis Scott feat. Drake"), "Travis Scott");
        assert_eq!(super::main_artist("Şebnem Ferah & Teoman"), "Şebnem Ferah");
        assert_eq!(super::main_artist("Rosalía, The Weeknd"), "Rosalía");
        assert_eq!(super::main_artist("Charli XCX x Lorde"), "Charli XCX");
        assert_eq!(super::main_artist("Twenty One Pilots"), "Twenty One Pilots");
        assert_eq!(super::page_url("track", "r1"), "https://listenbrainz.org/track/r1/");
        assert_eq!(super::phrase(r#"a"b\c"#), r#""a\"b\\c""#);
        let t = serde_json::json!({"tags": [{"count": 1, "name": "x"}, {"count": 5, "name": "rock"}]});
        assert_eq!(super::tags(&t).unwrap(), "rock - x");
    }

    /// Hits the real MusicBrainz/Apple: `cargo test -- --ignored live_art --nocapture`
    #[tokio::test]
    #[ignore]
    async fn live_art() {
        let lb = super::Lb::new(std::env::var("LB_TOKEN").unwrap_or_default());
        let a = lb.mb_search("artist", &format!("artist:{}", super::phrase("Twenty One Pilots"))).await.unwrap().expect("artist");
        let mbid = a["id"].as_str().unwrap();
        let info = lb.mb_artist(mbid).await.unwrap();
        let pages = super::photo_pages(&info);
        let page = &pages[0];
        let photo = lb.page_photo(page).await.unwrap().expect("photo");
        let tidal = lb.page_photo(&pages[1]).await.unwrap().expect("tidal photo");
        println!("tidal={tidal}");
        let by_name = lb.itunes_artist_image("Twenty One Pilots").await.unwrap().expect("itunes photo");
        let rg = lb.mb_search("release-group", r#"releasegroup:"Blurryface" AND artist:"Twenty One Pilots""#).await.unwrap().expect("rg");
        println!("mbid={mbid} tags={:?} page={page}\nphoto={photo}\nby_name={by_name}\nrg={}", super::tags(&info), rg["id"]);
    }

    /// Hits the real API: `cargo test -- --ignored live_api`
    #[tokio::test]
    #[ignore]
    async fn live_featured() {
        let lb = super::Lb::new(std::env::var("LB_TOKEN").unwrap_or_default());
        for range in [None, Some("week"), Some("all_time")] {
            let albums = lb.featured_albums("rob", range).await.unwrap();
            println!("{range:?}: {} albums, first {:?}", albums.len(), albums.first());
        }
        let albums = lb.featured_albums("rob", Some("all_time")).await.unwrap();
        let uri = lb.image_data_uri(&albums[0].cover).await.unwrap();
        assert!(uri.starts_with("data:image/"), "{}", &uri[..40]);
        println!("data uri {} KB", uri.len() / 1024);
    }

    #[tokio::test]
    #[ignore]
    async fn live_api() {
        let lb = super::Lb::new(std::env::var("LB_TOKEN").unwrap_or_default());
        let count = lb.listen_count("rob", super::Prio::Command).await.unwrap();
        let first = lb.listens_after("rob", 0, super::Prio::Command).await.unwrap();
        assert_eq!(first.len(), super::PAGE);
        let oldest = first.iter().map(|l| l.listened_at as i64).min().unwrap();
        let newest = first.iter().map(|l| l.listened_at as i64).max().unwrap();
        let next = lb.listens_after("rob", newest, super::Prio::Command).await.unwrap();
        assert!(next.iter().all(|l| l.listened_at as i64 > newest), "cursor must move forward");
        assert!(lb.now_or_last("rob").await.unwrap().is_some());
        println!("count={count} first_page={oldest}..{newest} next={}", next.len());
    }

    /// Local fake LB: answers each connection with the next `remaining`; returns the request paths and times.
    pub(crate) async fn fake_lb(
        responses: &'static [(u16, u32, &'static str)],
    ) -> (std::net::SocketAddr, tokio::task::JoinHandle<Vec<(tokio::time::Instant, String)>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut seen = Vec::new();
            for (status, remaining, body) in responses {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 4096];
                let n = sock.read(&mut buf).await.unwrap();
                let path = String::from_utf8_lossy(&buf[..n]).split_whitespace().nth(1).unwrap().to_owned();
                seen.push((tokio::time::Instant::now(), path));
                let resp = format!(
                    "HTTP/1.1 {status} X\r\nx-ratelimit-remaining: {remaining}\r\nx-ratelimit-reset-in: 1\r\n\
                     content-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                sock.write_all(resp.as_bytes()).await.unwrap();
            }
            seen
        });
        (addr, server)
    }

    const COUNT: &str = r#"{"payload":{"count":5}}"#;

    /// A completed export can predate an import, so `/login` must ask for a new one; a pending one is reused.
    #[tokio::test]
    async fn export_reuses_only_pending_exports() {
        const COMPLETED: &str = r#"[{"export_id": 1, "status": "completed", "start_time": null, "end_time": null}]"#;
        const CREATED: &str = r#"{"export_id": 2, "status": "waiting", "start_time": null, "end_time": null}"#;
        const PENDING: &str = r#"[{"export_id": 3, "status": "in_progress", "start_time": null, "end_time": null}]"#;
        let (addr, server) = fake_lb(&[(200, 10, COMPLETED), (200, 10, CREATED), (200, 10, PENDING)]).await;
        let lb = super::Lb::with_base(&format!("http://{addr}"), String::new());
        assert_eq!(lb.export("t").await.unwrap(), 2);
        assert_eq!(lb.export("t").await.unwrap(), 3);
        let paths: Vec<String> = server.await.unwrap().into_iter().map(|(_, p)| p).collect();
        assert_eq!(paths, ["/1/export/list", "/1/export/", "/1/export/list"]);
    }

    /// LB's bot-check page isn't JSON; the request must be retried.
    #[tokio::test]
    async fn retries_bot_check_page() {
        const PAGE_HTML: &str = "<!DOCTYPE html><html><head><title>Verifying your browser</title></head></html>";
        let (addr, server) = fake_lb(&[(200, 10, PAGE_HTML), (200, 10, COUNT)]).await;
        let lb = super::Lb::with_base(&format!("http://{addr}"), String::new());
        assert_eq!(lb.listen_count("x", super::Prio::Command).await.unwrap(), 5);
        assert_eq!(server.await.unwrap().len(), 2);
    }

    /// An HTML page with an error status (e.g. 404) must fail on the first try, not be retried like the bot check.
    #[tokio::test]
    async fn error_html_is_not_retried() {
        const ERROR_HTML: &str = "<html><body>Not Found</body></html>";
        let (addr, server) = fake_lb(&[(404, 10, ERROR_HTML)]).await;
        let lb = super::Lb::with_base(&format!("http://{addr}"), String::new());
        let started = std::time::Instant::now();
        assert!(lb.listen_count("x", super::Prio::Command).await.is_err());
        assert!(started.elapsed() < std::time::Duration::from_secs(1), "error page was retried");
        assert_eq!(server.await.unwrap().len(), 1);
    }

    /// The first answer uses up the quota; the second request must wait until the window ends.
    #[tokio::test]
    async fn gate_waits_for_window_and_encodes_usernames() {
        let (addr, server) = fake_lb(&[(200, 0, COUNT), (200, 10, COUNT)]).await;
        let lb = super::Lb::with_base(&format!("http://{addr}"), String::new());
        assert_eq!(lb.listen_count("a?b/c#d", super::Prio::Command).await.unwrap(), 5);
        assert_eq!(lb.listen_count("x", super::Prio::Background).await.unwrap(), 5);
        let seen = server.await.unwrap();
        assert_eq!(seen[0].1, "/1/user/a%3Fb%2Fc%23d/listen-count");
        assert!(seen[1].0 - seen[0].0 >= std::time::Duration::from_millis(900), "second request did not wait");
        assert_eq!(super::profile_url("a b/c"), "https://listenbrainz.org/user/a%20b%2Fc/");
    }

    /// When the quota is used up, a background request that queued first must not go before a later command.
    #[tokio::test]
    async fn commands_go_before_background_work() {
        let (addr, server) = fake_lb(&[(200, 0, COUNT), (200, 10, COUNT), (200, 10, COUNT)]).await;
        let lb = std::sync::Arc::new(super::Lb::with_base(&format!("http://{addr}"), String::new()));
        lb.listen_count("first", super::Prio::Command).await.unwrap();
        let bg = tokio::spawn({
            let lb = lb.clone();
            async move { lb.listen_count("bg", super::Prio::Background).await.unwrap() }
        });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        lb.listen_count("cmd", super::Prio::Command).await.unwrap();
        bg.await.unwrap();
        let order: Vec<String> = server.await.unwrap().into_iter().map(|(_, p)| p).collect();
        assert_eq!(order, ["/1/user/first/listen-count", "/1/user/cmd/listen-count", "/1/user/bg/listen-count"]);
    }

    #[test]
    fn long_names_are_clamped() {
        let long = "ä".repeat(3000);
        let json = serde_json::json!({"artist_name": long, "track_name": "x", "release_name": long});
        let m: super::TrackMetadata = serde_json::from_value(json).unwrap();
        assert_eq!(m.artist_name.chars().count(), super::MAX_NAME_CHARS);
        assert_eq!(m.release_name.unwrap().chars().count(), super::MAX_NAME_CHARS);
        assert_eq!(m.track_name, "x");
    }

    #[test]
    fn names_are_canonical_across_clients() {
        let meta = |v: serde_json::Value| serde_json::from_value::<super::TrackMetadata>(v).unwrap();
        // LB mapping: first credit + MusicBrainz track title.
        let m = meta(serde_json::json!({"artist_name": "Röyksopp feat. Susanne Sundfør", "track_name": "Tell Him - 2022 Remaster",
            "mbid_mapping": {"artists": [{"artist_credit_name": "Röyksopp"}, {"artist_credit_name": "Susanne Sundfør"}], "recording_name": "Tell Him"}}));
        assert_eq!((m.artist_name.as_str(), m.track_name.as_str()), ("Röyksopp", "Tell Him"));
        // No mapping, but the client's list is there.
        let m = meta(serde_json::json!({"artist_name": "Röyksopp, Susanne Sundfør", "track_name": "Tell Him (feat. Susanne Sundfør)",
            "additional_info": {"artist_names": ["Röyksopp", "Susanne Sundfør"]}}));
        assert_eq!((m.artist_name.as_str(), m.track_name.as_str()), ("Röyksopp", "Tell Him"));
        // Neither; a broken additional_info doesn't drop the listen.
        let m = meta(serde_json::json!({"artist_name": "Röyksopp ft. Susanne Sundfør", "track_name": "Tell Him",
            "release_name": "Profound Mysteries II (Deluxe Edition)", "additional_info": {"artist_names": 5}}));
        assert_eq!((m.artist_name.as_str(), m.release_name.as_deref()), ("Röyksopp", Some("Profound Mysteries II")));

        assert_eq!(super::artist_key("Simon & Garfunkel"), "Simon & Garfunkel");
        assert_eq!(super::artist_key("Tyler, The Creator"), "Tyler, The Creator");
        assert_eq!(super::title_key("Song (Live) [2011 Remastered Version]"), "Song (Live)");
        assert_eq!(super::title_key("Song - Remastered 2009"), "Song");
        assert_eq!(super::title_key("Song (with Justin Bieber)"), "Song");
        assert_eq!(super::title_key("Song - Live"), "Song - Live");
        assert_eq!(super::title_key("(Remastered)"), "(Remastered)");
        assert_eq!(super::album_key("Album [Expanded Edition]"), "Album");
    }

    #[test]
    fn mapping_ids_lengths_and_facts() {
        let meta = |v: serde_json::Value| serde_json::from_value::<super::TrackMetadata>(v).unwrap();
        let (a, r) = ("8F6BD1E4-fbe1-4f50-aa9b-94c450ec0f11", "e97f805a-ab48-4c52-855e-07049142113d");
        let m = meta(serde_json::json!({"artist_name": "Portishead", "track_name": "Roads",
            "mbid_mapping": {"artists": [{"artist_credit_name": "Portishead", "artist_mbid": a}], "recording_name": "Roads", "recording_mbid": r},
            "additional_info": {"duration_ms": 305_000}}));
        assert_eq!((m.artist_mbid.as_deref(), m.recording_mbid.as_deref(), m.length_ms), (Some(a.to_lowercase().as_str()), Some(r), Some(305_000)));
        // Ids only come with names from the same mapping; a broken id or length is dropped, not stored.
        let m = meta(serde_json::json!({"artist_name": "X", "track_name": "Y", "mbid_mapping": {"recording_mbid": r, "artists": [{"artist_mbid": a}]},
            "additional_info": {"duration": 200}}));
        assert_eq!((m.artist_mbid, m.recording_mbid, m.length_ms), (None, None, Some(200_000)));
        let m = meta(serde_json::json!({"artist_name": "X", "track_name": "Y", "mbid_mapping": {"recording_name": "Y", "recording_mbid": "'; DROP"},
            "additional_info": {"duration_ms": -5}}));
        assert_eq!((m.recording_mbid, m.length_ms), (None, None));
        assert_eq!(super::uuid(&serde_json::json!("e97f805a-ab48-4c52-855e-07049142113")), None, "too short");

        assert_eq!(super::date_number("1994-08-22"), Some(19_940_822));
        assert_eq!(super::date_number("1994-08"), Some(19_940_800));
        assert_eq!(super::date_number("1994"), Some(19_940_000));
        assert_eq!(super::date_number("199x"), None);
        assert_eq!(super::date_number("1994-13-01"), None);
        let f = super::RecordingFacts::from(&serde_json::json!({"recording": {"length": 373_133, "first_release_date": "1994-08-22"}, "release": {"year": 1997}}));
        assert_eq!(f, super::RecordingFacts { length_ms: 373_133, released: 19_940_822 });
        let f = super::RecordingFacts::from(&serde_json::json!({"recording": {}, "release": {"year": 1997}}));
        assert_eq!(f, super::RecordingFacts { length_ms: 0, released: 19_970_000 });
    }

    /// A lookup result is only kept when it is credited to our artist: the whole name, or the first of several
    /// credited artists. "Eden" must not take the ids of "Edenbridge" or "Eden Project", which would be stored for good.
    #[tokio::test]
    async fn lookup_needs_the_whole_artist_name() {
        const FOUND: &str = r#"[{"index": 0, "artist_credit_name": "Edenbridge", "artist_mbids": ["x"], "recording_mbid": "r0"},
            {"index": 1, "artist_credit_name": "Eden & Someone", "artist_mbids": ["y", "y2"], "recording_mbid": "r1"},
            {"index": 2, "artist_credit_name": "eden", "artist_mbids": ["z"], "recording_mbid": "r2"},
            {"index": 3, "artist_credit_name": "Eden Project", "artist_mbids": ["p"], "recording_mbid": "r3"},
            {"index": 4, "artist_credit_name": "Florence and the Machine", "artist_mbids": ["f"], "recording_mbid": "r4"},
            {"index": 5, "artist_credit_name": "Simon & Garfunkel", "artist_mbids": ["s"], "recording_mbid": "r5"},
            {"index": 6, "artist_credit_name": "Earth, Wind & Fire with The Emotions", "artist_mbids": ["e", "e2"], "recording_mbid": "r6"},
            {"index": 7, "artist_credit_name": "Eden Project & Someone", "artist_mbids": ["q", "q2"], "recording_mbid": "r7"}]"#;
        let (addr, server) = fake_lb(&[(200, 10, FOUND)]).await;
        let lb = super::Lb::with_base(&format!("http://{addr}"), String::new());
        let items = [
            ("Eden", "Wake Up", None),
            ("Eden", "Duet", None),
            ("Eden", "Solo", None),
            ("Eden", "Gravity", None),
            ("Florence", "Dog Days", None),
            ("Simon & Garfunkel", "The Boxer", None),
            ("Earth, Wind & Fire", "Boogie Wonderland", None),
            ("Eden", "Together", None),
        ];
        let found = lb.lookup_many(&items).await.unwrap();
        assert_eq!(found.iter().map(|r| r["recording_mbid"].as_str()).collect::<Vec<_>>(), [None, Some("r1"), Some("r2"), None, None, Some("r5"), Some("r6"), None]);
        server.await.unwrap();
    }

    /// Popularity and facts come back keyed by id; ids LB has no data for are left out.
    #[tokio::test]
    async fn popularity_and_facts_by_id() {
        const POP: &str = r#"[{"recording_mbid": "AA", "total_listen_count": 1000, "total_user_count": 10},
            {"recording_mbid": "bb", "total_listen_count": null, "total_user_count": null}]"#;
        const FACTS: &str = r#"{"aa": {"recording": {"length": 1000}, "release": {"year": 2001}}}"#;
        let (addr, server) = fake_lb(&[(200, 10, POP), (200, 10, FACTS)]).await;
        let lb = super::Lb::with_base(&format!("http://{addr}"), String::new());
        let pop = lb.popularity("recording", &["aa", "bb"]).await.unwrap();
        assert_eq!(pop, std::collections::HashMap::from([("aa".to_owned(), (10, 1000))]));
        let facts = lb.recording_facts(&["aa", "bb"]).await.unwrap();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts["aa"], super::RecordingFacts { length_ms: 1000, released: 20_010_000 });
        let paths: Vec<String> = server.await.unwrap().into_iter().map(|(_, p)| p).collect();
        assert_eq!(paths, ["/1/popularity/recording", "/1/metadata/recording/"]);
    }

    #[test]
    fn featured_candidates_need_a_cover() {
        let listens = serde_json::json!({"payload": {"listens": [
            {"track_metadata": {"artist_name": "A", "release_name": "X", "mbid_mapping": {"caa_id": 7, "caa_release_mbid": "r1", "release_group_mbid": "g1"}}},
            {"track_metadata": {"artist_name": "A", "release_name": "X", "mbid_mapping": {"caa_id": 7, "caa_release_mbid": "r1"}}},
            {"track_metadata": {"artist_name": "B", "release_name": "Y", "mbid_mapping": {"release_mbid": "r2"}}},
            {"track_metadata": {"artist_name": "C", "track_name": "Z"}},
        ]}});
        let album = |g: &str, r: &str, id: i64, label: &str| super::FeaturedAlbum {
            release_group: g.into(),
            cover: format!("https://coverartarchive.org/release/{r}/{id}-1200.jpg"),
            label: label.into(),
        };
        assert_eq!(super::featured_candidates(&listens), [album("g1", "r1", 7, "X - A")]);
        let stats = serde_json::json!({"payload": {"release_groups": [
            {"artist_name": "P", "release_group_name": "Q", "caa_id": 9, "caa_release_mbid": "r3", "release_group_mbid": "g3"},
        ]}});
        assert_eq!(super::featured_candidates(&stats), [album("g3", "r3", 9, "Q - P")]);
        assert!(super::featured_candidates(&serde_json::Value::Null).is_empty());
    }

    #[test]
    fn review_cases() {
        // MusicBrainz writes ’, clients write '.
        assert_eq!(super::title_key("Don’t Stop Me Now"), super::title_key("Don't Stop Me Now"));
        assert_eq!(super::artist_key("Guns N’ Roses"), super::artist_key("Guns N' Roses"));
        assert_eq!(super::title_key("Song - Live / Remastered 2011"), "Song - Live / Remastered 2011");
        assert_eq!(super::title_key("Song (Live) [Remix Remastered]"), "Song (Live) [Remix Remastered]");
        assert_eq!(super::title_key("Song (Feathers)"), "Song (Feathers)");
        assert_eq!(super::title_key("Song (feat. A (B))"), "Song");
        assert_eq!(super::title_key("Song (feat. A (B)) (Live)"), "Song (Live)");
        assert_eq!(super::title_key("Song (feat. Clive Davis)"), "Song");
        assert_eq!(super::title_key("Song (with Lil Demon)"), "Song");
    }
}
