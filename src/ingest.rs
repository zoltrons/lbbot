//! Fetches history from ListenBrainz and writes it to the database.
//! First index: the user's export ZIP (a few requests), paging as a fallback. After that: incremental with `min_ts`.

use std::{
    collections::HashSet,
    io::{BufRead, BufReader},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result};
use tokio::sync::{Semaphore, mpsc};

use crate::{
    db::{Db, User},
    history::Entry,
    lb::{Lb, Listen, PAGE, Prio},
};

pub struct Ingest {
    pub db: Db,
    pub lb: Lb,
    busy: Mutex<HashSet<i32>>,
    /// Full indexes processed at the same time: download and parse (or page through) a whole history and write it.
    /// The rest wait their turn in order, so many logins at once don't spike memory and disk.
    heavy: Semaphore,
}

/// Removes the user from the busy set when the work ends.
struct BusyGuard<'a>(&'a Ingest, i32);
impl Drop for BusyGuard<'_> {
    fn drop(&mut self) {
        self.0.busy.lock().unwrap().remove(&self.1);
    }
}

impl Ingest {
    pub fn new(db: Db, lb: Lb) -> Arc<Self> {
        Arc::new(Self { db, lb, busy: Mutex::default(), heavy: Semaphore::new(2) })
    }

    fn claim(&self, user_id: i32) -> Option<BusyGuard<'_>> {
        self.busy.lock().unwrap().insert(user_id).then(|| BusyGuard(self, user_id))
    }

    pub fn is_busy(&self, user_id: i32) -> bool {
        self.busy.lock().unwrap().contains(&user_id)
    }

    /// Indexes the whole history from scratch. Runs in the background.
    pub fn spawn_index(self: &Arc<Self>, user_id: i32) {
        let this = self.clone();
        tokio::spawn(async move {
            let _ = this.index_now(user_id).await;
        });
    }

    /// Runs a full index and returns how many listens are stored; `None` if another job holds the user.
    pub async fn index_now(&self, user_id: i32) -> Result<Option<i32>> {
        let Some(_guard) = self.claim(user_id) else { return Ok(None) };
        let result = self.index(user_id).await;
        if let Err(e) = &result {
            tracing::error!(user_id, error = ?e, "index failed");
        }
        result.map(Some)
    }

    async fn index(&self, user_id: i32) -> Result<i32> {
        // Marked after the claim is taken: if it is cut off, it starts again at boot and on `,update`.
        self.db.mark_unindexed(user_id).await?;
        let user = self.db.user(user_id).await?;
        // Every full index starts from /login, which stores the token until this index finishes.
        let token = self.db.token(&user)?.context("no token stored; the user has to /login again")?;
        // Waiting for LB to build the export holds no slot; everything after it does.
        let export = self.wait_export(&token).await;
        let _permit = self.heavy.acquire().await?;
        let entries = match async { self.export_entries(&token, export?).await }.await {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(user_id, error = ?e, "export failed, falling back to paging");
                self.pages(&user.lb_name, 0, Prio::Background).await?
            }
        };
        tracing::info!(user_id, listens = entries.len(), "indexed");
        self.db.store(user_id, entries, true).await?;
        // Listens that arrived after the export. The count is read first, so a listen arriving in between doesn't widen the gap.
        let user = self.db.user(user_id).await?;
        let lb_count = self.lb.listen_count(&user.lb_name, Prio::Background).await?;
        let rest = self.pages(&user.lb_name, user.last_ts, Prio::Background).await?;
        self.db.store(user_id, rest, false).await?;
        self.db.finish_index(user_id, lb_count).await?;
        Ok(self.db.user(user_id).await?.listens)
    }

    /// `update` plus a comparison with LB's total count. After a backdated import or a deletion the counts
    /// differ: `true` then means the stored history is out of date. Nothing is refetched automatically,
    /// because a full download needs the user's token again (`/login`) and paging all of it is wasteful.
    pub async fn sync(&self, user: &User) -> Result<(usize, bool)> {
        let mut added = 0;
        // Two tries: if a new listen arrives between counting and fetching, the second round covers it.
        for _ in 0..2 {
            let lb_count = self.lb.listen_count(&user.lb_name, Prio::Command).await?;
            let fresh = self.db.user(user.id).await?;
            let Some(n) = self.update(&fresh, Prio::Command).await? else { return Ok((added, false)) };
            added += n;
            let now = self.db.user(user.id).await?;
            if lb_count - now.listens as i64 == now.count_offset as i64 {
                return Ok((added, false));
            }
            tracing::info!(user_id = user.id, lb_count, stored = now.listens, "count drift");
        }
        Ok((added, true))
    }

    /// Fetches recent listens and returns how many were new.
    /// `None`: the user isn't indexed or another job holds them; nothing was done.
    pub async fn update(&self, user: &User, prio: Prio) -> Result<Option<usize>> {
        if !user.indexed {
            return Ok(None);
        }
        let Some(_guard) = self.claim(user.id) else { return Ok(None) };
        let entries = self.pages(&user.lb_name, user.last_ts, prio).await?;
        let n = entries.len();
        self.db.store(user.id, entries, false).await?;
        Ok(Some(n))
    }

    async fn pages(&self, lb_name: &str, after: i64, prio: Prio) -> Result<Vec<Entry>> {
        let mut out = Vec::new();
        let mut pager = Pager::new(after);
        loop {
            let page = self.lb.listens_after(lb_name, pager.cursor, prio).await?;
            let (fresh, done) = pager.take(page);
            if !fresh.is_empty() {
                out.extend(self.db.intern(&fresh).await?);
            }
            if done {
                break;
            }
        }
        Ok(out)
    }

    /// Asks LB for an export and waits until it is ready; returns its id.
    async fn wait_export(&self, token: &str) -> Result<i64> {
        let id = self.lb.export(token).await?;
        let mut delay = 30;
        let mut waited = 0;
        loop {
            match self.lb.export_status(token, id).await?.as_str() {
                "completed" => break,
                "failed" => anyhow::bail!("export {id} failed"),
                _ if waited > 2 * 3600 => anyhow::bail!("export {id} still pending after 2h"),
                _ => {}
            }
            tokio::time::sleep(Duration::from_secs(delay)).await;
            waited += delay;
            delay = (delay * 2).min(120);
        }
        Ok(id)
    }

    async fn export_entries(&self, token: &str, id: i64) -> Result<Vec<Entry>> {
        let path = std::env::temp_dir().join(format!("lbbot-export-{id}.zip"));
        let _cleanup = RemoveOnDrop(path.clone());
        self.lb.download_export(token, id, &path).await?;

        // Read the ZIP on a blocking thread and send batches of 5000; memory stays bounded.
        let (tx, mut rx) = mpsc::channel::<Vec<Listen>>(2);
        let reader = tokio::task::spawn_blocking(move || read_export(&path, tx));
        let mut out = Vec::new();
        while let Some(batch) = rx.recv().await {
            out.extend(self.db.intern(&batch).await?);
        }
        reader.await??;
        Ok(out)
    }
}

/// Cursor for paging. Imports often have many listens in the same second, so the next page starts again
/// at that second (one less, since `min_ts` is exclusive) and listens already taken are dropped by msid.
struct Pager {
    cursor: i64,
    edge_ts: i64,
    edge: HashSet<String>,
}

impl Pager {
    fn new(after: i64) -> Self {
        Self { cursor: after, edge_ts: i64::MIN, edge: HashSet::new() }
    }

    /// Returns the page's new listens and whether there are no more pages.
    fn take(&mut self, page: Vec<Listen>) -> (Vec<Listen>, bool) {
        let full = page.len() >= PAGE;
        let fresh: Vec<Listen> = page
            .into_iter()
            .filter(|l| {
                !(l.listened_at as i64 == self.edge_ts && l.recording_msid.as_ref().is_some_and(|m| self.edge.contains(m)))
            })
            .collect();
        let Some(newest) = fresh.iter().map(|l| l.listened_at as i64).max() else {
            // shortcut: with 1000+ listens in one second the page keeps returning the same listens; the rest of that second is skipped.
            if full && self.cursor < self.edge_ts {
                self.cursor = self.edge_ts;
                return (fresh, false);
            }
            return (fresh, true);
        };
        if newest != self.edge_ts {
            self.edge_ts = newest;
            self.edge.clear();
        }
        let at_edge = fresh.iter().filter(|l| l.listened_at as i64 == newest);
        self.edge.extend(at_edge.filter_map(|l| l.recording_msid.clone()));
        self.cursor = newest - 1;
        (fresh, !full)
    }
}

/// Like fmbot, twice a day: fetches new listens for users who don't run commands, so WhoKnows lists show
/// everyone's current counts. Users who ran a command meanwhile only cost one request.
pub async fn refresh_loop(ingest: Arc<Ingest>) {
    loop {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
        tokio::time::sleep(Duration::from_secs(secs_until_refresh(now))).await;
        let users = match ingest.db.active_users().await {
            Ok(users) => users,
            Err(e) => {
                tracing::warn!(error = ?e, "refresh: listing users failed");
                continue;
            }
        };
        let (mut added, mut failures) = (0, 0);
        for user in &users {
            match ingest.update(user, Prio::Background).await {
                Ok(n) => {
                    added += n.unwrap_or(0);
                    failures = 0;
                }
                Err(e) => {
                    tracing::warn!(user_id = user.id, error = ?e, "refresh failed");
                    failures += 1;
                    // Several users failing in a row means LB is down; each failure already waited through retries.
                    if failures == 5 {
                        tracing::warn!("refresh stopped after 5 failures in a row; trying again at the next slot");
                        break;
                    }
                }
            }
        }
        tracing::info!(users = users.len(), added, "refreshed listens");
    }
}

/// Seconds until the next 06:00 or 14:00 UTC (fmbot's update times).
fn secs_until_refresh(now: u64) -> u64 {
    const DAY: u64 = 86_400;
    let today = now - now % DAY;
    [6, 14, 30].iter().map(|h| today + h * 3600).find(|&t| t > now).expect("06:00 tomorrow is later") - now
}

fn read_export(path: &std::path::Path, tx: mpsc::Sender<Vec<Listen>>) -> Result<()> {
    let mut zip = zip::ZipArchive::new(std::fs::File::open(path)?)?;
    let mut batch = Vec::with_capacity(5000);
    for i in 0..zip.len() {
        let file = zip.by_index(i)?;
        let name = file.name()?.into_owned();
        if !(name.starts_with("listens/") && name.ends_with(".jsonl")) {
            continue;
        }
        for line in BufReader::new(file).lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let listen: Listen = serde_json::from_str(&line).with_context(|| format!("bad listen in {name}"))?;
            batch.push(listen);
            if batch.len() == 5000 {
                tx.blocking_send(std::mem::take(&mut batch)).context("receiver gone")?;
            }
        }
    }
    if !batch.is_empty() {
        tx.blocking_send(batch).context("receiver gone")?;
    }
    Ok(())
}

struct RemoveOnDrop(PathBuf);
impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use crate::lb::{Listen, PAGE, TrackMetadata};

    fn listen(ts: i64, msid: usize) -> Listen {
        Listen {
            listened_at: ts as f64,
            recording_msid: Some(format!("m{msid}")),
            track_metadata: TrackMetadata { artist_name: "A".into(), track_name: "T".into(), ..Default::default() },
        }
    }

    /// Behaves like LB: the first PAGE listens after `min_ts`.
    fn drain(all: &[Listen], after: i64) -> Vec<Listen> {
        let mut pager = super::Pager::new(after);
        let mut out = Vec::new();
        for _ in 0..100 {
            let page: Vec<Listen> = all.iter().filter(|l| l.listened_at as i64 > pager.cursor).take(PAGE).cloned().collect();
            let (fresh, done) = pager.take(page);
            out.extend(fresh);
            if done {
                return out;
            }
        }
        panic!("pager did not finish");
    }

    #[test]
    fn pager_keeps_listens_that_share_a_second_across_pages() {
        // The page edge (the 999th and 1000th listen) falls in the middle of a group of 5 listens in the same second.
        let mut all: Vec<Listen> = (0..997).map(|i| listen(100 + i, i as usize)).collect();
        all.extend((0..5).map(|j| listen(2000, 5000 + j)));
        all.extend((0..1500).map(|i| listen(3000 + i, 10_000 + i as usize)));
        let got = drain(&all, 0);
        let mut ids: Vec<_> = got.iter().map(|l| l.recording_msid.clone().unwrap()).collect();
        ids.sort();
        ids.dedup();
        assert_eq!((got.len(), ids.len()), (all.len(), all.len()));
    }

    /// Real API: `LB_USER=<name> cargo test -- --ignored live_pager --nocapture`
    #[tokio::test]
    #[ignore]
    async fn live_pager() {
        let user = std::env::var("LB_USER").unwrap_or_else(|_| "rob".into());
        let lb = crate::lb::Lb::new(String::new());
        let mut pager = super::Pager::new(0);
        let mut ids = std::collections::HashSet::new();
        let mut total = 0;
        loop {
            let page = lb.listens_after(&user, pager.cursor, crate::lb::Prio::Background).await.unwrap();
            let (fresh, done) = pager.take(page);
            total += fresh.len();
            ids.extend(fresh.into_iter().map(|l| (l.listened_at as i64, l.recording_msid)));
            if done {
                break;
            }
        }
        println!("{user}: {total} listens, {} unique", ids.len());
        assert_eq!(total, ids.len(), "pager returned duplicates");
    }

    #[test]
    fn refresh_runs_at_six_and_fourteen_utc() {
        let day = 20_000 * 86_400;
        assert_eq!(super::secs_until_refresh(day), 6 * 3600);
        assert_eq!(super::secs_until_refresh(day + 6 * 3600), 8 * 3600);
        assert_eq!(super::secs_until_refresh(day + 14 * 3600 - 1), 1);
        assert_eq!(super::secs_until_refresh(day + 20 * 3600), 10 * 3600);
    }

    #[test]
    fn pager_terminates_when_one_second_fills_a_page() {
        let mut all: Vec<Listen> = (0..10).map(|i| listen(100 + i, i as usize)).collect();
        all.extend((0..1200).map(|j| listen(500, 1000 + j)));
        all.push(listen(900, 9999));
        let got = drain(&all, 0);
        assert!(got.iter().any(|l| l.listened_at as i64 == 900), "listens after the crowded second must still arrive");
    }

    #[test]
    fn read_export_reads_all_listen_files() {
        let dir = std::env::temp_dir().join(format!("bb-export-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("e.zip");
        let mut zip = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
        let line = |ts: u32| format!(r#"{{"listened_at":{ts}.0,"track_metadata":{{"artist_name":"A","track_name":"T{ts}"}}}}"#);
        for (name, body) in [
            ("user.json", "{}".to_owned()),
            ("listens/2024/1.jsonl", format!("{}\n{}\n", line(1), line(2))),
            ("listens/2025/12.jsonl", format!("{}\n\n", line(3))),
            ("feedback.jsonl", line(9)),
        ] {
            zip.start_file(name, zip::write::SimpleFileOptions::default()).unwrap();
            zip.write_all(body.as_bytes()).unwrap();
        }
        zip.finish().unwrap();

        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        super::read_export(&path, tx).unwrap();
        let mut ts = Vec::new();
        while let Ok(batch) = rx.try_recv() {
            ts.extend(batch.into_iter().map(|l| l.listened_at as i64));
        }
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(ts, [1, 2, 3]);
    }
}
