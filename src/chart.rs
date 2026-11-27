//! fmbot's image charts (`,c` albums, `,ac` artists, `,tc` tracks), ranked from the stored history. Covers come from
//! ListenBrainz's metadata and the Cover Art Archive; the grid is an SVG rendered to PNG. The Edit button under a
//! chart opens fmbot's settings form and redraws the chart in place.

use std::collections::HashMap;

use anyhow::Result;
use futures_util::{StreamExt as _, stream};
use serde_json::Value;
use twilight_model::{
    application::{
        command::{Command, CommandType},
        interaction::application_command::{CommandDataOption, CommandOptionValue},
    },
    channel::message::{
        Component, Embed,
        component::{
            ActionRow, Button, ButtonStyle, CheckboxGroup, CheckboxGroupOption, Label, SelectMenu, SelectMenuOption, SelectMenuType,
            TextInput, TextInputStyle,
        },
    },
    http::{attachment::Attachment, interaction::InteractionResponse},
};
use twilight_util::builder::{
    command::{BooleanBuilder, CommandBuilder, IntegerBuilder, StringBuilder, SubCommandBuilder, UserBuilder},
    embed::{EmbedBuilder, EmbedFooterBuilder, ImageSource},
    interaction_response::ModalBuilder,
};

use crate::{
    bot::{Bot, COLOR, Ctx, Out, art_keys, cache_get, escape, login_first, reply},
    db::Kind,
    draw::{FALLBACK, SANS, xml},
    hist::{capitalized, kind_word, mention, now, with_note},
    history::Entry,
    lb::{Prio, album_key, cover_url, thumb},
    stats::{self, DAY},
};

/// fmbot's period choices; `None` = all time. Their values in forms and slash choices are the day count, 0 for all time.
const PERIODS: [(&str, Option<i64>); 6] =
    [("Weekly", Some(7)), ("Monthly", Some(30)), ("Quarterly", Some(90)), ("Half-yearly", Some(182)), ("Yearly", Some(365)), ("All time", None)];
/// How many of the top entries a chart looks through when filters or skipping can drop some. Each 50 costs 2–3 LB requests.
// shortcut: albums below the user's top 200 of the period are never reached; raise it if filtered charts come up short.
const SCAN: usize = 200;
/// MusicBrainz takes 1 request/s and the bot's queue turns away anything more than 2 s ahead, so a chart fetches at most
/// this many artist photos it hasn't cached; the rest show the artist's most played album until a later chart caches them.
const NEW_PHOTOS: usize = 3;
/// fmbot's limit: 4 charts per user in 40 s. A filtered 10x10 costs ~15 LB requests of the 30 per 10 s all commands share.
const PER_WINDOW: u8 = 4;
const WINDOW: std::time::Duration = std::time::Duration::from_secs(40);

/// What to draw. Small enough to fit in the Edit button's id (see `button_id`).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Chart {
    pub kind: Kind,
    pub cols: u32,
    pub rows: u32,
    /// `None` = all time.
    pub days: Option<i64>,
    pub titles: bool,
    pub plays: bool,
    pub skip: bool,
    pub rainbow: bool,
    /// Albums only, like `year` and `decade`.
    pub no_singles: bool,
    pub year: Option<i64>,
    pub decade: Option<i64>,
    /// Only this artist (an `artists` id).
    pub artist: Option<i32>,
    /// Discord id of the person whose chart it is; `None` = whoever ran it.
    pub target: Option<i64>,
}

impl Chart {
    fn new(kind: Kind) -> Self {
        Self {
            kind,
            cols: 3,
            rows: 3,
            days: Some(7),
            titles: true,
            plays: false,
            skip: false,
            rainbow: false,
            no_singles: false,
            year: None,
            decade: None,
            artist: None,
            target: None,
        }
    }

    /// fmbot's default period: all time when filtering on a release year or an artist, else a week.
    fn default_days(&self, artist: bool) -> Option<i64> {
        (self.year.is_none() && self.decade.is_none() && !artist).then_some(7)
    }

    /// Album filters that need each album's release group.
    fn filtered(&self) -> bool {
        self.kind == Kind::Album && (self.year.is_some() || self.decade.is_some() || self.no_singles)
    }

    /// Why this chart can't be drawn, as a reply.
    fn invalid(&self) -> Option<&'static str> {
        if !(1..=20).contains(&self.cols) || !(1..=20).contains(&self.rows) || self.cols * self.rows > 100 {
            return Some("The size must look like `3x3` or `5x4`: up to 20 on a side and 100 images in all.");
        }
        let years = 1900..=this_year();
        if self.year.is_some_and(|y| !years.contains(&y)) || self.decade.is_some_and(|d| !years.contains(&d)) {
            return Some("The release filter must be a year like `2023` or a decade like `90s`.");
        }
        None
    }
}

fn this_year() -> i64 {
    stats::civil(now().div_euclid(DAY)).0
}

/// `3x3`, `20x5`: fmbot's `WxH` with up to 3 digits a side; the size limits are checked by `Chart::invalid`.
fn size(word: &str) -> Option<(u32, u32)> {
    let (a, b) = word.split_once('x')?;
    let side = |s: &str| s.parse().ok().filter(|_| (1..=3).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit()) && !s.starts_with('0'));
    Some((side(a)?, side(b)?))
}

fn year(word: &str) -> Option<i64> {
    word.parse().ok().filter(|y| (1900..=this_year()).contains(y))
}

/// fmbot's decades: `80s`, `1980`, `1980s`, `80` (below 30 means the 2000s).
fn decade(word: &str) -> Option<i64> {
    let n: i64 = word.trim_end_matches(['s', 'S']).parse().ok()?;
    let n = (if n < 100 { n + if n < 30 { 2000 } else { 1900 } } else { n }) / 10 * 10;
    (1900..=this_year()).contains(&n).then_some(n)
}

/// fmbot's text options in any order; the words left over name an artist to filter on. `aoty`/`aotd` (which their
/// aliases put first) filter on a bare year/decade from the text, or this one.
pub(crate) fn parse(kind: Kind, arg: &str) -> (Chart, Option<String>) {
    let mut c = Chart::new(kind);
    let (mut days, mut rest, mut aoty, mut aotd) = (None, Vec::new(), false, false);
    for w in arg.split_whitespace() {
        let l = w.to_lowercase();
        match l.as_str() {
            "playcounts" | "plays" | "pc" => c.plays = true,
            "notitles" | "nt" => c.titles = false,
            "skipemptyimages" | "skipemptyalbums" | "skipalbums" | "skip" | "s" => c.skip = true,
            "rainbow" | "pride" => c.rainbow = true,
            "ns" | "nosingles" | "hidesingles" | "filtersingles" => c.no_singles = true,
            "aoty" => aoty = true,
            "aotd" => aotd = true,
            _ => {
                if let Some((cols, rows)) = size(&l) {
                    (c.cols, c.rows) = (cols, rows);
                } else if let Some(y) = l.strip_prefix("r:").or_else(|| l.strip_prefix("released:")).and_then(year) {
                    c.year = Some(y);
                } else if let Some(d) = l.strip_prefix("d:").or_else(|| l.strip_prefix("decade:")).and_then(decade) {
                    c.decade = Some(d);
                } else if let Some((id, "")) = mention(w) {
                    c.target = Some(id);
                } else if let Some(p) = days.is_none().then(|| stats::period(&l)).flatten() {
                    days = Some(p.0);
                } else {
                    rest.push(w);
                }
            }
        }
    }
    // A bare year counts only after aoty/aotd; elsewhere "1975" is an artist.
    let mut take = |f: fn(&str) -> Option<i64>| rest.iter().position(|w| f(w).is_some()).and_then(|i| f(rest.remove(i)));
    if aoty && c.year.is_none() {
        c.year = Some(take(year).unwrap_or_else(this_year));
    }
    if aotd && c.decade.is_none() {
        c.decade = Some(take(decade).unwrap_or(this_year() / 10 * 10));
    }
    let artist = (!rest.is_empty()).then(|| rest.join(" "));
    c.days = days.unwrap_or_else(|| c.default_days(artist.is_some()));
    (c, artist)
}

/// A `/chart` subcommand's options: (chart, artist to filter on, private).
pub(crate) fn from_slash(sub: &str, options: &[CommandDataOption]) -> (Chart, Option<String>, bool) {
    let kind = match sub {
        "artists" => Kind::Artist,
        "tracks" => Kind::Track,
        _ => Kind::Album,
    };
    let mut c = Chart::new(kind);
    let (mut days, mut artist, mut private) = (None, None, false);
    for o in options {
        match (o.name.as_str(), &o.value) {
            ("time-period", CommandOptionValue::String(p)) => days = nonzero(p),
            // An unreadable size becomes 0x0, which `invalid` explains.
            ("size", CommandOptionValue::String(s)) => (c.cols, c.rows) = size(&s.trim().to_lowercase()).unwrap_or((0, 0)),
            ("titles", CommandOptionValue::String(t)) => (c.titles, c.plays) = (t != "nt", t == "pc"),
            ("artist", CommandOptionValue::String(a)) if !a.trim().is_empty() => artist = Some(a.trim().to_owned()),
            ("released", CommandOptionValue::Integer(y)) => c.year = Some(*y),
            ("decade", CommandOptionValue::Integer(d)) => c.decade = Some(*d),
            ("skip", CommandOptionValue::Boolean(b)) => c.skip = *b,
            ("rainbow", CommandOptionValue::Boolean(b)) => c.rainbow = *b,
            ("hide-singles", CommandOptionValue::Boolean(b)) => c.no_singles = *b,
            ("private", CommandOptionValue::Boolean(b)) => private = *b,
            ("user", CommandOptionValue::User(id)) => c.target = Some(id.get() as i64),
            _ => {}
        }
    }
    c.days = days.unwrap_or_else(|| c.default_days(artist.is_some()));
    (c, artist, private)
}

pub(crate) fn slash_command() -> Command {
    let choices = PERIODS.map(|(label, d)| (label, d.unwrap_or(0).to_string()));
    let common = |sub: SubCommandBuilder, what: &str| {
        sub.option(StringBuilder::new("time-period", "Time period (default: weekly)").required(false).choices(choices.clone()))
            .option(StringBuilder::new("size", "Chart size, e.g. 3x3 or 5x4 (up to 100 images)").required(false))
            .option(StringBuilder::new("titles", "Title display setting").required(false).choices([
                ("Titles", "titles"),
                ("No titles", "nt"),
                ("Titles and playcounts", "pc"),
            ]))
            .option(BooleanBuilder::new("skip", format!("Skip {what} without an image")).required(false))
            .option(UserBuilder::new("user", "The user to show (default: you)").required(false))
            .option(BooleanBuilder::new("rainbow", "Sort by cover color").required(false))
            .option(BooleanBuilder::new("private", "Only show the response to you").required(false))
    };
    let artist = || StringBuilder::new("artist", "Only this artist").required(false);
    let decades = (195..=this_year() / 10).map(|d| (format!("{}s", d * 10), d * 10));
    let albums = SubCommandBuilder::new("albums", "Generates an album image chart")
        .option(artist())
        .option(IntegerBuilder::new("released", "Albums released in this year").min_value(1900).max_value(this_year()).required(false))
        .option(IntegerBuilder::new("decade", "Albums released in this decade").choices(decades).required(false))
        .option(BooleanBuilder::new("hide-singles", "Hide singles").required(false));
    CommandBuilder::new("chart", "Image charts of your top albums, artists or tracks", CommandType::ChatInput)
        .option(common(albums, "albums"))
        .option(common(SubCommandBuilder::new("artists", "Generates an artist image chart"), "artists"))
        .option(common(SubCommandBuilder::new("tracks", "Generates a track image chart").option(artist()), "tracks"))
        .build()
}

fn kind_code(kind: Kind) -> &'static str {
    match kind {
        Kind::Album => "a",
        Kind::Artist => "r",
        Kind::Track => "t",
    }
}

fn kind_of(code: &str) -> Option<Kind> {
    Some(match code {
        "a" => Kind::Album,
        "r" => Kind::Artist,
        "t" => Kind::Track,
        _ => return None,
    })
}

/// `ch:owner:target:kind:WxH:days:flags:year:decade:artist`, 0 for none: under 100 characters, Discord's limit.
fn button_id(owner: i64, c: &Chart) -> String {
    let flags: u8 = [c.titles, c.plays, c.skip, c.rainbow, c.no_singles].iter().enumerate().map(|(i, &on)| u8::from(on) << i).sum();
    format!(
        "ch:{owner}:{}:{}:{}x{}:{}:{flags}:{}:{}:{}",
        c.target.unwrap_or(0),
        kind_code(c.kind),
        c.cols,
        c.rows,
        c.days.unwrap_or(0),
        c.year.unwrap_or(0),
        c.decade.unwrap_or(0),
        c.artist.unwrap_or(0)
    )
}

/// A number in a button or form id, 0 = none (a period's 0 = all time).
fn nonzero(s: &str) -> Option<Option<i64>> {
    s.parse::<i64>().ok().map(|n| (n != 0).then_some(n))
}

/// (owner, chart) from an Edit button id.
pub(crate) fn from_button(id: &str) -> Option<(i64, Chart)> {
    let parts: Vec<&str> = id.strip_prefix("ch:")?.split(':').collect();
    let [owner, target, kind, size, days, flags, year, decade, artist] = parts[..] else { return None };
    let flags: u8 = flags.parse().ok()?;
    let on = |bit: u8| flags >> bit & 1 == 1;
    let (cols, rows) = size.split_once('x')?;
    let chart = Chart {
        kind: kind_of(kind)?,
        cols: cols.parse().ok()?,
        rows: rows.parse().ok()?,
        days: nonzero(days)?,
        titles: on(0),
        plays: on(1),
        skip: on(2),
        rainbow: on(3),
        no_singles: on(4),
        year: nonzero(year)?,
        decade: nonzero(decade)?,
        artist: nonzero(artist)?.map(|a| a as i32),
        target: nonzero(target)?,
    };
    Some((owner.parse().ok()?, chart))
}

pub(crate) fn edit_button(owner: i64, c: &Chart) -> Component {
    let button = Button {
        custom_id: Some(button_id(owner, c)),
        disabled: false,
        emoji: None,
        id: None,
        label: Some("Edit".to_owned()),
        sku_id: None,
        style: ButtonStyle::Secondary,
        url: None,
    };
    Component::ActionRow(ActionRow { components: vec![Component::Button(button)], id: None })
}

/// fmbot's "Edit chart settings" form, filled in with the chart's settings. Its id keeps what the form doesn't show.
pub(crate) fn modal(owner: i64, c: &Chart) -> InteractionResponse {
    let label = |text: &str, component| Component::Label(Label { component: Box::new(component), description: None, id: None, label: text.to_owned() });
    #[allow(deprecated)] // `label` now lives on the Label component
    let text = |id: &str, value: String, required: bool, placeholder: &str| {
        Component::TextInput(TextInput {
            custom_id: id.to_owned(),
            id: None,
            label: None,
            max_length: Some(10),
            min_length: None,
            placeholder: Some(placeholder.to_owned()),
            required: Some(required),
            style: TextInputStyle::Short,
            value: (!value.is_empty()).then_some(value),
        })
    };
    let mut periods: Vec<(String, Option<i64>)> = PERIODS.iter().map(|(l, d)| ((*l).to_owned(), *d)).collect();
    if !periods.iter().any(|p| p.1 == c.days) {
        periods.push((period_name(c.days), c.days));
    }
    let period = Component::SelectMenu(SelectMenu {
        channel_types: None,
        custom_id: "period".to_owned(),
        default_values: None,
        disabled: false,
        id: None,
        kind: SelectMenuType::Text,
        max_values: Some(1),
        min_values: Some(1),
        options: Some(
            periods
                .into_iter()
                .map(|(label, d)| SelectMenuOption { default: d == c.days, description: None, emoji: None, label, value: d.unwrap_or(0).to_string() })
                .collect(),
        ),
        placeholder: None,
        required: Some(true),
    });
    let skip = format!("Skip {} without image", kind_word(c.kind));
    let mut flags = vec![("titles", "Show titles", c.titles), ("plays", "Show playcounts", c.plays), ("skip", skip.as_str(), c.skip), ("rainbow", "Rainbow sort", c.rainbow)];
    if c.kind == Kind::Album {
        flags.push(("singles", "Hide singles", c.no_singles));
    }
    let options = Component::CheckboxGroup(CheckboxGroup {
        custom_id: "options".to_owned(),
        id: None,
        max_values: Some(flags.len() as u8),
        min_values: Some(0),
        options: flags
            .iter()
            .map(|(value, label, on)| CheckboxGroupOption { default: Some(*on), description: None, label: (*label).to_owned(), value: (*value).to_owned() })
            .collect(),
        required: Some(false),
    });
    let mut components = vec![
        label("Size (e.g. 3x3)", text("size", format!("{}x{}", c.cols, c.rows), true, "3x3")),
        label("Time period", period),
        label("Options", options),
    ];
    if c.kind == Kind::Album {
        let released = c.year.map(|y| y.to_string()).or(c.decade.map(|d| format!("{d}s"))).unwrap_or_default();
        components.push(label("Released in", text("released", released, false, "A year like 2023 or a decade like 90s")));
    }
    let id = format!("chm:{owner}:{}:{}:{}", c.target.unwrap_or(0), kind_code(c.kind), c.artist.unwrap_or(0));
    ModalBuilder::new(id, "Edit chart settings", components).build()
}

/// (owner, chart) from a submitted Edit form; `value` reads a field by id. Unreadable input becomes a value
/// `Chart::invalid` explains (size 0x0, year 0).
pub(crate) fn from_modal(id: &str, value: impl Fn(&str) -> Vec<String>) -> Option<(i64, Chart)> {
    let parts: Vec<&str> = id.strip_prefix("chm:")?.split(':').collect();
    let [owner, target, kind, artist] = parts[..] else { return None };
    let mut c = Chart::new(kind_of(kind)?);
    c.target = nonzero(target)?;
    c.artist = nonzero(artist)?.map(|a| a as i32);
    let first = |id: &str| value(id).into_iter().next().unwrap_or_default().trim().to_lowercase();
    (c.cols, c.rows) = size(&first("size")).unwrap_or((0, 0));
    c.days = nonzero(&first("period")).unwrap_or(Some(7));
    let checked = value("options");
    let on = |v: &str| checked.iter().any(|o| o == v);
    (c.titles, c.plays, c.skip, c.rainbow, c.no_singles) = (on("titles"), on("plays"), on("skip"), on("rainbow"), on("singles"));
    let released = first("released");
    if !released.is_empty() {
        match year(&released) {
            Some(y) => c.year = Some(y),
            None => c.decade = Some(decade(&released).unwrap_or(0)),
        }
    }
    Some((owner.parse().ok()?, c))
}

fn period_name(days: Option<i64>) -> String {
    match days {
        None => "all-time".to_owned(),
        Some(7) => "weekly".to_owned(),
        Some(30) => "monthly".to_owned(),
        Some(90) => "quarterly".to_owned(),
        Some(182) => "half-yearly".to_owned(),
        Some(365) => "yearly".to_owned(),
        Some(n) => format!("{n}-day"),
    }
}

/// One square: name lines (the artist, then the album or track), plays and the cover as a `data:` URI.
pub(crate) struct Tile {
    pub lines: Vec<String>,
    pub plays: u32,
    pub image: Option<String>,
}

/// Side of one square: 300px like fmbot, less for big charts so the PNG stays well under Discord's 10 MB.
fn cell(images: u32) -> u32 {
    (2_400_000.0 / f64::from(images.max(1))).sqrt().min(300.0) as u32
}

impl Bot {
    /// Draws a chart, or explains why it can't; both come with the Edit button.
    pub(crate) async fn chart(&self, ctx: &Ctx, c: &Chart) -> Result<Out> {
        let with_edit = |embed: Embed| Out { embed, components: vec![edit_button(ctx.discord_id, c)], files: Vec::new() };
        if let Some(why) = c.invalid() {
            return Ok(with_edit(reply(why)));
        }
        let Some(me) = self.registered(ctx).await? else { return Ok(login_first().into()) };
        if !allow(&mut self.chart_uses.lock().unwrap(), ctx.discord_id, std::time::Instant::now()) {
            return Ok(with_edit(reply("That's a lot of charts at once. Try again in a bit.")));
        }
        let (user, name) = match c.target.filter(|t| *t != ctx.discord_id) {
            None => (me, ctx.name.clone()),
            Some(id) => {
                // Like `,taste`: only members of this server, since hidden (`/privacy`) users show up only in their own servers.
                let Some(guild) = ctx.guild else { return Ok(reply("Other people's charts only work in servers.").into()) };
                match self.db().user_by_discord(id).await? {
                    Some(u) if self.db().is_member(guild, u.id).await? => {
                        let name = u.lb_name.clone();
                        (u, name)
                    }
                    // The same answer either way, so it doesn't tell whether an outsider has an account.
                    _ => return Ok(reply("They haven't connected a ListenBrainz account in this server.").into()),
                }
            }
        };
        let note = (!user.indexed).then_some("This history is still downloading, so counts may be incomplete.");
        if let Err(e) = self.ingest.update(&user, Prio::Command).await {
            tracing::warn!(error = ?e, "update during chart failed");
        }
        let h = self.load(&user).await?;
        let from = c.days.map_or(i64::MIN, |d| now() - d * DAY);
        let slice: Vec<Entry> = stats::range(&h.entries, from, i64::MAX)
            .iter()
            .filter(|e| c.artist.is_none_or(|a| h.artist.get(&e.track) == Some(&a)))
            .copied()
            .collect();
        let key = h.key(c.kind);
        let ranked = stats::top(&slice, &key);
        let needed = (c.cols * c.rows) as usize;
        let ranked = &ranked[..ranked.len().min(if c.filtered() || c.skip || c.rainbow { SCAN.max(needed) } else { needed })];

        // Each entry's most played (track, album): the names the lookup finds its cover by.
        let best = stats::top_pairs(&slice, &key);
        let tracks: Vec<i32> = ranked.iter().map(|(k, _)| best[k].0).collect();
        let albums: Vec<i32> = ranked.iter().map(|(k, _)| best[k].1).filter(|a| *a != 0).collect();
        let track_names = self.db().name_parts(Kind::Track, &tracks).await?;
        let album_names = self.db().name_parts(Kind::Album, &albums).await?;
        let artist_names = match c.kind {
            Kind::Artist => self.db().names(Kind::Artist, &ranked.iter().map(|r| r.0).collect::<Vec<_>>()).await?,
            _ => HashMap::new(),
        };

        let lb = &self.ingest.lb;
        let (mut tiles, mut covers, mut mappings) = (Vec::new(), Vec::new(), Vec::new());
        'scan: for chunk in ranked.chunks(50) {
            let names = |k: &i32| {
                let (track, album) = best[k];
                let (artist, track) = track_names.get(&track).map_or(("", ""), |(a, t)| (a.as_str(), t.as_str()));
                (artist, track, album_names.get(&album).map(|(_, n)| n.as_str()))
            };
            let items: Vec<(&str, &str, Option<&str>)> = chunk.iter().map(|(k, _)| names(k)).collect();
            let found = lb.lookup_many(&items).await?;
            // What the lookup found also serves the stats and cards (see `meta`); storing it can't stop the chart.
            let looked_up: Vec<i32> = chunk.iter().map(|(k, _)| best[k].0).collect();
            if let Err(e) = self.save_found(&looked_up, &found).await {
                tracing::warn!(error = ?e, "storing a chart's lookups failed");
            }
            let recordings: Vec<&str> = found.iter().filter_map(|m| m["recording_mbid"].as_str()).collect();
            let releases = lb.recording_releases(&recordings).await?;
            let release = |m: &Value| m["recording_mbid"].as_str().and_then(|r| releases.get(r)).cloned().unwrap_or_default();
            let groups = match c.filtered() {
                true => lb.release_groups(&releases.values().filter_map(|r| r["release_group_mbid"].as_str()).collect::<Vec<_>>()).await?,
                false => HashMap::new(),
            };
            for ((k, plays), mapping) in chunk.iter().zip(found) {
                let release = release(&mapping);
                // The track's page may show another album (the original of a song heard on a compilation). Then only the
                // release the lookup matched by album name is this album; its date and type are unknown.
                let page_is_album = c.kind != Kind::Album || same_album(&mapping, &release);
                let group = release["release_group_mbid"].as_str().and_then(|g| groups.get(g)).filter(|_| page_is_album);
                if c.filtered() && !keep(c, group) {
                    continue;
                }
                // The release group's cover is the album's main one; the page release's is next best.
                let cover = match page_is_album {
                    true => group.and_then(cover_url).or_else(|| cover_url(&release)),
                    false => cover_url(&mapping),
                };
                if (c.skip || c.rainbow) && cover.is_none() {
                    continue;
                }
                let lines = match c.kind {
                    Kind::Artist => vec![artist_names.get(k).cloned().unwrap_or_default()],
                    Kind::Album => album_names.get(k).map(|(a, n)| vec![a.clone(), n.clone()]).unwrap_or_default(),
                    Kind::Track => track_names.get(k).map(|(a, n)| vec![a.clone(), n.clone()]).unwrap_or_default(),
                };
                let (_, track, album) = names(k);
                tiles.push(Tile { lines, plays: *plays, image: None });
                covers.push(cover);
                mappings.push((mapping, track.to_owned(), album.map(str::to_owned)));
                if tiles.len() == needed {
                    break 'scan;
                }
            }
        }
        if tiles.len() < needed {
            let what = match c.filtered() || c.skip || c.rainbow || c.artist.is_some() {
                true => format!("{} that match these settings", kind_word(c.kind)),
                false => kind_word(c.kind).to_owned(),
            };
            let text = format!(
                "Not enough {what} in this period ({}): needed {needed}, found {}.\n\nTry a smaller chart or a longer period (weekly/monthly/quarterly/half/yearly/alltime).",
                period_name(c.days),
                tiles.len()
            );
            return Ok(with_edit(with_note(reply(text), note)));
        }
        if c.kind == Kind::Artist {
            // Artist photos, as on the cards; until one is cached the artist shows their most played album.
            let mut fetches = NEW_PHOTOS;
            for ((tile, cover), (mapping, track, album)) in tiles.iter().zip(&mut covers).zip(mappings) {
                let artist = &tile.lines[0];
                let cached = cache_get(&self.art_cache.lock().unwrap(), &art_keys(Kind::Artist, artist, "", &mapping).0);
                let art = match cached {
                    Some(art) => art,
                    None if fetches > 0 => {
                        fetches -= 1;
                        self.art(Kind::Artist, artist, Some(&track), album.as_deref(), mapping).await
                    }
                    None => continue,
                };
                if art.cover.is_some() {
                    *cover = art.cover;
                }
            }
        }
        let px = cell(needed as u32);
        let images: Vec<Option<String>> = stream::iter(covers)
            .map(|url| async move {
                let url = thumb(&url?, px);
                lb.image_data_uri(&url).await.inspect_err(|e| tracing::debug!(error = ?e, "chart cover failed")).ok()
            })
            .buffered(16)
            .collect()
            .await;
        for (tile, image) in tiles.iter_mut().zip(images) {
            tile.image = image;
        }
        let missing = tiles.iter().filter(|t| t.image.is_none()).count();
        let drawn = c.clone();
        let png = tokio::task::spawn_blocking(move || render(&drawn, tiles)).await??;

        let mut settings = Vec::new();
        if let Some(a) = c.artist {
            let artist = self.db().names(Kind::Artist, &[a]).await?.remove(&a).unwrap_or_default();
            settings.push(format!("Only {}", escape(&artist)));
        }
        if c.kind == Kind::Album {
            settings.extend(c.year.map(|y| format!("Released in {y}")));
            settings.extend(c.decade.map(|d| format!("Released in the {d}s")));
            settings.extend(c.no_singles.then(|| "Singles hidden".to_owned()));
        }
        settings.extend(c.skip.then(|| format!("{} without an image skipped", capitalized(kind_word(c.kind)))));
        settings.extend(c.rainbow.then(|| "Rainbow sorted".to_owned()));
        let total = h.entries.len() as i64 + i64::from(user.count_offset);
        let mut footer = format!("{} has {total} listens", user.lb_name);
        if missing > 0 {
            footer.push_str(&format!(" · {missing} without a cover"));
        }
        let kind = match c.kind {
            Kind::Album => "album",
            Kind::Artist => "artist",
            Kind::Track => "track",
        };
        let title = format!("{}x{} {} {kind} chart for {name}", c.cols, c.rows, period_name(c.days));
        let mut embed = EmbedBuilder::new()
            .color(COLOR)
            .title(title.chars().take(256).collect::<String>())
            .url(crate::lb::profile_url(&user.lb_name))
            .image(ImageSource::attachment("chart.png")?)
            .footer(EmbedFooterBuilder::new(footer));
        if !settings.is_empty() {
            embed = embed.description(settings.join("\n"));
        }
        let mut out = with_edit(with_note(embed.build(), note));
        out.files = vec![Attachment::from_bytes("chart.png".into(), png, 0)];
        Ok(out)
    }
}

/// Whether the release on a track's page is the album the lookup matched. Both use MusicBrainz's spelling ("…And
/// Justice for All"), which the listener's app may not; a deluxe edition of the same album counts as the same.
fn same_album(lookup: &Value, page: &Value) -> bool {
    let name = |v: &Value| album_key(v.as_str().unwrap_or_default()).to_lowercase();
    lookup["release_mbid"].as_str().is_some_and(|m| page["mbid"].as_str() == Some(m))
        || (lookup["release_name"].is_string() && name(&lookup["release_name"]) == name(&page["name"]))
}

/// Counts a chart for `user` in its 40 s window; `false` when the window already has `PER_WINDOW`. Old windows are dropped.
fn allow(uses: &mut HashMap<i64, (std::time::Instant, u8)>, user: i64, now: std::time::Instant) -> bool {
    uses.retain(|_, (start, _)| now.duration_since(*start) < WINDOW);
    let (_, count) = uses.entry(user).or_insert((now, 0));
    if *count >= PER_WINDOW {
        return false;
    }
    *count += 1;
    true
}

/// Whether an album passes the release filters. A year filter needs a known date; only known singles are hidden.
fn keep(c: &Chart, group: Option<&Value>) -> bool {
    let year = group.and_then(|g| g["date"].as_str()).and_then(|d| d.get(..4)).and_then(|y| y.parse::<i64>().ok());
    let single = group.is_some_and(|g| g["type"].as_str() == Some("Single"));
    c.year.is_none_or(|y| year == Some(y)) && c.decade.is_none_or(|d| year.is_some_and(|y| y / 10 * 10 == d)) && !(c.no_singles && single)
}

fn render(c: &Chart, mut tiles: Vec<Tile>) -> Result<Vec<u8>> {
    if c.rainbow {
        rainbow(&mut tiles);
    }
    crate::draw::png(&svg(c, &tiles))
}

fn svg(c: &Chart, tiles: &[Tile]) -> String {
    let px = cell(c.cols * c.rows);
    let (w, h, p) = (c.cols * px, c.rows * px, px as f32);
    let k = p / 300.0;
    let mut s = format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}" font-family="{SANS}, {FALLBACK}">
<defs><linearGradient id="fade" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="#000000" stop-opacity="0"/><stop offset="1" stop-color="#000000" stop-opacity="0.78"/></linearGradient></defs>
<rect width="{w}" height="{h}" fill="#17152b"/>
"##
    );
    for (i, t) in tiles.iter().enumerate() {
        let (x, y) = ((i as u32 % c.cols * px) as f32, (i as u32 / c.cols * px) as f32);
        match &t.image {
            Some(uri) => s.push_str(&format!(r#"<image x="{x}" y="{y}" width="{p}" height="{p}" preserveAspectRatio="xMidYMid slice" href="{uri}"/>"#)),
            None => s.push_str(&format!(r##"<rect x="{x}" y="{y}" width="{p}" height="{p}" fill="#2a2647" stroke="#17152b"/>"##)),
        }
        // A square without a cover always shows its name; it would be blank otherwise.
        let mut lines: Vec<(String, f32)> = match c.titles || t.image.is_none() {
            true => t.lines.iter().map(|l| (l.clone(), 16.0 * k)).collect(),
            false => Vec::new(),
        };
        if c.plays {
            lines.push((format!("{} plays", t.plays), 13.0 * k));
        }
        if lines.is_empty() {
            continue;
        }
        let height = lines.iter().map(|l| l.1 * 1.3).sum::<f32>() + 16.0 * k;
        s.push_str(&format!(r#"<rect x="{x}" y="{}" width="{p}" height="{height}" fill="url(#fade)"/>"#, y + p - height));
        let mut baseline = y + p - 9.0 * k;
        for (text, size) in lines.iter().rev() {
            s.push_str(&format!(
                r##"<text x="{}" y="{baseline}" text-anchor="middle" font-size="{size}" fill="#ffffff" stroke="#000000" stroke-opacity="0.5" stroke-width="{}" stroke-linejoin="round" paint-order="stroke">{}</text>"##,
                x + p / 2.0,
                size * 0.2,
                xml(&fit(text, p - 16.0 * k, *size))
            ));
            baseline -= size * 1.3;
        }
        s.push('\n');
    }
    s.push_str("</svg>");
    s
}

/// Cuts `s` to about `width` px at font size `size`. resvg can't measure text before drawing it, so this goes by
/// rough glyph widths: a full em for CJK, 0.6 em for the rest.
fn fit(s: &str, width: f32, size: f32) -> String {
    let w = |c: char| if c >= '\u{2E80}' { size } else { size * 0.6 };
    if s.chars().map(w).sum::<f32>() <= width {
        return s.to_owned();
    }
    let mut used = w('…');
    let mut out: String = s.chars().take_while(|&c| { used += w(c); used <= width }).collect();
    out.push('…');
    out
}

/// fmbot's rainbow order: colorful covers by hue, then grey ones from dark to light, then squares without a cover.
fn rainbow(tiles: &mut [Tile]) {
    tiles.sort_by_cached_key(|t| {
        let Some((hue, saturation, lightness)) = t.image.as_deref().and_then(average).map(hsl) else { return (2, 0, 0) };
        let (hue, lightness) = ((hue * 10.0) as i32, (lightness * 1000.0) as i32);
        if saturation < 0.15 { (1, lightness, 0) } else { (0, hue, lightness) }
    });
}

/// Average color of an image (0–1 RGB), from an 8x8 render of it.
fn average(uri: &str) -> Option<[f32; 3]> {
    let svg = format!(r#"<svg xmlns="http://www.w3.org/2000/svg" width="8" height="8"><image width="8" height="8" preserveAspectRatio="none" href="{uri}"/></svg>"#);
    let pixmap = crate::draw::pixmap(&svg).ok()?;
    let n = pixmap.pixels().len() as f32 * 255.0;
    let sum = pixmap.pixels().iter().fold([0.0; 3], |s, p| [s[0] + f32::from(p.red()), s[1] + f32::from(p.green()), s[2] + f32::from(p.blue())]);
    Some(sum.map(|v| v / n))
}

/// (hue in degrees, saturation, lightness) of an RGB color.
fn hsl([r, g, b]: [f32; 3]) -> (f32, f32, f32) {
    let (max, min) = (r.max(g).max(b), r.min(g).min(b));
    let (l, d) = ((max + min) / 2.0, max - min);
    if d == 0.0 {
        return (0.0, 0.0, l);
    }
    let hue = if max == r {
        ((g - b) / d).rem_euclid(6.0)
    } else if max == g {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    };
    (hue * 60.0, d / (1.0 - (2.0 * l - 1.0).abs()), l)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_options() {
        let (c, artist) = parse(Kind::Album, "q 8x8 nt s pc rainbow ns <@!42> Tame Impala");
        assert_eq!((c.cols, c.rows, c.days, c.titles, c.plays, c.skip, c.rainbow, c.no_singles), (8, 8, Some(90), false, true, true, true, true));
        assert_eq!((c.target, artist.as_deref()), (Some(42), Some("Tame Impala")));
        // Defaults: 3x3 weekly with titles; all time once a release year or an artist is given.
        assert_eq!(parse(Kind::Artist, ""), (Chart::new(Kind::Artist), None));
        assert_eq!(parse(Kind::Album, "r:2023").0.days, None);
        assert_eq!(parse(Kind::Album, "Radiohead").0.days, None);
        assert_eq!(parse(Kind::Album, "Radiohead month").0.days, Some(30));
        assert_eq!(parse(Kind::Album, "d:80s").0.decade, Some(1980));
        assert_eq!(parse(Kind::Album, "decade:2010").0.decade, Some(2010));
        // aoty/aotd take a bare year; elsewhere it's an artist.
        assert_eq!(parse(Kind::Album, "aoty 2019 4x4").0.year, Some(2019));
        assert_eq!(parse(Kind::Album, "aoty").0.year, Some(this_year()));
        assert_eq!(parse(Kind::Album, "aotd 90s").0.decade, Some(1990));
        assert_eq!(parse(Kind::Album, "1975").1.as_deref(), Some("1975"));
        assert_eq!(size("10x10"), Some((10, 10)));
        assert_eq!(size("0x5"), None);
        assert!(parse(Kind::Album, "11x10").0.invalid().is_some(), "over 100 images");
        assert!(parse(Kind::Album, "20x5").0.invalid().is_none());
    }

    #[test]
    fn button_and_form_ids() {
        let (mut c, _) = parse(Kind::Album, "5x4 90d pc s ns r:2001 <@123456789012345678>");
        c.artist = Some(2_000_000_000);
        let id = button_id(876_543_210_987_654_321, &c);
        assert!(id.len() <= 100, "{} characters", id.len());
        assert_eq!(from_button(&id), Some((876_543_210_987_654_321, c.clone())));
        assert_eq!(from_button("ov:1:2:3"), None);
        // The form's answers rebuild the same chart; unreadable answers turn into what `invalid` explains.
        let form = |size: &'static str, released: &'static str| {
            move |id: &str| match id {
                "size" => vec![size.to_owned()],
                "period" => vec!["90".to_owned()],
                "options" => vec!["titles".to_owned(), "plays".to_owned(), "skip".to_owned(), "singles".to_owned()],
                "released" => vec![released.to_owned()],
                _ => Vec::new(),
            }
        };
        let modal_id = format!("chm:7:{}:a:{}", c.target.unwrap(), c.artist.unwrap());
        assert_eq!(from_modal(&modal_id, form("5x4", "2001")), Some((7, c.clone())));
        assert_eq!(from_modal(&modal_id, form("5x4", "90s")).unwrap().1.decade, Some(1990));
        assert!(from_modal(&modal_id, form("big", "")).unwrap().1.invalid().is_some());
        assert!(from_modal(&modal_id, form("3x3", "soon")).unwrap().1.invalid().is_some());
    }

    #[test]
    fn release_filters() {
        let group = serde_json::json!({"date": "1994-08-22", "type": "Album"});
        let single = serde_json::json!({"date": "2024-01-05", "type": "Single"});
        let (aoty, _) = parse(Kind::Album, "r:1994");
        assert!(keep(&aoty, Some(&group)));
        assert!(!keep(&aoty, Some(&single)));
        assert!(!keep(&aoty, None), "unknown date");
        assert!(keep(&parse(Kind::Album, "d:90s").0, Some(&group)));
        let (ns, _) = parse(Kind::Album, "ns");
        assert!(!keep(&ns, Some(&single)) && keep(&ns, Some(&group)) && keep(&ns, None));
        // A track heard on "Greatest Hits" whose page shows "A Night at the Opera" isn't that album.
        use serde_json::json;
        assert!(!same_album(&json!({"release_mbid": "g", "release_name": "Greatest Hits"}), &json!({"mbid": "o", "name": "A Night at the Opera"})));
        // The same release in MusicBrainz's spelling, or a deluxe edition of it, is.
        let justice = json!({"release_mbid": "j", "release_name": "…And Justice for All"});
        assert!(same_album(&justice, &json!({"mbid": "j", "name": "…And Justice for All"})));
        assert!(same_album(&justice, &json!({"mbid": "d", "name": "…And Justice for All (Deluxe Edition)"})));
        assert!(!same_album(&json!(null), &json!({"mbid": "j", "name": ""})), "no match at all");
    }

    #[test]
    fn four_charts_per_window() {
        let (mut uses, start) = (HashMap::new(), std::time::Instant::now());
        assert!((0..4).all(|_| allow(&mut uses, 1, start)));
        assert!((0..300).all(|_| !allow(&mut uses, 1, start)), "refused from the fifth on, without overflowing");
        assert!(allow(&mut uses, 2, start), "others aren't limited");
        assert!(allow(&mut uses, 1, start + WINDOW), "a new window");
    }

    #[test]
    fn fits_and_colors() {
        assert_eq!(fit("Short", 200.0, 16.0), "Short");
        let cut = fit("A very long album title that cannot fit", 100.0, 16.0);
        assert!(cut.ends_with('…') && cut.chars().count() < 12, "{cut}");
        assert!(fit("鷺巣詩郎鷺巣詩郎", 50.0, 16.0).chars().count() <= 4);
        let (h, s, l) = hsl([1.0, 0.0, 0.0]);
        assert_eq!((h, s, l), (0.0, 1.0, 0.5));
        assert_eq!(hsl([0.0, 0.0, 1.0]).0, 240.0);
        assert_eq!(hsl([0.5, 0.5, 0.5]).1, 0.0);
        assert_eq!(cell(9), 300);
        assert_eq!(cell(100), 154);
    }

    /// A solid-color PNG as a `data:` URI.
    fn swatch(r: u8, g: u8, b: u8) -> String {
        use base64::Engine;
        let mut p = resvg::tiny_skia::Pixmap::new(40, 40).unwrap();
        p.fill(resvg::tiny_skia::Color::from_rgba8(r, g, b, 255));
        format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(p.encode_png().unwrap()))
    }

    #[test]
    fn renders_a_rainbow_chart() {
        let tile = |name: &str, image: Option<String>| Tile { lines: vec!["Artist & <Co>".into(), name.into()], plays: 12, image };
        let tiles = vec![
            tile("Blue", Some(swatch(20, 40, 230))),
            tile("No cover", None),
            tile("Grey", Some(swatch(128, 128, 128))),
            tile("Red", Some(swatch(230, 30, 20))),
        ];
        let (mut c, _) = parse(Kind::Album, "2x2 rainbow pc");
        let mut sorted = tiles.iter().map(|t| Tile { lines: t.lines.clone(), plays: t.plays, image: t.image.clone() }).collect::<Vec<_>>();
        rainbow(&mut sorted);
        let order: Vec<&str> = sorted.iter().map(|t| t.lines[1].as_str()).collect();
        assert_eq!(order, ["Red", "Blue", "Grey", "No cover"]);
        c.titles = true;
        let png = render(&c, tiles).unwrap();
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        // For a look: CHART_OUT=/path/to.png cargo test chart
        if let Ok(path) = std::env::var("CHART_OUT") {
            std::fs::write(path, &png).unwrap();
        }
    }

    /// Real APIs and covers: `cargo test -- --ignored live_chart --nocapture` (the lookup also needs `LB_TOKEN`).
    /// `CHART_OUT=/dir` writes the 3x3 and 10x10 PNGs there.
    #[tokio::test]
    #[ignore]
    async fn live_chart() {
        let lb = crate::lb::Lb::new(std::env::var("LB_TOKEN").unwrap_or_default());
        let roads = "ab6cc25d-f8a9-428b-805d-13bd84a76731";
        let release = lb.recording_releases(&[roads]).await.unwrap().remove(roads).unwrap();
        assert_eq!((release["year"].as_i64(), release["release_group_mbid"].as_str()), (Some(1994), Some("48140466-cff6-3222-bd55-63c27e43190d")));
        let group = lb.release_groups(&["48140466-cff6-3222-bd55-63c27e43190d"]).await.unwrap().remove("48140466-cff6-3222-bd55-63c27e43190d").unwrap();
        assert!(keep(&parse(Kind::Album, "r:1994 ns").0, Some(&group)));
        if std::env::var("LB_TOKEN").is_ok_and(|t| !t.is_empty()) {
            let found = lb.lookup_many(&[("Portishead", "Roads", Some("Dummy")), ("Nobody Real", "", None), ("Portishead", "Glory Box", None)]).await.unwrap();
            assert!(found[0]["recording_mbid"].is_string() && found[1].is_null() && found[2]["recording_mbid"].is_string(), "{found:?}");
            // A compilation: the track's page shows the original album, so the cover comes from the matched release.
            let hits = lb.lookup_many(&[("Queen", "Bohemian Rhapsody", Some("Greatest Hits"))]).await.unwrap().remove(0);
            let rec = hits["recording_mbid"].as_str().unwrap();
            let page = lb.recording_releases(&[rec]).await.unwrap().remove(rec).unwrap();
            assert!(!same_album(&hits, &page), "{page}");
            let justice = lb.lookup_many(&[("Metallica", "One", Some("...And Justice for All"))]).await.unwrap().remove(0);
            let rec = justice["recording_mbid"].as_str().unwrap();
            assert!(same_album(&justice, &lb.recording_releases(&[rec]).await.unwrap()[rec]), "{justice}");
            assert_eq!(cover_url(&hits).unwrap(), format!("https://coverartarchive.org/release/{}/front-1200", hits["release_mbid"].as_str().unwrap()));
        }
        let albums = lb.featured_albums("rob", Some("all_time")).await.unwrap();
        for (side, name) in [(3u32, "3x3"), (10, "10x10")] {
            let px = cell(side * side);
            let covers: Vec<String> = albums.iter().cycle().take((side * side) as usize).map(|a| thumb(&a.cover, px)).collect();
            let started = std::time::Instant::now();
            let images: Vec<Option<String>> = stream::iter(covers).map(|u| { let lb = &lb; async move { lb.image_data_uri(&u).await.ok() } }).buffered(16).collect().await;
            let fetched = started.elapsed();
            let tiles = albums.iter().cycle().zip(images).map(|(a, image)| Tile { lines: a.label.split(" - ").map(str::to_owned).collect(), plays: 42, image }).collect();
            let (c, _) = parse(Kind::Album, &format!("{name} pc rainbow"));
            let started = std::time::Instant::now();
            let png = render(&c, tiles).unwrap();
            println!("{name}: {} covers fetched in {fetched:?}, drawn in {:?}, {} KB", side * side, started.elapsed(), png.len() / 1024);
            assert!(png.len() < 9 << 20, "{name} is {} bytes; Discord takes 10 MB", png.len());
            if let Ok(dir) = std::env::var("CHART_OUT") {
                std::fs::write(format!("{dir}/live-{name}.png"), &png).unwrap();
            }
        }
    }
}
