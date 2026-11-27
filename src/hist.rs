//! Commands computed from the stored history (fmbot's time-based and summary commands). Math lives in `stats`.

use std::collections::HashMap;

use anyhow::Result;
use twilight_model::channel::message::Embed;
use twilight_util::builder::embed::{EmbedBuilder, EmbedFooterBuilder};

use twilight_model::{
    channel::message::{
        Component,
        component::{ActionRow, Button, ButtonStyle},
    },
    http::attachment::Attachment,
};
use twilight_util::builder::embed::ImageSource;

use crate::{
    bot::{Art, Bot, COLOR, Ctx, Hc, Out, cache_get, cache_put, escape, reply},
    db::{Kind, User},
    history::Entry,
    lb::Prio,
    stats::{self, DAY},
    tz::Tz,
};

/// A user's history with each track's artist, so any kind can be counted, and the time zone their days are in.
pub(crate) struct Hist {
    pub entries: Vec<Entry>,
    /// The artist of each track.
    pub artist: HashMap<i32, i32>,
    pub tz: Tz,
}

impl Hist {
    pub(crate) fn key(&self, kind: Kind) -> impl Fn(&Entry) -> Option<i32> + '_ {
        move |e| match kind {
            Kind::Artist => self.artist.get(&e.track).copied(),
            Kind::Album => (e.album != 0).then_some(e.album),
            Kind::Track => Some(e.track),
        }
    }

    /// Today's local day number.
    pub(crate) fn today(&self) -> i64 {
        self.tz.day(now())
    }

    /// The listens of local day `day`.
    pub(crate) fn day(&self, day: i64) -> &[Entry] {
        stats::range(&self.entries, self.tz.start(day), self.tz.start(day + 1))
    }

    /// Footer line for commands that count by day or hour.
    pub(crate) fn tz_note(&self) -> String {
        if self.tz.is_set() { format!("Days are {}", self.tz.name()) } else { "Days are UTC · set your time zone with /timezone".to_owned() }
    }
}

/// Words a command can take in any order: a period, a kind, a number; the rest is a name.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Opts {
    pub period: Option<(Option<i64>, String)>,
    pub kind: Option<Kind>,
    pub number: Option<i64>,
    pub rest: String,
}

pub(crate) fn opts(arg: &str) -> Opts {
    let mut o = Opts::default();
    let mut rest = Vec::new();
    for w in arg.split_whitespace() {
        let kind = match w.to_lowercase().as_str() {
            "artist" | "artists" => Some(Kind::Artist),
            "album" | "albums" => Some(Kind::Album),
            "track" | "tracks" => Some(Kind::Track),
            _ => None,
        };
        if o.kind.is_none() && kind.is_some() {
            o.kind = kind;
        } else if let Some(p) = o.period.is_none().then(|| stats::period(w)).flatten() {
            o.period = Some(p);
        } else if let Some(n) = o.number.is_none().then(|| w.replace([',', '.'], "").parse().ok()).flatten() {
            o.number = Some(n);
        } else {
            rest.push(w);
        }
    }
    o.rest = rest.join(" ");
    o
}

/// The Discord id in a `<@id>` or `<@!id>` mention at the start of `arg`, and what follows it.
pub(crate) fn mention(arg: &str) -> Option<(i64, &str)> {
    let inner = arg.strip_prefix("<@")?;
    let (id, rest) = inner.split_once('>')?;
    Some((id.trim_start_matches('!').parse().ok()?, rest.trim()))
}

/// The overview covers this many days, `PER_PAGE` days with listens per page.
const OVERVIEW_DAYS: i64 = 28;
const PER_PAGE: usize = 4;

/// ⏮ ◀ ▶ ⏭. The id carries who may press, the day the overview was made (so pages don't shift at midnight)
/// and the target page; the last part keeps ids unique when two buttons go to the same page.
fn page_buttons(owner: i64, today: i64, page: usize, pages: usize) -> Component {
    let targets = [0, page.saturating_sub(1), (page + 1).min(pages - 1), pages - 1];
    let components = ["⏮", "◀", "▶", "⏭"]
        .into_iter()
        .zip(targets)
        .enumerate()
        .map(|(slot, (label, target))| {
            Component::Button(Button {
                custom_id: Some(format!("ov:{owner}:{today}:{target}:{slot}")),
                disabled: target == page,
                emoji: None,
                id: None,
                label: Some(label.to_owned()),
                sku_id: None,
                style: ButtonStyle::Secondary,
                url: None,
            })
        })
        .collect();
    Component::ActionRow(ActionRow { components, id: None })
}

/// (owner, day, page) from an overview button id.
pub(crate) fn overview_button(id: &str) -> Option<(i64, i64, usize)> {
    let mut parts = id.strip_prefix("ov:")?.split(':');
    Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
}

pub(crate) fn with_note(mut embed: Embed, note: Option<&str>) -> Embed {
    if let Some(n) = note {
        let footer = embed.footer.take().map_or(n.to_owned(), |f| format!("{}\n{n}", f.text));
        embed.footer = Some(EmbedFooterBuilder::new(footer).build());
    }
    embed
}

pub(crate) fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs() as i64
}

pub(crate) fn date(ts: i64) -> String {
    format!("<t:{ts}:D>")
}

pub(crate) fn ago(ts: i64) -> String {
    format!("<t:{ts}:R>")
}

pub(crate) fn days(secs: i64) -> String {
    match secs / DAY {
        0 => "less than a day".to_owned(),
        1 => "1 day".to_owned(),
        n => format!("{n} days"),
    }
}

/// "Mon 25 Nov 2026" for a day number; field names don't render Discord timestamps.
pub(crate) fn day_label(day: i64) -> String {
    const WEEK: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let (y, m, d) = stats::civil(day);
    format!("{} {d} {} {y}", WEEK[day.rem_euclid(7) as usize], MONTHS[m as usize - 1])
}

fn ordinal(n: i64) -> String {
    let suffix = match (n % 10, n % 100) {
        (_, 11..=13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
}

pub(crate) fn kind_word(kind: Kind) -> &'static str {
    match kind {
        Kind::Artist => "artists",
        Kind::Album => "albums",
        Kind::Track => "tracks",
    }
}

/// `1\.` so Discord doesn't turn the lines into a Markdown list.
pub(crate) fn numbered(lines: impl IntoIterator<Item = String>) -> String {
    let lines: Vec<String> = lines.into_iter().enumerate().map(|(i, l)| format!("{}\\. {l}", i + 1)).collect();
    if lines.is_empty() { "Nothing yet.".to_owned() } else { lines.join("\n") }
}

/// From the period words: (start, label); `default_days` when none was given, `None` = all time.
pub(crate) fn since(o: &Opts, default_days: Option<i64>, default_label: &str) -> (i64, String) {
    let (d, label) = o.period.clone().unwrap_or((default_days, default_label.to_owned()));
    (d.map_or(i64::MIN, |d| now() - d * DAY), label)
}

impl Bot {
    pub(crate) async fn hist(&self, ctx: &Ctx, user: &User, cmd: Hc, kind: Kind, arg: &str) -> Result<Out> {
        let note = (!user.indexed).then_some("Your history is still downloading, so counts may be incomplete.");
        // The server iceberg reads the members' counters, not this user's history.
        if cmd == Hc::ServerIceberg {
            return match ctx.guild {
                Some(g) => self.server_iceberg(g, note).await,
                None => Ok(reply("The server iceberg only works in servers.").into()),
            };
        }
        if let Err(e) = self.ingest.update(user, Prio::Command).await {
            tracing::warn!(error = ?e, "update during history command failed");
        }
        let h = self.load(user).await?;
        let o = opts(arg);
        let out = match cmd {
            Hc::Overview => return self.overview(ctx, &h, h.today(), 0, note).await,
            Hc::Streak => self.streak(&h).await?,
            Hc::Streaks => {
                let kind = o.kind.unwrap_or(Kind::Artist);
                let rows = stats::streaks(&h.entries, h.key(kind), 3);
                let names = self.names_of(kind, rows.iter().take(10).map(|r| r.0)).await?;
                let lines = rows.iter().take(10).map(|(k, n, t)| format!("**{}**: {n} plays in a row, {}", names[k], date(*t)));
                embed(format!("Longest {} streaks", kind_word(kind)), numbered(lines))
            }
            Hc::Gaps => {
                let kind = o.kind.unwrap_or(Kind::Artist);
                let rows = stats::gaps(&h.entries, h.key(kind));
                let names = self.names_of(kind, rows.iter().take(10).map(|r| r.0)).await?;
                let lines = rows.iter().take(10).map(|(k, g, t)| format!("**{}**: back after {}, {}", names[k], days(*g), date(*t)));
                embed(format!("{} you came back to", capitalized(kind_word(kind))), numbered(lines))
            }
            Hc::Discoveries => {
                let kind = o.kind.unwrap_or(Kind::Artist);
                let (from, label) = since(&o, Some(30), "month");
                let first = stats::first_plays(&h.entries, h.key(kind));
                let new: Vec<(i32, u32)> =
                    stats::top(stats::range(&h.entries, from, i64::MAX), h.key(kind)).into_iter().filter(|(k, _)| first[k] >= from).collect();
                let names = self.names_of(kind, new.iter().take(15).map(|r| r.0)).await?;
                let lines = new.iter().take(15).map(|(k, n)| format!("**{}**: {n} plays, first on {}", names[k], date(first[k])));
                embed(format!("New {} ({label}): {}", kind_word(kind), new.len()), numbered(lines))
            }
            Hc::DiscoveryDate | Hc::LastListened => match self.find_in(user, kind, arg).await? {
                Ok((name, id)) => {
                    let name = escape(&name);
                    let ts: Vec<i64> = h.entries.iter().filter(|e| h.key(kind)(e) == Some(id)).map(|e| e.ts).collect();
                    let (count, first, last) = (ts.len(), ts.first().copied(), ts.last().copied());
                    let text = match (cmd, first, last) {
                        (Hc::DiscoveryDate, Some(t), _) => format!("You first listened to **{name}** on {} ({}).", date(t), ago(t)),
                        (_, _, Some(t)) => format!("You last listened to **{name}** on {} ({}).", date(t), ago(t)),
                        _ => format!("You haven't listened to **{name}**."),
                    };
                    embed(String::new(), format!("{text}\n{count} plays in total."))
                }
                Err(e) => return Ok(e.into()),
            },
            Hc::Pace => {
                let total = h.entries.len() as i64 + i64::from(user.count_offset);
                let goal = o.number.unwrap_or_else(|| stats::next_goal(total, false));
                pace_embed("Your pace", &o, total, goal, &h.entries, |_| true)
            }
            Hc::ArtistPace => {
                let (goal, artist) = goal_and_name(arg);
                match self.find_in(user, Kind::Artist, artist).await? {
                    Ok((name, id)) => {
                        let key = h.key(Kind::Artist);
                        let total = h.entries.iter().filter(|e| key(e) == Some(id)).count() as i64;
                        let goal = goal.unwrap_or_else(|| stats::next_goal(total, true));
                        pace_embed(&format!("Your pace for {name}"), &Opts::default(), total, goal, &h.entries, |e| key(e) == Some(id))
                    }
                    Err(e) => return Ok(e.into()),
                }
            }
            Hc::Milestone => {
                let total = h.entries.len() as i64;
                if total == 0 {
                    return Ok(reply("No listens yet.").into());
                }
                let n = o.number.unwrap_or_else(|| stats::last_milestone(total));
                match usize::try_from(n - 1).ok().and_then(|i| h.entries.get(i)) {
                    Some(e) => {
                        let names = self.names_of(Kind::Track, [e.track]).await?;
                        embed(format!("Your {} listen", ordinal(n)), format!("**{}**\n{} ({})", names[&e.track], date(e.ts), ago(e.ts)))
                    }
                    None => return Ok(reply(format!("You have {total} stored listens; pick a number from 1 to {total}.")).into()),
                }
            }
            Hc::Year => {
                let this = stats::civil(h.today()).0;
                let year = o.number.filter(|y| (1970..=this).contains(y)).unwrap_or(this);
                let start = |y: i64| h.tz.start(stats::days_from_civil(y, 1, 1));
                let (from, to) = (start(year), start(year + 1));
                let before = stats::range(&h.entries, start(year - 1), from).len();
                let e = self.summary(&h, from, to, format!("{year} in music")).await?;
                let plays = stats::range(&h.entries, from, to).len();
                let mut footer = h.tz_note();
                if before > 0 {
                    footer = format!("{:+.0}% plays compared to {}\n{footer}", (plays as f64 / before as f64 - 1.0) * 100.0, year - 1);
                }
                e.footer(EmbedFooterBuilder::new(footer))
            }
            Hc::Recap => {
                let (from, label) = since(&o, Some(30), "month");
                let e = self.summary(&h, from, i64::MAX, format!("Your {label}")).await?;
                let mut footer = h.tz_note();
                // Alistral's reports compare each period with the one before it; all time has none.
                if from > i64::MIN {
                    let plays = stats::range(&h.entries, from, i64::MAX).len();
                    let before = stats::range(&h.entries, from - (now() - from), from).len();
                    if before > 0 {
                        footer = format!("{:+.0}% plays compared to the {label} before\n{footer}", (plays as f64 / before as f64 - 1.0) * 100.0);
                    }
                }
                e.footer(EmbedFooterBuilder::new(footer))
            }
            Hc::Taste => match ctx.guild {
                Some(g) => return Ok(self.taste(user, &h, g, arg).await?.into()),
                None => return Ok(reply("Taste only works in servers.").into()),
            },
            Hc::Affinity => match ctx.guild {
                Some(g) => self.affinity(user, &h, g).await?,
                None => return Ok(reply("Affinity only works in servers.").into()),
            },
            Hc::Iceberg => {
                let (from, label) = since(&o, None, "all time");
                return self.iceberg(ctx, &h, from, &label, note).await;
            }
            Hc::Receipt => {
                let (from, label) = since(&o, Some(30), "month");
                self.receipt(&h, user, from, &label).await?
            }
            Hc::Overdue => self.overdue(&h, &o).await?,
            Hc::Comebacks => self.comebacks(&h, &o).await?,
            Hc::OnThisDay => self.on_this_day(&h).await?,
            Hc::Movers => self.movers(&h, &o).await?,
            Hc::Growth => return self.growth(ctx, user, &h, &o, note).await,
            Hc::Obsessions => self.obsessions(&h).await?,
            Hc::Hidden => self.hidden(&h, &o).await?,
            Hc::Obscurity => self.obscurity(ctx, user, &h, &o).await?,
            Hc::ServerIceberg => unreachable!("handled above"),
            Hc::Decades => return self.decades(ctx, &h, &o, note).await,
            Hc::ListeningTime => self.listening_time(&h, &o).await?,
            Hc::Clock => return self.clock(ctx, &h, &o, note).await,
            Hc::Unlinked => self.unlinked(&h).await?,
        };
        Ok(with_note(out.build(), note).into())
    }

    /// A page turn on an overview: the same days as when it was sent, without fetching new listens.
    pub(crate) async fn overview_turn(&self, ctx: &Ctx, today: i64, page: usize) -> Result<Out> {
        let Some(user) = self.registered(ctx).await? else { return Ok(reply("Your account is gone.").into()) };
        let h = self.load(&user).await?;
        self.overview(ctx, &h, today, page, None).await
    }

    /// Genres of an artist from MusicBrainz tags: the card cache first, else one search, cached for a day under its own key.
    /// `None` when MusicBrainz is busy or has no tags; a busy answer isn't cached.
    async fn artist_tags(&self, artist: &str) -> Option<String> {
        let name = artist.to_lowercase();
        let (card, own) = ((Kind::Artist, name.clone(), String::new()), (Kind::Artist, name, "tags".to_owned()));
        if let Some(art) = [&card, &own].into_iter().find_map(|k| cache_get(&self.art_cache.lock().unwrap(), k)) {
            return art.tags;
        }
        let q = format!("artist:{}", crate::lb::phrase(artist));
        let found = self.ingest.lb.mb_search("artist", &q).await.inspect_err(|e| tracing::debug!(error = ?e, "genre lookup skipped")).ok()?;
        let tags = found.as_ref().and_then(crate::lb::tags);
        let art = Art { tags: tags.clone(), ..Art::default() };
        cache_put(&mut self.art_cache.lock().unwrap(), own, art, std::time::Instant::now());
        tags
    }

    pub(crate) async fn load(&self, user: &User) -> Result<Hist> {
        let entries = self.db().history(user.id).await?;
        let mut tracks: Vec<i32> = entries.iter().map(|e| e.track).collect();
        tracks.sort_unstable();
        tracks.dedup();
        Ok(Hist { artist: self.db().track_artists(&tracks).await?, entries, tz: Tz::new(user.tz.as_deref()) })
    }

    /// Escaped display names; an id without one shows as "?".
    pub(crate) async fn names_of(&self, kind: Kind, ids: impl IntoIterator<Item = i32>) -> Result<HashMap<i32, String>> {
        let ids: Vec<i32> = ids.into_iter().collect();
        let mut names: HashMap<i32, String> = self.db().names(kind, &ids).await?.into_iter().map(|(k, v)| (k, escape(&v))).collect();
        for id in ids {
            names.entry(id).or_insert_with(|| "?".into());
        }
        Ok(names)
    }

    /// The id of the typed or playing artist/album/track, with its name, unescaped (it also goes into titles,
    /// where Discord shows backslashes as they are). `Err` is a reply for the user.
    pub(crate) async fn find_in(&self, user: &User, kind: Kind, query: &str) -> Result<Result<(String, i32), Embed>> {
        let query = (!query.trim().is_empty()).then(|| query.trim().to_owned());
        let (artist, name, _) = match self.subject(user, kind, query).await? {
            Ok(s) => s,
            Err(e) => return Ok(Err(e)),
        };
        Ok(match self.db().find(kind, &artist, name.as_deref()).await? {
            Some((id, title)) => Ok((title, id)),
            None => Err(reply(format!("You haven't listened to **{}**.", escape(&name.unwrap_or(artist))))),
        })
    }

    /// fmbot's overview: days with listens from the last four weeks, four per page, newest first.
    async fn overview(&self, ctx: &Ctx, h: &Hist, today: i64, page: usize, note: Option<&str>) -> Result<Out> {
        let days: Vec<i64> = (today - OVERVIEW_DAYS + 1..=today).rev().filter(|d| !h.day(*d).is_empty()).collect();
        if days.is_empty() {
            return Ok(reply("No listens in the last four weeks.").into());
        }
        let pages = days.len().div_ceil(PER_PAGE);
        let page = page.min(pages - 1);
        let shown = &days[page * PER_PAGE..((page + 1) * PER_PAGE).min(days.len())];
        let mut sections = Vec::new();
        for &day in shown {
            let slice = h.day(day);
            let mut lines = vec![format!("**{}** · {} plays", day_label(day), slice.len())];
            let mut genre_done = false;
            for kind in [Kind::Artist, Kind::Album, Kind::Track] {
                let Some(&(id, n)) = stats::top(slice, h.key(kind)).first() else { continue };
                let name = self.db().names(kind, &[id]).await?.remove(&id).unwrap_or_else(|| "?".into());
                if !genre_done {
                    genre_done = true;
                    if let Some(tags) = self.artist_tags(&name).await {
                        lines.push(format!("*{}*", escape(&tags)));
                    }
                }
                lines.push(format!("{} · *{n} plays*", escape(&name)));
            }
            sections.push(lines.join("\n"));
        }
        let slice = stats::range(&h.entries, h.tz.start(shown[shown.len() - 1]), h.tz.start(shown[0] + 1));
        let plays: usize = shown.iter().map(|d| h.day(*d).len()).sum();
        let tracks = stats::top(slice, |e| Some(e.track)).len();
        let footer = format!(
            "{}/{pages} · {tracks} different tracks · {plays} plays · {:.1} avg\n{}",
            page + 1,
            plays as f64 / shown.len() as f64,
            h.tz_note()
        );
        let embed = EmbedBuilder::new()
            .color(COLOR)
            .title(format!("Daily overview for {}", ctx.name).chars().take(256).collect::<String>())
            .description(sections.join("\n\n").chars().take(4096).collect::<String>())
            .footer(EmbedFooterBuilder::new(footer))
            .build();
        let components = if pages > 1 { vec![page_buttons(ctx.discord_id, today, page, pages)] } else { Vec::new() };
        Ok(Out { embed: with_note(embed, note), components, files: Vec::new() })
    }

    async fn streak(&self, h: &Hist) -> Result<EmbedBuilder> {
        let mut lines = Vec::new();
        let mut started = None;
        for kind in [Kind::Artist, Kind::Album, Kind::Track] {
            if let Some((id, n, t)) = stats::streak(&h.entries, h.key(kind)).filter(|s| s.1 >= 2) {
                let name = &self.names_of(kind, [id]).await?[&id];
                lines.push(format!("{}: **{name}**, {n} plays in a row", capitalized(&kind_word(kind)[..kind_word(kind).len() - 1])));
                started = started.min(Some(t)).or(Some(t));
            }
        }
        Ok(match started {
            Some(t) => embed("Current streak".into(), format!("{}\nStarted {}", lines.join("\n"), ago(t))),
            None => embed("Current streak".into(), "No streak: your last two listens are by different artists.".into()),
        })
    }

    /// Plays, new artists, the busiest day and the top 5 of each kind in [from, to).
    async fn summary(&self, h: &Hist, from: i64, to: i64, title: String) -> Result<EmbedBuilder> {
        let slice = stats::range(&h.entries, from, to);
        if slice.is_empty() {
            return Ok(embed(title, "No listens in this period.".into()));
        }
        let first = stats::first_plays(&h.entries, h.key(Kind::Artist));
        let [artists, albums, tracks] = [Kind::Artist, Kind::Album, Kind::Track].map(|k| stats::top(slice, h.key(k)));
        let new = artists.iter().filter(|(k, _)| first[k] >= slice[0].ts).count();
        let busiest = stats::top(slice, |e| Some(h.tz.day(e.ts) as i32))[0];
        let mut desc = format!(
            "**{}** plays · **{}** artists ({new} new) · **{}** albums · **{}** tracks\nBusiest day: {} with {} plays",
            slice.len(),
            artists.len(),
            albums.len(),
            tracks.len(),
            day_label(i64::from(busiest.0)),
            busiest.1,
        );
        for (kind, all) in [(Kind::Artist, &artists), (Kind::Album, &albums), (Kind::Track, &tracks)] {
            let top = &all[..all.len().min(5)];
            let names = self.names_of(kind, top.iter().map(|t| t.0)).await?;
            desc.push_str(&format!("\n\n**Top {}**\n{}", kind_word(kind), numbered(top.iter().map(|(k, n)| format!("{} ({n})", names[k])))));
        }
        Ok(embed(title, desc))
    }

    /// Only with members of this server: a mention is just an id, and hidden (`/privacy`) users appear only in their own servers.
    async fn taste(&self, user: &User, mine: &Hist, guild: i64, arg: &str) -> Result<Embed> {
        let Some((id, rest)) = mention(arg) else { return Ok(reply("Mention someone to compare with: `,taste @user`")) };
        let other = match self.db().user_by_discord(id).await? {
            Some(o) if self.db().is_member(guild, o.id).await? => o,
            // The same answer either way, so it doesn't tell whether an outsider has an account.
            _ => return Ok(reply("They haven't connected a ListenBrainz account in this server.")),
        };
        if other.id == user.id {
            return Ok(reply("That's you. Mention someone else."));
        }
        let theirs = self.load(&other).await?;
        let (from, label) = since(&opts(rest), None, "all time");
        let count = |h: &Hist| -> (HashMap<i32, i64>, i64) {
            let slice = stats::range(&h.entries, from, i64::MAX);
            (stats::top(slice, h.key(Kind::Artist)).into_iter().map(|(k, n)| (k, i64::from(n))).collect(), slice.len() as i64)
        };
        let ((a, at), (b, bt)) = (count(mine), count(&theirs));
        let score = stats::affinity(&a, at, &b, bt) * 100.0;
        let mut shared: Vec<(i32, i64, i64)> = a.iter().filter_map(|(k, pa)| b.get(k).map(|pb| (*k, *pa, *pb))).collect();
        shared.sort_unstable_by_key(|(k, pa, pb)| (std::cmp::Reverse((*pa * bt).min(*pb * at)), *k));
        let names = self.names_of(Kind::Artist, shared.iter().take(15).map(|s| s.0)).await?;
        let lines = shared.iter().take(15).map(|(k, pa, pb)| format!("**{}**: {pa} · {pb}", names[k]));
        let desc = format!("**{score:.0}%** match ({} shared artists)\nYour plays · their plays\n\n{}", shared.len(), numbered(lines));
        Ok(embed(format!("Taste: you vs {} ({label})", other.lb_name), desc).build())
    }

    /// Members ranked by how much of their listening overlaps with this user's top 50 artists.
    async fn affinity(&self, user: &User, h: &Hist, guild: i64) -> Result<EmbedBuilder> {
        let mine: HashMap<i32, i64> = stats::top(&h.entries, h.key(Kind::Artist)).into_iter().take(50).map(|(k, n)| (k, i64::from(n))).collect();
        let ids: Vec<i32> = mine.keys().copied().collect();
        let mut others: HashMap<i32, (String, i64, HashMap<i32, i64>)> = HashMap::new();
        for (uid, name, total, artist, plays) in self.db().member_artist_plays(guild, &ids).await? {
            if uid != user.id {
                others.entry(uid).or_insert_with(|| (name, total, HashMap::new())).2.insert(artist, plays);
            }
        }
        let mut ranked: Vec<(f64, String)> = others
            .into_values()
            .map(|(name, total, plays)| (stats::affinity(&mine, h.entries.len() as i64, &plays, total), name))
            .collect();
        ranked.sort_unstable_by(|a, b| b.0.total_cmp(&a.0));
        let lines = ranked.iter().take(15).map(|(s, name)| format!("**{}**: {:.0}%", escape(name), s * 100.0));
        Ok(embed("Who listens like you here".into(), numbered(lines)))
    }

    async fn iceberg(&self, ctx: &Ctx, h: &Hist, from: i64, label: &str, note: Option<&str>) -> Result<Out> {
        let slice = stats::range(&h.entries, from, i64::MAX);
        let artists: Vec<i32> = stats::top(slice, h.key(Kind::Artist)).into_iter().take(50).map(|t| t.0).collect();
        if artists.is_empty() {
            return Ok(reply(format!("No listens ({label}).")).into());
        }
        self.iceberg_picture(&ctx.name, label, &artists, "Top 50 artists", note).await
    }

    /// `artists` on the iceberg picture by how many ListenBrainz users listened to them. Their ids come from the
    /// database or one lookup (see `meta`), the counts from `/1/popularity/artist`.
    pub(crate) async fn iceberg_picture(&self, title: &str, label: &str, artists: &[i32], what: &str, note: Option<&str>) -> Result<Out> {
        let mbids = self.artist_mbids(artists).await?;
        let counts = self.popularity("artist", &mbids.values().map(String::as_str).collect::<Vec<_>>()).await?;
        let names = self.db().names(Kind::Artist, artists).await?;
        let mut tiers: Vec<Vec<String>> = vec![Vec::new(); stats::TIERS.len()];
        let mut unknown = 0;
        for a in artists {
            match mbids.get(a).and_then(|m| counts.get(m)) {
                Some(&(listeners, _)) => tiers[stats::tier(listeners)].push(names.get(a).cloned().unwrap_or_else(|| "?".into())),
                None => unknown += 1,
            }
        }
        let (title, label) = (title.to_owned(), label.to_owned());
        let png = tokio::task::spawn_blocking(move || crate::iceberg::render(&title, &label, &tiers)).await??;
        let mut footer = format!("{what}, deeper = fewer ListenBrainz listeners");
        if unknown > 0 {
            footer.push_str(&format!(" · {unknown} not found on ListenBrainz"));
        }
        Ok(picture("iceberg.png", png, footer, note))
    }

    async fn receipt(&self, h: &Hist, user: &User, from: i64, label: &str) -> Result<EmbedBuilder> {
        let slice = stats::range(&h.entries, from, i64::MAX);
        let all = stats::top(slice, |e| Some(e.track));
        let top = &all[..all.len().min(12)];
        let names = self.db().names(Kind::Track, &top.iter().map(|t| t.0).collect::<Vec<_>>()).await?;
        const WIDTH: usize = 34;
        let rule = "-".repeat(WIDTH);
        let mut text = format!("{:^WIDTH$}\n{:^WIDTH$}\n{rule}\nQTY  ITEM\n", "LISTENBRAINZ RECEIPT", format!("{} · {label}", user.lb_name));
        for (k, n) in top {
            // Backticks would end the code block.
            let item: String = names.get(k).map_or("?", String::as_str).replace('`', "'").chars().take(WIDTH - 5).collect();
            text.push_str(&format!("{n:>3}  {item}\n"));
        }
        text.push_str(&format!("{rule}\nTOTAL PLAYS{:>w$}\nTRACKS{:>v$}\n{rule}\n{:^WIDTH$}", slice.len(), all.len(), "THANK YOU FOR LISTENING", w = WIDTH - 11, v = WIDTH - 6));
        Ok(embed(String::new(), format!("```\n{text}\n```")))
    }
}

/// A reply that is one picture, with a footer.
pub(crate) fn picture(file: &str, png: Vec<u8>, footer: String, note: Option<&str>) -> Out {
    let mut embed = EmbedBuilder::new().color(COLOR).footer(EmbedFooterBuilder::new(footer));
    if let Ok(img) = ImageSource::attachment(file) {
        embed = embed.image(img);
    }
    Out { embed: with_note(embed.build(), note), components: Vec::new(), files: vec![Attachment::from_bytes(file.into(), png, 0)] }
}

pub(crate) fn embed(title: String, desc: String) -> EmbedBuilder {
    let mut e = EmbedBuilder::new().color(COLOR).description(desc.chars().take(4096).collect::<String>());
    if !title.is_empty() {
        e = e.title(title.chars().take(256).collect::<String>());
    }
    e
}

pub(crate) fn capitalized(s: &str) -> String {
    let mut c = s.chars();
    c.next().map_or_else(String::new, |f| f.to_uppercase().chain(c).collect())
}

/// A leading goal, only when a name follows it: `,apc 1000 Radiohead`, but `,apc 311` is the artist 311.
fn goal_and_name(arg: &str) -> (Option<i64>, &str) {
    match arg.split_once(' ').map(|(n, a)| (n.replace(',', "").parse(), a)) {
        Some((Ok(n), a)) => (Some(n), a.trim()),
        _ => (None, arg),
    }
}

/// When `matching` listens reach `goal` at the rate of the period (default: the last month; all time: since the first play).
fn pace_embed(title: &str, o: &Opts, total: i64, goal: i64, entries: &[Entry], matching: impl Fn(&Entry) -> bool) -> EmbedBuilder {
    let (from, label) = match o.period.clone() {
        Some((Some(d), l)) => (now() - d * DAY, format!("last {l}")),
        Some((None, l)) => (entries.iter().find(|e| matching(e)).map_or(now(), |e| e.ts), l),
        None => (now() - 30 * DAY, "last month".to_owned()),
    };
    let recent = stats::range(entries, from, i64::MAX).iter().filter(|e| matching(e)).count() as f64;
    let per_day = recent / ((now() - from) as f64 / DAY as f64).max(1.0);
    let text = if goal <= total {
        format!("You're already past **{goal}**: you have **{total}** plays.")
    } else if per_day == 0.0 {
        format!("No plays ({label}), so there's no pace to go by. You have **{total}** plays.")
    } else {
        let eta = now() + ((goal - total) as f64 / per_day * DAY as f64) as i64;
        format!("At **{per_day:.1}** plays a day ({label}), you'll reach **{goal}** plays on {} ({}).\nYou have **{total}** now.", date(eta), ago(eta))
    };
    embed(title.to_owned(), text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_and_mentions() {
        let o = opts("tracks 2,000 week Radiohead live");
        assert_eq!((o.kind, o.number, o.rest.as_str()), (Some(Kind::Track), Some(2000), "Radiohead live"));
        assert_eq!(o.period.map(|p| p.0), Some(Some(7)));
        assert_eq!(opts("").rest, "");
        assert_eq!(mention("<@!123> month"), Some((123, "month")));
        assert_eq!(mention("<@45>"), Some((45, "")));
        assert_eq!(mention("someone"), None);
        assert_eq!(numbered(["a".into(), "b".into()]), "1\\. a\n2\\. b");
        assert_eq!(capitalized("albums"), "Albums");
        assert_eq!(goal_and_name("1,000 Radiohead"), (Some(1000), "Radiohead"));
        // Page buttons: unique ids (Discord rejects duplicates), the current page's way disabled, ids read back.
        let Component::ActionRow(row) = page_buttons(7, 20_000, 0, 3) else { panic!("not a row") };
        let buttons: Vec<&Button> = row.components.iter().map(|c| match c { Component::Button(b) => b, _ => panic!() }).collect();
        let ids: std::collections::HashSet<_> = buttons.iter().map(|b| b.custom_id.clone()).collect();
        assert_eq!(ids.len(), 4);
        assert_eq!(buttons.iter().map(|b| b.disabled).collect::<Vec<_>>(), [true, true, false, false]);
        assert_eq!(overview_button(buttons[3].custom_id.as_deref().unwrap()), Some((7, 20_000, 2)));
        assert_eq!(overview_button("other:1"), None);
        assert_eq!(goal_and_name("Radiohead"), (None, "Radiohead"));
        assert_eq!(goal_and_name("311"), (None, "311"), "a lone number is an artist");
        assert_eq!(goal_and_name("The 1975"), (None, "The 1975"));
    }
}
