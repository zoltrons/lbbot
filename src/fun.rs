//! Stats commands inspired by Alistral (github.com/RustyNova016/Alistral): overdue, comebacks, all-time movers,
//! underrated tracks, plus charts and summaries in the same spirit. Math lives in `stats`, ids and facts in `meta`.

use std::collections::HashMap;

use anyhow::Result;
use twilight_util::builder::embed::{EmbedBuilder, EmbedFooterBuilder};

use crate::{
    bot::{Bot, Ctx, Out, escape, reply},
    db::{Kind, User},
    graph::Heading,
    hist::{Hist, Opts, ago, capitalized, date, day_label, days, embed, kind_word, now, numbered, picture, since},
    history::Entry,
    stats::{self, DAY},
};

/// Plays a key needs before it can be overdue: a song heard twice isn't a habit.
const OVERDUE_MIN_PLAYS: u32 = 10;
const MONTHS: [&str; 12] = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];
const WEEKDAYS: [&str; 7] = ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"];

fn footer(e: EmbedBuilder, text: impl Into<String>) -> EmbedBuilder {
    e.footer(EmbedFooterBuilder::new(text))
}

/// `,growth`'s argument: a leading year (1970 to `this`), then the artist exactly as typed, so "The 1975", "311" and
/// "All Time Low" stay whole.
fn year_and_artist(arg: &str, this: i64) -> (Option<i64>, &str) {
    let arg = arg.trim();
    let (first, rest) = arg.split_once(' ').unwrap_or((arg, ""));
    match first.parse::<i64>().ok().filter(|y| (1970..=this).contains(y)) {
        Some(y) => (Some(y), rest.trim()),
        None => (None, arg),
    }
}

/// "every 5 hours", "every 3 days".
fn every(secs: i64) -> String {
    match secs {
        s if s < 2 * 3600 => "every hour or so".to_owned(),
        s if s < DAY => format!("every {} hours", s / 3600),
        s => format!("every {}", days(s)),
    }
}

/// "37h 12m", "4 days 2h".
fn duration(ms: i64) -> String {
    let m = ms / 60_000;
    match m {
        m if m < 60 => format!("{m}m"),
        m if m < 48 * 60 => format!("{}h {}m", m / 60, m % 60),
        m => format!("{} days {}h", m / (24 * 60), m / 60 % 24),
    }
}

fn percent(part: f64, whole: f64) -> f64 {
    if whole > 0.0 { part * 100.0 / whole } else { 0.0 }
}

impl Bot {
    /// Alistral's overdue radio as a list: what you played often and, at your usual pace, should have played again.
    pub(crate) async fn overdue(&self, h: &Hist, o: &Opts) -> Result<EmbedBuilder> {
        let kind = o.kind.unwrap_or(Kind::Track);
        let rows = stats::overdue(&h.entries, h.key(kind), now(), OVERDUE_MIN_PLAYS);
        let shown = &rows[..rows.len().min(10)];
        let names = self.names_of(kind, shown.iter().map(|r| r.0)).await?;
        let lines = shown.iter().map(|(k, factor, last, gap)| {
            format!("**{}**: last played {}, used to be {}, **{factor:.0}×** overdue", names[k], ago(*last), every(*gap))
        });
        let e = embed(format!("Overdue {}", kind_word(kind)), numbered(lines));
        Ok(footer(e, format!("At least {OVERDUE_MIN_PLAYS} plays · overdue = plays missed at your usual pace since the first one")))
    }

    pub(crate) async fn comebacks(&self, h: &Hist, o: &Opts) -> Result<EmbedBuilder> {
        let kind = o.kind.unwrap_or(Kind::Artist);
        let (from, label) = since(o, Some(30), "month");
        if from == i64::MIN {
            return Ok(embed(String::new(), "Pick a period to compare, e.g. `,comebacks month` or `,comebacks 90d`.".into()));
        }
        let rows = stats::comebacks(&h.entries, h.key(kind), from, now());
        let shown = &rows[..rows.len().min(15)];
        let names = self.names_of(kind, shown.iter().map(|r| r.0)).await?;
        let lines = shown.iter().map(|(k, n, last)| format!("**{}**: {n} plays, before that last played {}", names[k], date(*last)));
        let e = embed(format!("{} you came back to ({label})", capitalized(kind_word(kind))), numbered(lines));
        Ok(footer(e, format!("Not played in the {label} before this one, but earlier · {} in total", rows.len())))
    }

    /// This date in earlier years, and songs from your top 300 released on it.
    pub(crate) async fn on_this_day(&self, h: &Hist) -> Result<EmbedBuilder> {
        let today = h.today();
        let (this_year, month, day) = stats::civil(today);
        let first_year = h.entries.first().map_or(this_year, |e| stats::civil(h.tz.day(e.ts)).0);
        let first_artist = stats::first_plays(&h.entries, h.key(Kind::Artist));
        let mut sections = Vec::new();
        for (year, d) in stats::same_days(today, first_year) {
            let slice = h.day(d);
            let Some(&(track, plays)) = stats::top(slice, |e| Some(e.track)).first() else { continue };
            let (start, end) = (h.tz.start(d), h.tz.start(d + 1));
            let new: Vec<i32> = stats::top(slice, h.key(Kind::Artist))
                .into_iter()
                .filter(|(a, _)| first_artist.get(a).is_some_and(|t| (start..end).contains(t)))
                .map(|r| r.0)
                .take(3)
                .collect();
            let tracks = self.names_of(Kind::Track, [track]).await?;
            let mut text = format!("**{year}** · {} plays ({} years ago)\nTop: {} ({plays})", slice.len(), this_year - year, tracks[&track]);
            if !new.is_empty() {
                let artists = self.names_of(Kind::Artist, new.iter().copied()).await?;
                text.push_str(&format!("\nFirst listen: {}", new.iter().map(|a| artists[a].as_str()).collect::<Vec<_>>().join(", ")));
            }
            sections.push(text);
            if sections.len() == 6 {
                break;
            }
        }
        let top: Vec<(i32, u32)> = stats::top(&h.entries, |e| Some(e.track)).into_iter().take(300).collect();
        // Only the release section needs ListenBrainz; when it's down the years above still show.
        let facts = self.track_facts(&top.iter().map(|t| t.0).collect::<Vec<_>>()).await.unwrap_or_else(|e| {
            tracing::warn!(error = ?e, "release dates for on this day failed");
            HashMap::new()
        });
        let md = (month * 100 + day) as i32;
        let born: Vec<(i32, u32, i32)> = top
            .iter()
            .filter_map(|&(t, n)| {
                let r = facts.get(&t)?.released?;
                (r % 10_000 == md && i64::from(r / 10_000) < this_year).then_some((t, n, r / 10_000))
            })
            .take(5)
            .collect();
        if !born.is_empty() {
            let names = self.names_of(Kind::Track, born.iter().map(|b| b.0)).await?;
            let lines = born.iter().map(|(t, n, y)| format!("**{}**: out {} years ago today ({y}), {n} plays", names[t], this_year - i64::from(*y)));
            sections.push(format!("**Released on this day**\n{}", lines.collect::<Vec<_>>().join("\n")));
        }
        if sections.is_empty() {
            sections.push("Nothing on this day in earlier years yet.".into());
        }
        let e = embed(format!("On this day: {day} {}", MONTHS[month as usize - 1]), sections.join("\n\n"));
        Ok(footer(e, h.tz_note()))
    }

    /// Alistral's "all time changes": what climbed your all-time top during the period.
    pub(crate) async fn movers(&self, h: &Hist, o: &Opts) -> Result<EmbedBuilder> {
        let kind = o.kind.unwrap_or(Kind::Artist);
        let (from, label) = since(o, Some(30), "month");
        if from == i64::MIN {
            return Ok(embed(String::new(), "Pick a period to compare, e.g. `,movers month` or `,movers year`.".into()));
        }
        let rows = stats::movers(&h.entries, h.key(kind), from, 50);
        let shown = &rows[..rows.len().min(15)];
        let names = self.names_of(kind, shown.iter().map(|r| r.0)).await?;
        let lines = shown.iter().map(|(k, rank, old, n)| match old {
            Some(old) => format!("**{}**: #{old} → **#{rank}** (+{}), {n} new plays", names[k], old - rank),
            None => format!("**{}**: new at **#{rank}**, {n} plays", names[k]),
        });
        let e = embed(format!("Climbers in your all-time top {} ({label})", kind_word(kind)), numbered(lines));
        Ok(footer(e, format!("Your all-time top 50 now, against where they stood a {label} ago")))
    }

    /// Plays per year (all time) or per month of a year, optionally for one artist, as a bar chart.
    pub(crate) async fn growth(&self, ctx: &Ctx, user: &User, h: &Hist, arg: &str, note: Option<&str>) -> Result<Out> {
        let this = stats::civil(h.today()).0;
        let (year, artist_name) = year_and_artist(arg, this);
        let (name, artist) = if artist_name.is_empty() {
            (None, None)
        } else {
            match self.find_in(user, Kind::Artist, artist_name).await? {
                Ok((name, id)) => (Some(name), Some(id)),
                Err(e) => return Ok(e.into()),
            }
        };
        let key = h.key(Kind::Artist);
        let entries: Vec<Entry> = h.entries.iter().filter(|e| artist.is_none_or(|a| key(e) == Some(a))).copied().collect();
        let civil = |t: i64| stats::civil(h.tz.day(t));
        let (kicker, bars) = match year {
            Some(y) => {
                let (from, to) = (h.tz.start(stats::days_from_civil(y, 1, 1)), h.tz.start(stats::days_from_civil(y + 1, 1, 1)));
                let per = stats::buckets(stats::range(&entries, from, to), |t| i64::from(civil(t).1));
                ("Plays per month", (1..=12).map(|m| (MONTHS[m as usize - 1][..3].to_owned(), u64::from(per.get(&m).copied().unwrap_or(0)))).collect::<Vec<_>>())
            }
            None => {
                let per = stats::buckets(&entries, |t| civil(t).0);
                let first = per.keys().next().copied().unwrap_or(this);
                ("Plays per year", (first..=this).map(|y| (y.to_string(), u64::from(per.get(&y).copied().unwrap_or(0)))).collect())
            }
        };
        let total: u64 = bars.iter().map(|b| b.1).sum();
        if total == 0 {
            return Ok(reply("No listens to draw.").into());
        }
        let mut label = vec![year.map_or("all time".to_owned(), |y| y.to_string())];
        label.extend(name);
        label.push(format!("{total} plays"));
        let (title, label) = (ctx.name.clone(), label.join(" · "));
        let png = tokio::task::spawn_blocking(move || crate::graph::bars(&Heading { kicker, title: &title, label: &label }, &bars)).await??;
        Ok(picture("growth.png", png, h.tz_note(), note))
    }

    /// The days you played one song the most, and artists you know for one song.
    pub(crate) async fn obsessions(&self, h: &Hist) -> Result<EmbedBuilder> {
        let days_ = stats::binges(&h.entries, |e| Some(e.track), |t| h.tz.day(t));
        let days_: Vec<_> = days_.into_iter().filter(|r| r.2 >= 3).take(10).collect();
        let names = self.names_of(Kind::Track, days_.iter().map(|r| r.0)).await?;
        let lines = days_.iter().map(|(t, d, n)| format!("**{}**: {n} plays on {}", names[t], day_label(*d)));
        let mut desc = format!("**Biggest days with one song**\n{}", numbered(lines));
        let one = stats::one_track_artists(&h.entries, |t| h.artist.get(&t).copied(), 20, 0.8);
        let one = &one[..one.len().min(8)];
        if !one.is_empty() {
            let artists = self.names_of(Kind::Artist, one.iter().map(|r| r.0)).await?;
            let titles = self.db().name_parts(Kind::Track, &one.iter().map(|r| r.1).collect::<Vec<_>>()).await?;
            let lines = one.iter().map(|(a, t, n, total)| {
                let title = titles.get(t).map_or("?".to_owned(), |p| escape(&p.1));
                format!("**{}**: {:.0}% of {total} plays are *{title}*", artists[a], percent(f64::from(*n), f64::from(*total)))
            });
            desc.push_str(&format!("\n\n**One-song artists**\n{}", numbered(lines)));
        }
        Ok(footer(embed("Obsessions".into(), desc), format!("One-song artists: 20+ plays, 80%+ on one track\n{}", h.tz_note())))
    }

    /// Alistral's underrated radio as a list: your tracks scored by your rank and your share of all LB listens.
    pub(crate) async fn hidden(&self, h: &Hist, o: &Opts) -> Result<EmbedBuilder> {
        let (from, label) = since(o, None, "all time");
        let top: Vec<(i32, u32)> = stats::top(stats::range(&h.entries, from, i64::MAX), |e| Some(e.track)).into_iter().take(200).collect();
        let all_time: HashMap<i32, u32> = stats::top(&h.entries, |e| Some(e.track)).into_iter().collect();
        let facts = self.track_facts(&top.iter().map(|t| t.0).collect::<Vec<_>>()).await?;
        let mbids: Vec<&str> = top.iter().filter_map(|t| facts.get(&t.0)?.mbid.as_deref()).collect();
        let pop = self.popularity("recording", &mbids).await?;
        let mut scored: Vec<(f64, i32, i64, i64)> = Vec::new();
        for (rank, (t, _)) in top.iter().enumerate() {
            let Some(&(_, listens)) = facts.get(t).and_then(|f| pop.get(f.mbid.as_deref()?)) else { continue };
            let mine = i64::from(all_time[t]);
            scored.push((stats::underrated(rank, mine, listens), *t, mine, listens.max(mine)));
        }
        scored.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
        let shown = &scored[..scored.len().min(10)];
        let names = self.names_of(Kind::Track, shown.iter().map(|r| r.1)).await?;
        let lines = shown.iter().map(|(_, t, mine, all)| {
            format!("**{}**: {mine} of {all} ListenBrainz listens are yours ({:.0}%)", names[t], percent(*mine as f64, *all as f64))
        });
        let e = embed(format!("Hidden gems ({label})"), numbered(lines));
        let missing = top.len() - scored.len();
        let mut text = "Alistral's underrated score: your rank plus your share of everyone's listens".to_owned();
        if missing > 0 {
            text.push_str(&format!("\n{missing} of your top {} tracks have no ListenBrainz stats", top.len()));
        }
        Ok(footer(e, text))
    }

    /// A 0-100 score of how little known your top 50 artists are, and the ranking among members who ran it.
    pub(crate) async fn obscurity(&self, ctx: &Ctx, user: &User, h: &Hist, o: &Opts) -> Result<EmbedBuilder> {
        let (from, label) = since(o, None, "all time");
        let top: Vec<(i32, u32)> = stats::top(stats::range(&h.entries, from, i64::MAX), h.key(Kind::Artist)).into_iter().take(50).collect();
        let mbids = self.artist_mbids(&top.iter().map(|t| t.0).collect::<Vec<_>>()).await?;
        let pop = self.popularity("artist", &mbids.values().map(String::as_str).collect::<Vec<_>>()).await?;
        let mut rows: Vec<(i32, u32, i64)> = top.iter().filter_map(|&(a, n)| Some((a, n, pop.get(mbids.get(&a)?)?.0))).collect();
        if rows.is_empty() {
            return Ok(embed(String::new(), format!("None of your artists ({label}) have ListenBrainz stats yet.")));
        }
        let score = stats::underground(&rows.iter().map(|r| (r.1, r.2)).collect::<Vec<_>>());
        // Only all-time scores are ranked, so everyone is compared over the same span.
        if from == i64::MIN {
            self.db().set_obscurity(user.id, &user.lb_name, score.round() as i16).await?;
        }
        rows.sort_by_key(|r| (r.2, r.0));
        // Up to 3 from each end; with few artists each one shows once (a single artist shows in both).
        let n = (rows.len() / 2).clamp(1, 3);
        let ends: Vec<i32> = rows.iter().take(n).chain(rows.iter().rev().take(n)).map(|r| r.0).collect();
        let names = self.names_of(Kind::Artist, ends).await?;
        let list = |it: &mut dyn Iterator<Item = &(i32, u32, i64)>| it.map(|r| format!("{} ({} listeners)", names[&r.0], r.2)).collect::<Vec<_>>().join(", ");
        let mut desc = format!(
            "**{score:.0}**/100 underground ({label})\n0 = everybody knows them, 100 = nobody does.\n\n**Deepest**: {}\n**Best known**: {}",
            list(&mut rows.iter().take(n)),
            list(&mut rows.iter().rev().take(n))
        );
        if let Some(g) = ctx.guild {
            let ranking = self.db().member_obscurity(g).await?;
            if ranking.len() > 1 {
                let lines = ranking.iter().take(10).map(|(id, name, s)| {
                    if *id == user.id { format!("**{}: {s}**", escape(name)) } else { format!("{}: {s}", escape(name)) }
                });
                desc.push_str(&format!("\n\n**Most underground here**\n{}", numbered(lines)));
            }
        }
        let e = embed("How underground you are".into(), desc);
        Ok(footer(e, "Your top 50 artists by ListenBrainz listeners, weighted by your plays · the server list uses all-time scores"))
    }

    pub(crate) async fn server_iceberg(&self, guild: i64, note: Option<&str>) -> Result<Out> {
        let top = self.db().server_top_artists(guild, 50).await?;
        if top.is_empty() {
            return Ok(reply("No one in this server has listens yet.").into());
        }
        let name = self.guild_names.read().unwrap().get(&guild).cloned().unwrap_or_else(|| "This server".into());
        self.iceberg_picture(&name, "all time", &top.iter().map(|t| t.0).collect::<Vec<_>>(), "The server's top 50 artists", note).await
    }

    /// Plays by the decade your most played tracks came out, as a bar chart.
    pub(crate) async fn decades(&self, ctx: &Ctx, h: &Hist, o: &Opts, note: Option<&str>) -> Result<Out> {
        let (from, label) = since(o, None, "all time");
        let slice = stats::range(&h.entries, from, i64::MAX);
        let top: Vec<(i32, u32)> = stats::top(slice, |e| Some(e.track)).into_iter().take(500).collect();
        let facts = self.track_facts(&top.iter().map(|t| t.0).collect::<Vec<_>>()).await?;
        let rows: Vec<(i32, u32)> = top.iter().filter_map(|&(t, n)| facts.get(&t)?.released.filter(|r| *r > 0).map(|r| (r / 10_000, n))).collect();
        let (by_decade, Some(mean)) = stats::decades(&rows) else {
            return Ok(reply(format!("No release dates found for your tracks ({label}) yet.")).into());
        };
        let (first, last) = (*by_decade.keys().next().unwrap_or(&0), *by_decade.keys().last().unwrap_or(&0));
        let bars: Vec<(String, u64)> = (first..=last).step_by(10).map(|d| (format!("{d}s"), u64::from(by_decade.get(&d).copied().unwrap_or(0)))).collect();
        let covered: u32 = rows.iter().map(|r| r.1).sum();
        let (title, label_text) = (ctx.name.clone(), format!("{label} · average year {mean:.0}"));
        let png = tokio::task::spawn_blocking(move || {
            crate::graph::bars(&Heading { kicker: "When your music came out", title: &title, label: &label_text }, &bars)
        })
        .await??;
        let text = format!(
            "First releases of {} of your most played tracks, {:.0}% of your plays ({label})",
            rows.len(),
            percent(f64::from(covered), slice.len() as f64)
        );
        Ok(picture("decades.png", png, text, note))
    }

    pub(crate) async fn listening_time(&self, h: &Hist, o: &Opts) -> Result<EmbedBuilder> {
        let (from, label) = since(o, Some(30), "month");
        let slice = stats::range(&h.entries, from, i64::MAX);
        if slice.is_empty() {
            return Ok(embed(String::new(), format!("No listens ({label}).")));
        }
        let tracks: Vec<i32> = stats::top(slice, |e| Some(e.track)).into_iter().map(|t| t.0).collect();
        let facts = self.track_facts(&tracks).await?;
        let len = |t: i32| facts.get(&t).and_then(|f| f.length_ms).filter(|l| *l > 0).map(i64::from);
        let Some((avg, known)) = stats::average_length(slice, len) else {
            return Ok(embed(String::new(), format!("No track lengths found for your listens ({label}) yet.")));
        };
        let play = |e: &Entry| len(e.track).unwrap_or(avg as i64);
        let total: i64 = slice.iter().map(play).sum();
        let span = (now() - from.max(slice[0].ts)) as f64 / DAY as f64;
        let mut desc = format!("**{}** ({label}) · {} a day on average", duration(total), duration((total as f64 / span.max(1.0)) as i64));
        for (kind, title) in [(Kind::Artist, "Artists"), (Kind::Track, "Tracks")] {
            let by = stats::time_by(slice, h.key(kind), play);
            let shown = &by[..by.len().min(5)];
            let names = self.names_of(kind, shown.iter().map(|r| r.0)).await?;
            desc.push_str(&format!("\n\n**{title} by time**\n{}", numbered(shown.iter().map(|(k, ms)| format!("{}: {}", names[k], duration(*ms))))));
        }
        let secs = (avg / 1000.0) as i64;
        let text = format!("{:.0}% of plays have a known length; the rest count as your average, {}:{:02}", known * 100.0, secs / 60, secs % 60);
        Ok(footer(embed("Listening time".into(), desc), text))
    }

    /// Plays by weekday and hour in the user's time zone, as a heatmap.
    pub(crate) async fn clock(&self, ctx: &Ctx, h: &Hist, o: &Opts, note: Option<&str>) -> Result<Out> {
        let (from, label) = since(o, None, "all time");
        let slice = stats::range(&h.entries, from, i64::MAX);
        if slice.is_empty() {
            return Ok(reply(format!("No listens ({label}).")).into());
        }
        let grid = stats::clock(slice, |t| h.tz.local(t));
        let (day, hour) = (0..7).flat_map(|d| (0..24).map(move |hr| (d, hr))).max_by_key(|&(d, hr)| (grid[d][hr], std::cmp::Reverse((d, hr)))).unwrap_or((0, 0));
        let (title, label) = (ctx.name.clone(), format!("{label} · {} · busiest: {} {hour:02}:00", h.tz.name(), WEEKDAYS[day]));
        let png = tokio::task::spawn_blocking(move || crate::graph::heatmap(&Heading { kicker: "When you listen", title: &title, label: &label }, &grid)).await??;
        Ok(picture("clock.png", png, h.tz_note(), note))
    }

    /// Your most played tracks ListenBrainz can't link to a MusicBrainz recording, which the id-based stats miss.
    pub(crate) async fn unlinked(&self, h: &Hist) -> Result<EmbedBuilder> {
        let top: Vec<(i32, u32)> = stats::top(&h.entries, |e| Some(e.track)).into_iter().take(100).collect();
        let facts = self.track_facts(&top.iter().map(|t| t.0).collect::<Vec<_>>()).await?;
        let missing: Vec<(i32, u32)> = top.iter().copied().filter(|(t, _)| facts.get(t).is_some_and(|f| f.mbid.is_none())).collect();
        if missing.is_empty() {
            return Ok(embed(String::new(), format!("All of your top {} tracks are linked to MusicBrainz.", top.len())));
        }
        let shown = &missing[..missing.len().min(15)];
        let names = self.names_of(Kind::Track, shown.iter().map(|r| r.0)).await?;
        let lines = shown.iter().map(|(t, n)| format!("**{}**: {n} plays", names[t]));
        let e = embed(format!("Unlinked tracks: {} of your top {}", missing.len(), top.len()), numbered(lines));
        Ok(footer(e, "ListenBrainz can't match these to MusicBrainz, so hidden gems, decades and listening time skip them. Link them at listenbrainz.org/settings/link-listens"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wording() {
        assert_eq!(every(3600), "every hour or so");
        assert_eq!(every(5 * 3600), "every 5 hours");
        assert_eq!(every(3 * DAY), "every 3 days");
        assert_eq!(duration(59 * 60_000), "59m");
        assert_eq!(duration((37 * 60 + 12) * 60_000), "37h 12m");
        assert_eq!(duration((4 * 24 + 2) * 3_600_000), "4 days 2h");
        assert_eq!(percent(1.0, 0.0), 0.0);
        assert_eq!(year_and_artist("The 1975", 2026), (None, "The 1975"));
        assert_eq!(year_and_artist("311", 2026), (None, "311"));
        assert_eq!(year_and_artist("All Time Low", 2026), (None, "All Time Low"));
        assert_eq!(year_and_artist(" 2025  Radiohead ", 2026), (Some(2025), "Radiohead"));
        assert_eq!(year_and_artist("2025", 2026), (Some(2025), ""));
        assert_eq!(year_and_artist("2030 Radiohead", 2026), (None, "2030 Radiohead"), "a future year is part of the name");
        assert_eq!(year_and_artist("", 2026), (None, ""));
    }
}
