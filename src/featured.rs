//! Changes the bot's avatar every hour to an album cover from a random user, like fmbot's "featured".
//! Mode is picked at random: the user's recent listens, or their top albums of the week, month or all time.

use std::{
    collections::VecDeque,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::Result;
use tokio::sync::Mutex;
use twilight_http::Client;

use crate::ingest::Ingest;

const MODES: [(Option<&str>, &str); 4] =
    [(None, "recent listens"), (Some("week"), "top albums this week"), (Some("month"), "top albums this month"), (Some("all_time"), "top albums of all time")];
/// fmbot doesn't feature the same album again within 28 days.
const RECENT: usize = 28 * 24;
/// Users tried per hour before giving up until the next hour.
const TRIES: usize = 5;

/// Shared with the bot for `.featured block`. One lock, so the hourly change and the command never run at once.
#[derive(Default)]
pub struct Featured(Mutex<State>);

#[derive(Default)]
struct State {
    // shortcut: kept in memory, so a restart may feature a recent album again; store it in the DB if that matters.
    recent: VecDeque<String>,
    /// Release group of the current avatar.
    current: Option<String>,
}

pub async fn run(featured: Arc<Featured>, http: Arc<Client>, ingest: Arc<Ingest>) {
    // A bot without any avatar gets one right away; otherwise restarts wait for the hour (avatar change limit).
    match http.current_user().await {
        Ok(r) => match r.model().await {
            Ok(me) if me.avatar.is_none() => {
                if let Err(e) = rotate(&mut *featured.0.lock().await, &http, &ingest).await {
                    tracing::warn!(error = ?e, "first featured avatar failed");
                }
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(error = ?e, "couldn't read the bot's avatar"),
        },
        Err(e) => tracing::warn!(error = ?e, "couldn't read the bot's avatar"),
    }
    loop {
        // On the hour, not at startup: frequent restarts would hit Discord's avatar change limit.
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
        tokio::time::sleep(Duration::from_secs(3600 - now % 3600)).await;
        let mut state = featured.0.lock().await;
        if let Err(e) = rotate(&mut state, &http, &ingest).await {
            tracing::warn!(error = ?e, "featured avatar failed");
        }
    }
}

impl Featured {
    /// Bans the current avatar's album and replaces it at once. If no replacement can be set, the avatar falls
    /// back to Discord's default, so a blocked (e.g. explicit) cover never stays up. Returns the reply text.
    pub async fn block(&self, http: &Client, ingest: &Ingest) -> Result<String> {
        let mut state = self.0.lock().await;
        let Some(rg) = state.current.clone() else { return Ok("The bot hasn't set an avatar since it started.".into()) };
        ingest.db.block_featured(&rg).await?;
        let why = match rotate(&mut state, http, ingest).await {
            Ok(Some(label)) => return Ok(format!("Blocked release group {rg}. New avatar: {label}.")),
            Ok(None) => "no new album was found".to_owned(),
            Err(e) => {
                tracing::warn!(error = ?e, "featured avatar after block failed");
                format!("setting a new avatar failed ({e})")
            }
        };
        http.update_current_user().avatar(None).await?;
        state.current = None;
        Ok(format!("Blocked release group {rg}, but {why}; the avatar is back to Discord's default."))
    }
}

/// Sets a new avatar and returns its "Album - Artist", or `None` when no user had a new album with a cover.
async fn rotate(state: &mut State, http: &Client, ingest: &Ingest) -> Result<Option<String>> {
    // shortcut: no automatic check for explicit covers (fmbot keeps a hand-made list too); the owner blocks them.
    let blocked = ingest.db.featured_blocked().await?;
    for _ in 0..TRIES {
        let Some((user, mode)) = ingest.db.featured_user().await? else {
            tracing::info!("featured: no eligible user");
            return Ok(None);
        };
        let (range, mode_name) = MODES[mode as usize % MODES.len()];
        let albums = ingest.lb.featured_albums(&user, range).await?;
        let fresh = |a: &&crate::lb::FeaturedAlbum| !state.recent.contains(&a.release_group) && !blocked.contains(&a.release_group);
        let Some(album) = albums.iter().find(fresh) else { continue };
        let data = ingest.lb.image_data_uri(&album.cover).await?;
        http.update_current_user().avatar(Some(&data)).await?;
        tracing::info!(%user, mode = mode_name, album = %album.label, release_group = %album.release_group, "featured avatar changed");
        if state.recent.len() == RECENT {
            state.recent.pop_front();
        }
        state.recent.push_back(album.release_group.clone());
        state.current = Some(album.release_group.clone());
        return Ok(Some(album.label.clone()));
    }
    tracing::info!("featured: no new album with a cover after {TRIES} users");
    Ok(None)
}
