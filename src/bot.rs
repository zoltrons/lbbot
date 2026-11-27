//! Discord side: prefix and slash commands are parsed into the same `Cmd` and run by `run`.

use std::{collections::HashMap, sync::{Arc, RwLock}};

use anyhow::Result;
use serde_json::Value;
use twilight_gateway::{Event, MessageSender};
use twilight_http::Client;
use twilight_model::{
    application::{
        command::{Command, CommandOptionChoice, CommandOptionChoiceValue, CommandType},
        interaction::{
            Interaction, InteractionData, InteractionType,
            application_command::{CommandData, CommandOptionValue},
            modal::ModalInteractionComponent,
        },
    },
    channel::{
        Message,
        message::{
            Component, Embed, MessageFlags,
            component::{Label, TextDisplay, TextInput, TextInputStyle},
        },
    },
    gateway::payload::outgoing::RequestGuildMembers,
    guild::Permissions,
    http::interaction::{InteractionResponse, InteractionResponseData, InteractionResponseType},
    id::{Id, marker::ApplicationMarker},
    user::User,
};
use twilight_util::builder::{
    interaction_response::{ChannelMessageBuilder, ModalBuilder},
    command::{BooleanBuilder, CommandBuilder, StringBuilder, UserBuilder},
    embed::{EmbedAuthorBuilder, EmbedBuilder, EmbedFooterBuilder, ImageSource},
};

use crate::{db::{Db, Kind}, ingest::Ingest};

const DEFAULT_PREFIX: &str = ",";
const SETTINGS: &str = "[listenbrainz.org/settings](https://listenbrainz.org/settings/)";
pub(crate) const COLOR: u32 = 0x353070;

pub struct Bot {
    pub http: Arc<Client>,
    pub app_id: Id<ApplicationMarker>,
    pub ingest: Arc<Ingest>,
    pub prefixes: RwLock<HashMap<i64, String>>,
    /// For the WhoKnows title; filled by GUILD_CREATE.
    pub guild_names: RwLock<HashMap<i64, String>>,
    /// Card image/tag cache, so popular artists don't fill the MusicBrainz queue.
    pub art_cache: std::sync::Mutex<HashMap<ArtKey, (std::time::Instant, Art)>>,
    /// LB popularity by (entity, mbid): `None` = LB has no stats for it yet (see `meta`).
    pub popularity_cache: std::sync::Mutex<PopularityCache>,
    pub featured: Arc<crate::featured::Featured>,
    /// Discord id of the application owner, for owner-only commands.
    pub owner: Option<i64>,
    /// Charts per Discord id in the current window (see `chart::allow`).
    pub chart_uses: std::sync::Mutex<HashMap<i64, (std::time::Instant, u8)>>,
}

/// (artist, album or track name, (playing track name, its mapping))
pub(crate) type Subject = (String, Option<String>, (Option<String>, Value));

pub type PopularityCache = HashMap<(&'static str, String), (std::time::Instant, Option<(i64, i64)>)>;

/// (kind, lowercase artist, lowercase album/track)
pub type ArtKey = (Kind, String, String);
/// Card extras: cover, tags and the entity's ListenBrainz page.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Art {
    pub cover: Option<String>,
    pub tags: Option<String>,
    pub url: Option<String>,
}
/// For each in-memory cache: card images and tags, popularity.
const ART_TTL: std::time::Duration = std::time::Duration::from_secs(24 * 3600);
const ART_CACHE_MAX: usize = 10_000;

#[derive(Debug, PartialEq)]
enum Cmd {
    Login,
    Update,
    Np,
    Wk { kind: Kind, global: bool, query: Option<String> },
    Prefix(Option<String>),
    Privacy,
    Remove { confirm: bool },
    Help,
    RefreshMembers,
    /// Show (`None`) or set the time zone the history commands use.
    Timezone(Option<String>),
    /// Owner only, text only: ban the current avatar's album and pick another one.
    FeaturedBlock,
    /// Commands computed from the stored history; `arg` is the rest of the text, parsed by each command.
    Hist { cmd: Hc, kind: Kind, arg: String },
    /// An image chart; `artist` is a name still to look up, `private` the slash option.
    Chart { chart: crate::chart::Chart, artist: Option<String>, private: bool },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Hc {
    Overview,
    Streak,
    Streaks,
    Gaps,
    Discoveries,
    DiscoveryDate,
    LastListened,
    Pace,
    ArtistPace,
    Milestone,
    Year,
    Recap,
    Taste,
    Affinity,
    Iceberg,
    Receipt,
    Overdue,
    Comebacks,
    OnThisDay,
    Movers,
    Growth,
    Obsessions,
    Hidden,
    Obscurity,
    ServerIceberg,
    Decades,
    ListeningTime,
    Clock,
    Unlinked,
}

pub(crate) struct Ctx {
    pub discord_id: i64,
    pub guild: Option<i64>,
    pub name: String,
}

/// A reply: one embed, plus buttons and files for the commands that have them.
pub(crate) struct Out {
    pub embed: Embed,
    pub components: Vec<Component>,
    pub files: Vec<twilight_model::http::attachment::Attachment>,
}

impl From<Embed> for Out {
    fn from(embed: Embed) -> Self {
        Self { embed, components: Vec::new(), files: Vec::new() }
    }
}

fn failed(e: anyhow::Error) -> Out {
    error_reply(e).into()
}

pub(crate) fn login_first() -> Embed {
    reply(format!("Connect your ListenBrainz account with `/login` first. You'll need your **User token** from {SETTINGS}."))
}

pub(crate) fn reply(text: impl Into<String>) -> Embed {
    EmbedBuilder::new().color(COLOR).description(text).build()
}

/// fmbot's text aliases.
fn parse_text(word: &str, rest: &str) -> Option<Cmd> {
    let query = (!rest.is_empty()).then(|| rest.to_owned());
    let wk = |kind, global| Some(Cmd::Wk { kind, global, query: query.clone() });
    let hist = |cmd, kind| Some(Cmd::Hist { cmd, kind, arg: rest.to_owned() });
    let chart = |kind, arg: &str| {
        let (chart, artist) = crate::chart::parse(kind, arg);
        Some(Cmd::Chart { chart, artist, private: false })
    };
    match word {
        "chart" | "c" | "topster" | "topsters" => chart(Kind::Album, rest),
        "aoty" | "albumsoftheyear" | "albumoftheyear" => chart(Kind::Album, &format!("aoty {rest}")),
        "aotd" | "albumsofthedecade" | "albumofthedecade" => chart(Kind::Album, &format!("aotd {rest}")),
        "artistchart" | "ac" | "top" | "ca" | "chartartist" | "chartartists" | "artistcharts" => chart(Kind::Artist, rest),
        "trackchart" | "tc" | "charttrack" | "charttracks" | "trackcharts" => chart(Kind::Track, rest),
        "overview" | "o" | "ov" => hist(Hc::Overview, Kind::Artist),
        "streak" | "str" | "combo" | "cb" => hist(Hc::Streak, Kind::Artist),
        "streaks" | "combos" | "streakhistory" => hist(Hc::Streaks, Kind::Artist),
        "gaps" | "gap" => hist(Hc::Gaps, Kind::Artist),
        "discoveries" | "discovered" | "discovery" => hist(Hc::Discoveries, Kind::Artist),
        "discoverydate" | "dd" | "firstlistened" => hist(Hc::DiscoveryDate, Kind::Artist),
        "albumdiscoverydate" | "dda" => hist(Hc::DiscoveryDate, Kind::Album),
        "trackdiscoverydate" | "ddt" => hist(Hc::DiscoveryDate, Kind::Track),
        "lastlistened" | "ll" | "lastplayed" => hist(Hc::LastListened, Kind::Artist),
        "albumlastlistened" | "lla" => hist(Hc::LastListened, Kind::Album),
        "tracklastlistened" | "llt" => hist(Hc::LastListened, Kind::Track),
        "pace" | "pc" => hist(Hc::Pace, Kind::Artist),
        "artistpace" | "apc" | "apace" => hist(Hc::ArtistPace, Kind::Artist),
        "milestone" | "ms" | "mls" => hist(Hc::Milestone, Kind::Artist),
        "year" | "yr" | "yearoverview" => hist(Hc::Year, Kind::Artist),
        "recap" | "rc" => hist(Hc::Recap, Kind::Artist),
        "taste" | "t" | "tst" => hist(Hc::Taste, Kind::Artist),
        "affinity" | "aff" | "neighbors" | "n" => hist(Hc::Affinity, Kind::Artist),
        "iceberg" | "ice" | "icebergify" | "berg" => hist(Hc::Iceberg, Kind::Artist),
        "receipt" | "rcpt" | "receiptify" => hist(Hc::Receipt, Kind::Artist),
        "overdue" | "od" | "missed" => hist(Hc::Overdue, Kind::Track),
        "comebacks" | "comeback" | "returns" => hist(Hc::Comebacks, Kind::Artist),
        "onthisday" | "otd" | "today" => hist(Hc::OnThisDay, Kind::Artist),
        "movers" | "climbers" | "mv" => hist(Hc::Movers, Kind::Artist),
        "growth" | "gr" | "listenchart" => hist(Hc::Growth, Kind::Artist),
        "obsessions" | "obsession" | "binges" => hist(Hc::Obsessions, Kind::Track),
        "hidden" | "gems" | "underrated" => hist(Hc::Hidden, Kind::Track),
        "obscurity" | "underground" | "ug" => hist(Hc::Obscurity, Kind::Artist),
        "servericeberg" | "sice" | "serverberg" => hist(Hc::ServerIceberg, Kind::Artist),
        "decades" | "decade" | "eras" => hist(Hc::Decades, Kind::Track),
        "listeningtime" | "lt" | "playtime" => hist(Hc::ListeningTime, Kind::Artist),
        "clock" | "hours" | "when" => hist(Hc::Clock, Kind::Artist),
        "unlinked" | "unmapped" => hist(Hc::Unlinked, Kind::Track),
        "timezone" | "tz" | "settimezone" => Some(Cmd::Timezone(query.clone())),
        "login" | "set" | "setusername" | "connect" => Some(Cmd::Login),
        "update" | "u" => Some(Cmd::Update),
        "np" | "nowplaying" => Some(Cmd::Np),
        "whoknows" | "w" | "wk" | "thosewhoknow" => wk(Kind::Artist, false),
        "whoknowsalbum" | "wa" | "wka" | "wkab" | "wab" | "wkal" | "wkalbum" => wk(Kind::Album, false),
        "whoknowstrack" | "wt" | "wkt" | "wktr" | "wtr" | "wktrack" => wk(Kind::Track, false),
        "globalwhoknows" | "gw" | "gwk" | "globalwk" => wk(Kind::Artist, true),
        "globalwhoknowsalbum" | "gwa" | "gwka" | "gwab" | "gwkab" | "globalwka" | "globalwkalbum" => wk(Kind::Album, true),
        "globalwhoknowstrack" | "gwt" | "gwkt" | "gwtr" | "gwktr" | "globalwkt" | "globalwktrack" => wk(Kind::Track, true),
        "prefix" => Some(Cmd::Prefix(None)),
        "privacy" => Some(Cmd::Privacy),
        "remove" | "delete" | "removedata" | "deletedata" | "logout" => Some(Cmd::Remove { confirm: rest == "confirm" }),
        "help" => Some(Cmd::Help),
        "refreshmembers" => Some(Cmd::RefreshMembers),
        "featured" if rest == "block" => Some(Cmd::FeaturedBlock),
        _ => None,
    }
}

pub fn slash_commands() -> Vec<Command> {
    let cmd = |name: &str, desc: &str| CommandBuilder::new(name, desc, CommandType::ChatInput);
    let query = |desc: &str| StringBuilder::new("query", desc).required(false);
    let mut out = vec![
        cmd("login", "Connect your ListenBrainz account").build(),
        cmd("update", "Fetch your latest listens").build(),
        cmd("np", "Your current or last played track").build(),
        cmd("wk", "Who listens to an artist in this server").option(query("Artist")).build(),
        cmd("wkalbum", "Who listens to an album in this server").option(query("Artist - Album")).build(),
        cmd("wktrack", "Who listens to a track in this server").option(query("Artist - Track")).build(),
        cmd("gwk", "Who listens to an artist across the bot").option(query("Artist")).build(),
        cmd("gwkalbum", "Who listens to an album across the bot").option(query("Artist - Album")).build(),
        cmd("gwktrack", "Who listens to a track across the bot").option(query("Artist - Track")).build(),
        cmd("prefix", "Change the text command prefix for this server")
            .option(StringBuilder::new("prefix", "New prefix (up to 5 characters)").required(true))
            .default_member_permissions(Permissions::MANAGE_GUILD)
            .build(),
        cmd("privacy", "Show or hide yourself in global WhoKnows").build(),
        cmd("remove", "Delete your account and all your data")
            .option(BooleanBuilder::new("confirm", "Confirm the deletion").required(true))
            .build(),
        cmd("help", "List of commands").build(),
        cmd("refreshmembers", "Reload this server's member list from Discord")
            .default_member_permissions(Permissions::MANAGE_GUILD)
            .build(),
        cmd("timezone", "Set the time zone your days and hours are counted in")
            .option(StringBuilder::new("zone", "City or time zone, e.g. Istanbul").required(false).autocomplete(true))
            .build(),
    ];
    for (name, desc, hint) in HIST_SLASH {
        let mut c = cmd(name, desc);
        if matches!(name, "discoverydate" | "lastlistened") {
            let kind = StringBuilder::new("type", "Artist, album or track").required(false);
            c = c.option(kind.choices([("Artist", "artist"), ("Album", "album"), ("Track", "track")]));
        }
        if name == "taste" {
            c = c.option(UserBuilder::new("user", "Who to compare with").required(true));
        }
        if !hint.is_empty() {
            c = c.option(StringBuilder::new("options", hint).required(false));
        }
        out.push(c.build());
    }
    out.push(crate::chart::slash_command());
    out
}

/// Slash versions of the history commands: (name, description, hint for the free-text options; empty = none).
const HIST_SLASH: [(&str, &str, &str); 29] = [
    ("overview", "Your last days: plays, genres and top artist, album and track per day", ""),
    ("streak", "Your current artist, album and track streak", ""),
    ("streaks", "Your longest streaks", "artists, albums or tracks"),
    ("gaps", "What you came back to after the longest break", "artists, albums or tracks"),
    ("discoveries", "Artists you discovered recently", "Period (week, month, year, 90d…) and artists/albums/tracks"),
    ("discoverydate", "When you first listened to something", "Artist or Artist - Name"),
    ("lastlistened", "When you last listened to something", "Artist or Artist - Name"),
    ("pace", "When you'll reach a play count", "Goal and period, e.g. 50000 week"),
    ("artistpace", "When you'll reach a play count for an artist", "Goal and artist, e.g. 1000 Radiohead"),
    ("milestone", "Your nth listen", "Listen number"),
    ("year", "Your year in music", "Year"),
    ("recap", "Summary of a period", "Period (week, month, year, 90d…)"),
    ("taste", "Compare your taste with someone", "Period (week, month, year…)"),
    ("affinity", "Who in this server listens most like you", ""),
    ("iceberg", "Your artists by how known they are on ListenBrainz", "Period (week, month, year…)"),
    ("receipt", "Your top tracks as a receipt", "Period (week, month, year…)"),
    ("overdue", "What you used to play often and haven't lately", "artists, albums or tracks"),
    ("comebacks", "What you came back to after a break", "Period (week, month, year…) and artists/albums/tracks"),
    ("onthisday", "This day in earlier years, and songs released on it", ""),
    ("movers", "What climbed your all-time top", "Period (week, month, year…) and artists/albums/tracks"),
    ("growth", "Your plays per year, or per month of a year, as a chart", "Year and/or artist, e.g. 2025 Radiohead"),
    ("obsessions", "Your biggest days with one song, and one-song artists", ""),
    ("hidden", "Tracks you play that few others on ListenBrainz do", "Period (week, month, year…)"),
    ("obscurity", "How underground your artists are, and who here is most", "Period (week, month, year…)"),
    ("servericeberg", "This server's top artists by how known they are", ""),
    ("decades", "When the music you play came out, as a chart", "Period (week, month, year…)"),
    ("listeningtime", "How long you listened, by track lengths", "Period (week, month, year…)"),
    ("clock", "When you listen: weekday and hour, as a chart", "Period (week, month, year…)"),
    ("unlinked", "Your top tracks ListenBrainz can't match to MusicBrainz", ""),
];

fn parse_slash(data: &CommandData) -> Option<Cmd> {
    let string = |name: &str| {
        data.options.iter().find(|o| o.name == name).and_then(|o| match &o.value {
            CommandOptionValue::String(s) if !s.trim().is_empty() => Some(s.trim().to_owned()),
            _ => None,
        })
    };
    let wk = |kind, global| Some(Cmd::Wk { kind, global, query: string("query") });
    match data.name.as_str() {
        "login" => Some(Cmd::Login),
        "update" => Some(Cmd::Update),
        "np" => Some(Cmd::Np),
        "wk" => wk(Kind::Artist, false),
        "wkalbum" => wk(Kind::Album, false),
        "wktrack" => wk(Kind::Track, false),
        "gwk" => wk(Kind::Artist, true),
        "gwkalbum" => wk(Kind::Album, true),
        "gwktrack" => wk(Kind::Track, true),
        "prefix" => Some(Cmd::Prefix(string("prefix"))),
        "privacy" => Some(Cmd::Privacy),
        "remove" => Some(Cmd::Remove {
            confirm: data.options.iter().any(|o| o.name == "confirm" && o.value == CommandOptionValue::Boolean(true)),
        }),
        "help" => Some(Cmd::Help),
        "refreshmembers" => Some(Cmd::RefreshMembers),
        "timezone" => Some(Cmd::Timezone(string("zone"))),
        "chart" => {
            let sub = data.options.first()?;
            let CommandOptionValue::SubCommand(options) = &sub.value else { return None };
            let (chart, artist, private) = crate::chart::from_slash(&sub.name, options);
            Some(Cmd::Chart { chart, artist, private })
        }
        name if HIST_SLASH.iter().any(|(n, ..)| *n == name) => {
            let alias = match (name, string("type").as_deref()) {
                ("discoverydate", Some("album")) => "dda",
                ("discoverydate", Some("track")) => "ddt",
                ("lastlistened", Some("album")) => "lla",
                ("lastlistened", Some("track")) => "llt",
                _ => name,
            };
            let mut rest = string("options").unwrap_or_default();
            if let Some(CommandOptionValue::User(id)) = data.options.iter().find(|o| o.name == "user").map(|o| &o.value) {
                rest = format!("<@{id}> {rest}");
            }
            parse_text(alias, rest.trim())
        }
        _ => None,
    }
}

fn display_name(user: &User, nick: Option<&str>) -> String {
    nick.or(user.global_name.as_deref()).unwrap_or(&user.name).to_owned()
}

impl Bot {
    pub(crate) fn db(&self) -> &Db {
        &self.ingest.db
    }

    pub async fn handle(self: Arc<Self>, event: Event, sender: MessageSender) {
        let result = match event {
            Event::MessageCreate(msg) => self.on_message(&msg).await,
            Event::InteractionCreate(i) => self.on_interaction(&i, &sender).await,
            Event::GuildCreate(g) => {
                let id = g.id();
                if let twilight_model::gateway::payload::incoming::GuildCreate::Available(g) = &*g {
                    self.guild_names.write().unwrap().insert(id.get() as i64, g.name.clone());
                }
                // Rebuild the member list; Discord sends it in MEMBER_CHUNK parts.
                let prefix = self.db().prefix(id.get() as i64).await;
                if let Ok(Some(p)) = &prefix {
                    self.prefixes.write().unwrap().insert(id.get() as i64, p.clone());
                }
                prefix.and(self.db().clear_guild(id.get() as i64).await).and_then(|()| {
                    Ok(sender.command(&RequestGuildMembers::builder(id).query("", None))?)
                })
            }
            // `unavailable` means an outage; otherwise the bot was removed and keeps nothing about the server.
            Event::GuildDelete(g) if !g.unavailable.unwrap_or(false) => {
                let id = g.id.get() as i64;
                self.prefixes.write().unwrap().remove(&id);
                self.guild_names.write().unwrap().remove(&id);
                self.db().forget_guild(id).await
            }
            Event::GuildUpdate(g) => {
                self.guild_names.write().unwrap().insert(g.id.get() as i64, g.name.clone());
                Ok(())
            }
            Event::MemberChunk(c) => {
                let members: Vec<(i64, String)> = c
                    .members
                    .iter()
                    .map(|m| (m.user.id.get() as i64, display_name(&m.user, m.nick.as_deref())))
                    .collect();
                self.db().add_members(c.guild_id.get() as i64, &members).await
            }
            Event::MemberAdd(m) => {
                let entry = (m.member.user.id.get() as i64, display_name(&m.member.user, m.member.nick.as_deref()));
                self.db().add_members(m.guild_id.get() as i64, &[entry]).await
            }
            Event::MemberUpdate(m) => {
                let entry = (m.user.id.get() as i64, display_name(&m.user, m.nick.as_deref()));
                self.db().add_members(m.guild_id.get() as i64, &[entry]).await
            }
            Event::MemberRemove(m) => self.db().remove_member(m.guild_id.get() as i64, m.user.id.get() as i64).await,
            _ => Ok(()),
        };
        if let Err(e) = result {
            tracing::error!(error = ?e, "event handler failed");
        }
    }

    fn prefix(&self, guild: Option<i64>) -> String {
        guild
            .and_then(|g| self.prefixes.read().unwrap().get(&g).cloned())
            .unwrap_or_else(|| DEFAULT_PREFIX.to_owned())
    }

    async fn on_message(&self, msg: &Message) -> Result<()> {
        if msg.author.bot {
            return Ok(());
        }
        let guild = msg.guild_id.map(|g| g.get() as i64);
        let prefix = self.prefix(guild);
        let Some(body) = msg.content.strip_prefix(prefix.as_str()) else { return Ok(()) };
        let (word, rest) = body.split_once(char::is_whitespace).unwrap_or((body, ""));
        let Some(cmd) = parse_text(&word.to_lowercase(), rest.trim()) else { return Ok(()) };
        // No command text or ids in logs: the command name and a keyed hash are enough to follow a problem.
        let name = match &cmd {
            Cmd::Hist { cmd, .. } => format!("{cmd:?}"),
            cmd => format!("{cmd:?}"),
        };
        let name = name.split([' ', '(', '{']).next().unwrap_or_default();
        tracing::info!(user = %self.db().1.log_id(msg.author.id.get() as i64), cmd = name, "text command");

        let ctx = Ctx {
            discord_id: msg.author.id.get() as i64,
            guild,
            name: display_name(&msg.author, msg.member.as_ref().and_then(|m| m.nick.as_deref())),
        };
        let typing = self.http.create_typing_trigger(msg.channel_id);
        let _ = typing.await;
        let out: Out = match cmd {
            Cmd::Login => reply("Use `/login` so your token stays private.").into(),
            Cmd::Prefix(_) => reply(format!("Prefix: `{prefix}`. Use `/prefix` to change it (needs Manage Server).")).into(),
            Cmd::RefreshMembers => reply("Use `/refreshmembers` to reload the member list (needs Manage Server).").into(),
            Cmd::FeaturedBlock => self.featured_block(&ctx).await.unwrap_or_else(error_reply).into(),
            cmd => self.run(&ctx, cmd).await.unwrap_or_else(failed),
        };
        self.http
            .create_message(msg.channel_id)
            .embeds(&[out.embed])
            .components(&out.components)
            .attachments(&out.files)
            .reply(msg.id)
            .await?;
        Ok(())
    }

    async fn on_interaction(&self, i: &Interaction, sender: &MessageSender) -> Result<()> {
        let Some(user) = i.author() else { return Ok(()) };
        let ctx = Ctx {
            discord_id: user.id.get() as i64,
            guild: i.guild_id.map(|g| g.get() as i64),
            name: display_name(user, i.member.as_ref().and_then(|m| m.nick.as_deref())),
        };
        let client = self.http.interaction(self.app_id);
        match &i.data {
            Some(InteractionData::ApplicationCommand(data)) if i.kind == InteractionType::ApplicationCommandAutocomplete => {
                // Only `/timezone zone:` autocompletes: matches of what was typed, else the zones of the user's language.
                let typed = data.options.iter().find_map(|o| match &o.value {
                    CommandOptionValue::Focused(v, _) => Some(v.as_str()),
                    _ => None,
                });
                let locales: Vec<&str> = [i.locale.as_deref(), i.guild_locale.as_deref()].into_iter().flatten().collect();
                let choices = crate::tz::suggest(typed.unwrap_or_default(), &locales)
                    .into_iter()
                    .map(|zone| CommandOptionChoice { name: crate::tz::label(&zone), name_localizations: None, value: CommandOptionChoiceValue::String(zone) })
                    .collect();
                let data = InteractionResponseData { choices: Some(choices), ..Default::default() };
                let resp = InteractionResponse { kind: InteractionResponseType::ApplicationCommandAutocompleteResult, data: Some(data) };
                client.create_response(i.id, &i.token, &resp).await?;
            }
            Some(InteractionData::ApplicationCommand(data)) => {
                let Some(cmd) = parse_slash(data) else { return Ok(()) };
                if cmd == Cmd::RefreshMembers {
                    // Discord checks the permission (MANAGE_GUILD); members arrive in MEMBER_CHUNK.
                    let text = match i.guild_id {
                        Some(g) => {
                            sender.command(&RequestGuildMembers::builder(g).query("", None))?;
                            "Reloading the member list; WhoKnows will reflect it in a few seconds."
                        }
                        None => "This command only works in servers.",
                    };
                    return self.ephemeral(i, text).await;
                }
                if cmd == Cmd::Login {
                    #[allow(deprecated)] // `label` now lives on the Label component
                    let input = TextInput {
                        custom_id: "token".into(),
                        id: None,
                        label: None,
                        max_length: Some(100),
                        min_length: Some(10),
                        placeholder: Some("User token from listenbrainz.org/settings".into()),
                        required: Some(true),
                        style: TextInputStyle::Short,
                        value: None,
                    };
                    let help = TextDisplay {
                        content: format!(
                            "Copy your **User token** from {SETTINGS}. It's stored encrypted, used once to download your history, and deleted when that's done."
                        ),
                        id: None,
                    };
                    let label = Label {
                        component: Box::new(Component::TextInput(input)),
                        description: None,
                        id: None,
                        label: "ListenBrainz token".into(),
                    };
                    let resp = ModalBuilder::new("login", "Connect to ListenBrainz", [Component::TextDisplay(help), Component::Label(label)]).build();
                    client.create_response(i.id, &i.token, &resp).await?;
                    return Ok(());
                }
                let ephemeral = matches!(cmd, Cmd::Privacy | Cmd::Remove { .. } | Cmd::Prefix(_) | Cmd::Chart { private: true, .. });
                self.defer(i, ephemeral).await?;
                let out = self.run(&ctx, cmd).await.unwrap_or_else(failed);
                client.update_response(&i.token).embeds(Some(&[out.embed])).components(Some(&out.components)).attachments(&out.files).await?;
            }
            Some(InteractionData::MessageComponent(data)) => {
                if let Some((owner, chart)) = crate::chart::from_button(&data.custom_id) {
                    if owner != ctx.discord_id {
                        return self.ephemeral(i, "Only the person who ran the command can edit this chart.").await;
                    }
                    client.create_response(i.id, &i.token, &crate::chart::modal(owner, &chart)).await?;
                    return Ok(());
                }
                let Some((owner, today, page)) = crate::hist::overview_button(&data.custom_id) else { return Ok(()) };
                if owner != ctx.discord_id {
                    return self.ephemeral(i, "Only the person who ran the command can turn its pages.").await;
                }
                let resp = InteractionResponse { kind: InteractionResponseType::DeferredUpdateMessage, data: None };
                client.create_response(i.id, &i.token, &resp).await?;
                let out = self.overview_turn(&ctx, today, page).await.unwrap_or_else(failed);
                client.update_response(&i.token).embeds(Some(&[out.embed])).components(Some(&out.components)).await?;
            }
            Some(InteractionData::ModalSubmit(data)) if data.custom_id.starts_with("chm:") => {
                let value = |id: &str| data.components.iter().find_map(|c| modal_values(c, id)).unwrap_or_default();
                let Some((owner, chart)) = crate::chart::from_modal(&data.custom_id, value) else { return Ok(()) };
                if owner != ctx.discord_id {
                    return self.ephemeral(i, "Only the person who ran the command can edit this chart.").await;
                }
                let resp = InteractionResponse { kind: InteractionResponseType::DeferredUpdateMessage, data: None };
                client.create_response(i.id, &i.token, &resp).await?;
                // An error keeps the Edit button, so a hiccup doesn't end the chart.
                let out = self.chart(&ctx, &chart).await.unwrap_or_else(|e| Out { components: vec![crate::chart::edit_button(owner, &chart)], ..failed(e) });
                // Keeping no old attachments drops the previous picture, also when the new reply has none.
                let embeds = [out.embed];
                let update = client.update_response(&i.token).embeds(Some(&embeds)).components(Some(&out.components));
                update.keep_attachment_ids(&[]).attachments(&out.files).await?;
            }
            Some(InteractionData::ModalSubmit(data)) if data.custom_id == "login" => {
                self.defer(i, true).await?;
                let token = data.components.iter().find_map(|c| modal_values(c, "token")).and_then(|v| v.into_iter().next()).unwrap_or_default();
                let out = self.login(&ctx, token.trim(), &i.token).await.unwrap_or_else(error_reply);
                client.update_response(&i.token).embeds(Some(&[out])).await?;
            }
            _ => {}
        }
        Ok(())
    }

    /// A reply only the person who pressed or typed sees.
    async fn ephemeral(&self, i: &Interaction, text: &str) -> Result<()> {
        let resp = ChannelMessageBuilder::new().embeds([reply(text)]).flags(MessageFlags::EPHEMERAL).build();
        self.http.interaction(self.app_id).create_response(i.id, &i.token, &resp).await?;
        Ok(())
    }

    async fn defer(&self, i: &Interaction, ephemeral: bool) -> Result<()> {
        let data = ephemeral.then(|| InteractionResponseData { flags: Some(MessageFlags::EPHEMERAL), ..Default::default() });
        let resp = InteractionResponse { kind: InteractionResponseType::DeferredChannelMessageWithSource, data };
        self.http.interaction(self.app_id).create_response(i.id, &i.token, &resp).await?;
        Ok(())
    }

    async fn login(&self, ctx: &Ctx, token: &str, interaction_token: &str) -> Result<Embed> {
        // A running index would write the old account's data over the new one.
        if let Some(old) = self.db().user_by_discord(ctx.discord_id).await?
            && self.ingest.is_busy(old.id)
        {
            return Ok(reply("Your history is still downloading. Try again when it finishes."));
        }
        let Some(lb_name) = self.ingest.lb.validate_token(token).await? else {
            return Ok(reply(format!("Invalid token. Paste the **User token** from {SETTINGS}.")));
        };
        let user_id = self.db().upsert_user(ctx.discord_id, &lb_name, token).await?;
        if let Some(g) = ctx.guild {
            self.db().add_members(g, &[(ctx.discord_id, ctx.name.clone())]).await?;
        }
        // Each /login downloads the full history once; the token is deleted when that finishes.
        self.notify_when_indexed(user_id, ctx.discord_id, interaction_token.to_owned());
        let mut text = format!(
            "Connected as **{lb_name}**. Your full history is downloading as a ListenBrainz export. Large accounts can take a few minutes. You'll get a message when it's done and your token has been deleted."
        );
        if ctx.guild.is_none() {
            // Users who connect in a DM join server lists with their first command there, or via /refreshmembers.
            text.push_str(" To show up in a server's WhoKnows, use any command in that server.");
        }
        Ok(reply(text))
    }

    /// Indexes in the background and tells the user when it's done: as a reply to their /login while Discord
    /// still accepts one (15 minutes), else in a DM. The raw Discord id lives only in this task, never in the database.
    fn notify_when_indexed(&self, user_id: i32, discord_id: i64, interaction_token: String) {
        let (ingest, http, app_id) = (self.ingest.clone(), self.http.clone(), self.app_id);
        tokio::spawn(async move {
            let started = std::time::Instant::now();
            let text = match ingest.index_now(user_id).await {
                Ok(Some(n)) => format!(
                    "Your history is downloaded: **{n}** listens. Your ListenBrainz token has been deleted from the bot; from now on it only reads your public listens."
                ),
                Ok(None) => return,
                Err(_) => "Downloading your history failed. Run `/login` again to retry.".to_owned(),
            };
            let sent = if started.elapsed() < std::time::Duration::from_secs(14 * 60) {
                let client = http.interaction(app_id);
                let embeds = [reply(text)];
                client.create_followup(&interaction_token).flags(MessageFlags::EPHEMERAL).embeds(&embeds).await.map(drop).map_err(anyhow::Error::from)
            } else {
                async {
                    let channel = http.create_private_channel(Id::new(discord_id as u64)).await?.model().await?;
                    http.create_message(channel.id).embeds(&[reply(text)]).await?;
                    anyhow::Ok(())
                }
                .await
            };
            if let Err(e) = sent {
                tracing::warn!(user_id, error = ?e, "couldn't tell the user their index finished");
            }
        });
    }

    async fn featured_block(&self, ctx: &Ctx) -> Result<Embed> {
        if self.owner != Some(ctx.discord_id) {
            return Ok(reply("Only the bot owner can do this."));
        }
        Ok(reply(escape(&self.featured.block(&self.http, &self.ingest).await?)))
    }

    async fn run(&self, ctx: &Ctx, cmd: Cmd) -> Result<Out> {
        match cmd {
            Cmd::Hist { cmd, kind, arg } => match self.registered(ctx).await? {
                Some(user) => self.hist(ctx, &user, cmd, kind, &arg).await,
                None => Ok(login_first().into()),
            },
            Cmd::Chart { mut chart, artist, .. } => {
                if let Some(name) = artist {
                    match self.db().find(Kind::Artist, &name, None).await? {
                        Some((id, _)) => chart.artist = Some(id),
                        None => return Ok(reply(format!("No one on the bot has listened to **{}**.", escape(&name))).into()),
                    }
                }
                self.chart(ctx, &chart).await
            }
            cmd => Ok(self.run_embed(ctx, cmd).await?.into()),
        }
    }

    /// The caller's account, after adding them to this server's member list. `None` = not logged in.
    pub(crate) async fn registered(&self, ctx: &Ctx) -> Result<Option<crate::db::User>> {
        let user = self.db().user_by_discord(ctx.discord_id).await?;
        if let (Some(_), Some(g)) = (&user, ctx.guild) {
            self.db().add_members(g, &[(ctx.discord_id, ctx.name.clone())]).await?;
        }
        Ok(user)
    }

    async fn run_embed(&self, ctx: &Ctx, cmd: Cmd) -> Result<Embed> {
        if cmd == Cmd::Help {
            return Ok(reply(HELP));
        }
        if let Cmd::Prefix(Some(p)) = &cmd {
            let Some(g) = ctx.guild else { return Ok(reply("This command only works in servers.")) };
            if p.chars().count() > 5 || p.contains('`') {
                return Ok(reply("The prefix must be at most 5 characters and can't contain `."));
            }
            self.db().set_prefix(g, p).await?;
            self.prefixes.write().unwrap().insert(g, p.clone());
            return Ok(reply(format!("The prefix is now `{p}`.")));
        }

        let Some(user) = self.registered(ctx).await? else { return Ok(login_first()) };
        let indexing_note = (!user.indexed).then_some("Your history is still downloading, so counts may be incomplete.");

        match cmd {
            Cmd::Timezone(None) => {
                let current = crate::tz::Tz::new(user.tz.as_deref());
                let text = if current.is_set() {
                    format!("Your days and hours are counted in **{}**. Change it with `/timezone`.", crate::tz::label(current.name()))
                } else {
                    "Your days and hours are counted in **UTC**. Set your time zone with `/timezone`, which suggests yours, or `,timezone Istanbul`.".to_owned()
                };
                Ok(reply(text))
            }
            Cmd::Timezone(Some(query)) => match crate::tz::find(&query) {
                Some(zone) => {
                    // UTC is the default, so it is stored as nothing.
                    self.db().set_tz(user.id, (zone != "UTC").then_some(zone.as_str())).await?;
                    Ok(reply(format!("Your time zone is now **{}**.", crate::tz::label(&zone))))
                }
                None => {
                    let close: Vec<String> = crate::tz::suggest(&query, &[]).into_iter().take(5).map(|z| format!("`{z}`")).collect();
                    let hint = if close.is_empty() { "Try a city, e.g. `Istanbul` or `New York`.".to_owned() } else { format!("Did you mean {}?", close.join(", ")) };
                    Ok(reply(format!("I don't know that time zone. {hint}")))
                }
            },
            Cmd::Update => {
                if self.ingest.is_busy(user.id) {
                    return Ok(reply("Your history is downloading now. It will be up to date when it finishes."));
                }
                if !user.indexed {
                    // The previous index stopped on an error; its token is kept until an index finishes.
                    if user.lb_token.is_none() {
                        return Ok(reply("Run `/login` again to download your history."));
                    }
                    self.ingest.spawn_index(user.id);
                    return Ok(reply("Restarted the download of your history."));
                }
                Ok(match self.ingest.sync(&user).await? {
                    (n, true) => reply(format!(
                        "Updated: {n} new listens. Your older history on ListenBrainz changed (an import or a deletion), so some counts are out of date. Run `/login` again to download it fresh."
                    )),
                    (n, false) => reply(format!("Updated: {n} new listens.")),
                })
            }
            Cmd::Np => {
                let (now, updated) = tokio::join!(self.ingest.lb.now_or_last(&user.lb_name), self.ingest.update(&user, crate::lb::Prio::Command));
                if let Err(e) = updated {
                    tracing::warn!(error = ?e, "update during np failed");
                }
                let Some((t, playing, mapping)) = now? else { return Ok(reply("No listens yet.")) };
                let plays = async {
                    Ok::<_, anyhow::Error>(match self.db().find(Kind::Artist, &t.artist_name, None).await? {
                        Some((id, _)) => self.db().plays(Kind::Artist, id, user.id).await?,
                        None => 0,
                    })
                };
                let art = self.art(Kind::Track, &t.artist_name, Some(&t.track_name), t.release_name.as_deref(), mapping);
                let (artist_plays, art) = tokio::join!(plays, art);
                let artist_plays = artist_plays?;
                let track = match &art.url {
                    Some(url) => format!("[{}]({url})", link_text(&t.track_name)),
                    None => escape(&t.track_name),
                };
                let mut desc = format!("**{track}**\n**{}**", escape(&t.artist_name));
                if let Some(album) = t.release_name.as_deref().filter(|a| !a.is_empty()) {
                    desc.push_str(&format!(" • *{}*", escape(album)));
                }
                let author = format!("{} - {}", if playing { "Now playing" } else { "Last played" }, ctx.name);
                let embed = EmbedBuilder::new()
                    .color(COLOR)
                    .author(EmbedAuthorBuilder::new(author).url(crate::lb::profile_url(&user.lb_name)))
                    .description(desc)
                    .footer(EmbedFooterBuilder::new(format!("{artist_plays} artist plays")));
                Ok(with_art(embed, art))
            }
            Cmd::Wk { kind, global, query } => {
                if !global && ctx.guild.is_none() {
                    return Ok(reply("Server WhoKnows only works in servers. Use `gw` for global."));
                }
                let (artist, name, now) = match self.subject(&user, kind, query).await? {
                    Ok(s) => s,
                    Err(e) => return Ok(e),
                };
                if let Err(e) = self.ingest.update(&user, crate::lb::Prio::Command).await {
                    tracing::warn!(error = ?e, "update during wk failed");
                }
                let Some((id, title)) = self.db().find(kind, &artist, name.as_deref()).await? else {
                    let mut text = format!("No one on the bot has listened to **{}**.", escape(&name.unwrap_or(artist)));
                    if let Some(note) = indexing_note {
                        text = format!("{text} {note}");
                    }
                    return Ok(reply(text));
                };
                let guild = if global { None } else { ctx.guild };
                let rows = self.db().who_knows(kind, id, guild).await?;
                let (track, album) = match kind {
                    Kind::Artist => (now.0, None),
                    Kind::Album => (now.0, name.as_deref()),
                    Kind::Track => (name.clone(), None),
                };
                let art = self.art(kind, &artist, track.as_deref(), album, now.1);
                let own = self.db().plays(kind, id, user.id);
                let (mut art, own) = tokio::join!(art, own);
                let tags = art.tags.take();
                let embed = self.who_knows_embed(&user, kind, own?, global, ctx, &title, &rows, tags, indexing_note);
                Ok(with_art(embed, art))
            }
            Cmd::Privacy => {
                let hidden = self.db().toggle_private(user.id).await?;
                Ok(reply(if hidden {
                    "You are now hidden from global WhoKnows."
                } else {
                    "You are now visible in global WhoKnows."
                }))
            }
            Cmd::Remove { confirm: false } => {
                Ok(reply("All your data will be deleted. To confirm, run `/remove confirm:True` or `,remove confirm`."))
            }
            Cmd::Remove { confirm: true } => {
                if self.ingest.is_busy(user.id) {
                    return Ok(reply("Your data can't be deleted while your history is downloading. Try again shortly."));
                }
                self.db().delete_user(user.id).await?;
                Ok(reply("Your account and all your data were deleted."))
            }
            Cmd::Login | Cmd::Help | Cmd::Prefix(_) | Cmd::RefreshMembers | Cmd::FeaturedBlock | Cmd::Hist { .. } | Cmd::Chart { .. } => {
                unreachable!("handled above")
            }
        }
    }

    /// What a command is about: the typed `Artist` or `Artist - Name`, else the playing track's artist, album or track.
    /// Also the playing track's name and mapping, for the cover (empty for a typed query). `Err` is a reply for the user.
    pub(crate) async fn subject(&self, user: &crate::db::User, kind: Kind, query: Option<String>) -> Result<Result<Subject, Embed>> {
        Ok(Ok(match query {
            Some(q) => match kind {
                Kind::Artist => (q, None, (None, Value::Null)),
                _ => match q.split_once(" - ") {
                    Some((a, n)) => (a.trim().to_owned(), Some(n.trim().to_owned()), (None, Value::Null)),
                    None => return Ok(Err(reply("Format: `Artist - Name`"))),
                },
            },
            None => {
                let Some((t, _, mapping)) = self.ingest.lb.now_or_last(&user.lb_name).await? else {
                    return Ok(Err(reply("No listens yet.")));
                };
                let name = match kind {
                    Kind::Artist => None,
                    Kind::Album => match t.release_name.filter(|a| !a.is_empty()) {
                        Some(a) => Some(a),
                        None => return Ok(Err(reply("The playing track has no album."))),
                    },
                    Kind::Track => Some(t.track_name.clone()),
                };
                (t.artist_name, name, (Some(t.track_name), mapping))
            }
        }))
    }

    #[allow(clippy::too_many_arguments)]
    fn who_knows_embed(
        &self,
        user: &crate::db::User,
        kind: Kind,
        own: i32,
        global: bool,
        ctx: &Ctx,
        title: &str,
        rows: &[crate::db::WkRow],
        tags: Option<String>,
        note: Option<&str>,
    ) -> EmbedBuilder {
        let row = |rank: String, name: &str, lb_name: &str, plays: i32, me: bool| {
            let link = format!("[{}]({})", link_text(name), crate::lb::profile_url(lb_name));
            if me { format!("**{rank} {link}** - **{plays}** plays") } else { format!("{rank} {link} - **{plays}** plays") }
        };
        let crown = has_crown(kind, global, &rows.iter().map(|r| r.plays).collect::<Vec<_>>());
        let mut lines: Vec<String> = rows
            .iter()
            .enumerate()
            .map(|(n, r)| {
                // `1.` at the start of a line is a Markdown list item, which Discord draws as its own block.
                let rank = if n == 0 && crown { "👑".to_owned() } else { format!("{}\\.", n + 1) };
                row(rank, &r.name, &r.lb_name, r.plays, r.discord_hash == user.discord_hash)
            })
            .collect();
        if own > 0 && !rows.iter().any(|r| r.discord_hash == user.discord_hash) {
            // Rows use the server nickname in a server and the LB name globally.
            let name = if global { &user.lb_name } else { &ctx.name };
            lines.push(row("…".into(), name, &user.lb_name, own, true));
        }
        if lines.is_empty() {
            lines.push("No one has listened yet.".into());
        }
        let (listeners, total) = rows.first().map_or((0, 0), |r| (r.listeners, r.total));
        let kind_name = match kind { Kind::Artist => "Artist", Kind::Album => "Album", Kind::Track => "Track" };
        let mut footer = format!("{kind_name} - {listeners} listeners - {total} plays - {} avg", total / listeners.max(1));
        if let Some(t) = tags {
            footer = format!("{t}\n{footer}");
        }
        if let Some(n) = note {
            footer = format!("{footer}\n{n}");
        }
        let heading = match ctx.guild.filter(|_| !global) {
            Some(g) => match self.guild_names.read().unwrap().get(&g) {
                Some(server) => format!("{title} in {server}"),
                None => format!("{title} in this server"),
            },
            None => format!("{title} globally"),
        };
        EmbedBuilder::new()
            .color(COLOR)
            // Discord allows at most 256 characters in an embed title; longer ones stop the message from sending.
            .title(heading.chars().take(256).collect::<String>())
            .description(lines.join("\n"))
            .footer(EmbedFooterBuilder::new(footer))
    }

    /// Card image and tags, from the cache or `fetch_art`. An error doesn't break the card and isn't cached.
    pub(crate) async fn art(&self, kind: Kind, artist: &str, track: Option<&str>, album: Option<&str>, mapping: Value) -> Art {
        let name = match kind {
            Kind::Artist => "",
            Kind::Album => album.unwrap_or_default(),
            Kind::Track => track.unwrap_or_default(),
        };
        let (key, name_key) = art_keys(kind, artist, name, &mapping);
        let cached = cache_get(&self.art_cache.lock().unwrap(), &key);
        let mut art = match cached {
            Some(art) => art,
            None => self.fetch_art_cached(kind, artist, track, album, mapping, key, name_key).await,
        };
        // Without an id the title still links somewhere: a ListenBrainz search.
        if art.url.is_none() {
            let (search_type, term) = match kind {
                Kind::Artist => ("artist", artist),
                Kind::Album => ("album", name),
                Kind::Track => ("track", name),
            };
            art.url = Some(crate::lb::search_url(search_type, term));
        }
        art
    }

    #[allow(clippy::too_many_arguments)]
    async fn fetch_art_cached(
        &self,
        kind: Kind,
        artist: &str,
        track: Option<&str>,
        album: Option<&str>,
        mapping: Value,
        key: ArtKey,
        name_key: ArtKey,
    ) -> Art {
        match self.fetch_art(kind, artist, track, album, mapping).await {
            Ok((art, complete)) => {
                if complete {
                    let mut cache = self.art_cache.lock().unwrap();
                    let now = std::time::Instant::now();
                    if key != name_key {
                        // Also overwrite the (possibly wrong) result of the name search with the right one.
                        cache_put(&mut cache, name_key, art.clone(), now);
                    }
                    cache_put(&mut cache, key, art.clone(), now);
                }
                art
            }
            Err(e) => {
                tracing::warn!(error = ?e, "art lookup failed");
                Art::default()
            }
        }
    }

    /// Artist/album from MusicBrainz (artist image: the Apple Music photo, else the cover of their most popular album), track from the LB mapping.
    /// When the playing track's mapping has the right artist/album id, it is used before a name search.
    async fn fetch_art(
        &self,
        kind: Kind,
        artist: &str,
        track: Option<&str>,
        album: Option<&str>,
        mut mapping: Value,
    ) -> Result<(Art, bool)> {
        let lb = &self.ingest.lb;
        Ok(match kind {
            Kind::Artist => {
                // An id stored from listens or an earlier lookup (see `meta`) is as sure as a mapping's and costs no request.
                if mapping["artist_mbids"].get(0).is_none()
                    && let Some(m) = self.stored_artist_mbid(artist).await
                {
                    mapping["artist_mbids"] = serde_json::json!([m]);
                }
                // Without a mapping (playing-now) it comes from a lookup: the first id is the main artist ("A feat. B" → A).
                if mapping["artist_mbids"].get(0).is_none()
                    && let Some(t) = track
                {
                    match lb.lookup(artist, t, album).await {
                        Ok(m) => mapping = m,
                        Err(e) => tracing::warn!(error = ?e, "artist lookup failed"),
                    }
                }
                // A search can pick the wrong one of two artists with the same name; the playing track's id is certain. Tags only come from the same artist.
                let known = mapping["artist_mbids"].get(0).and_then(Value::as_str);
                let (searched, photo) = match known {
                    // With a known id, read the record directly: tags and the right Apple Music page (a name search can pick a namesake).
                    Some(mbid) => {
                        let info = lb.mb_artist(mbid).await;
                        let pages = info.as_ref().map(crate::lb::photo_pages).unwrap_or_default();
                        // Apple, else Tidal; if one fails, try the next.
                        let mut photo = Ok(None);
                        for page in &pages {
                            photo = lb.page_photo(page).await;
                            if matches!(photo, Ok(Some(_))) {
                                break;
                            }
                        }
                        (info.map(Some), photo)
                    }
                    None => {
                        let q = format!("artist:{}", crate::lb::phrase(artist));
                        let (mut searched, mut photo) = tokio::join!(lb.mb_search("artist", &q), lb.itunes_artist_image(artist));
                        // If "A & B" or "A feat. B" isn't found, try the main artist; names like "Simon & Garfunkel" are searched in full first.
                        // If the full name exists on MusicBrainz it is one artist ("Florence and the Machine") and isn't split; otherwise
                        // iTunes' different spelling ("+") could return someone else's photo, such as "Florence".
                        let main = crate::lb::main_artist(artist);
                        if main != artist && matches!(searched, Ok(None)) {
                            let q = format!("artist:{}", crate::lb::phrase(main));
                            let retry_photo = async { if matches!(photo, Ok(None)) { Some(lb.itunes_artist_image(main).await) } else { None } };
                            let (s2, p2) = tokio::join!(lb.mb_search("artist", &q), retry_photo);
                            searched = s2;
                            photo = p2.unwrap_or(photo);
                        }
                        (searched, photo)
                    }
                };
                // Without a photo, or when Apple/Tidal fail, fall back to the album cover. This error doesn't block caching:
                // retrying on every command while blocked makes the block last longer; the photo arrives the next day.
                let photo = photo.inspect_err(|e| tracing::warn!(error = ?e, "artist photo failed")).ok().flatten();
                // If MusicBrainz is down, carry on with what we have (known id, iTunes photo); an incomplete result isn't cached.
                let mut complete = searched.is_ok();
                let a = searched.inspect_err(|e| tracing::warn!(error = ?e, "musicbrainz artist failed")).ok().flatten();
                let a = a.filter(|a| known.is_none_or(|k| a["id"].as_str() == Some(k)));
                let mbid = known.or(a.as_ref().and_then(|a| a["id"].as_str()));
                let cover = match (photo, mbid) {
                    (Some(photo), _) => Some(photo),
                    (None, Some(mbid)) => lb.artist_cover(mbid).await.unwrap_or_else(|e| {
                        tracing::warn!(error = ?e, "artist cover failed");
                        complete = false;
                        None
                    }),
                    (None, None) => None,
                };
                let url = mbid.map(|m| crate::lb::page_url("artist", m));
                (Art { cover, tags: a.as_ref().and_then(crate::lb::tags), url }, complete)
            }
            Kind::Album => {
                let q = format!("releasegroup:{} AND artist:{}", crate::lb::phrase(album.unwrap_or_default()), crate::lb::phrase(artist));
                // If the name is spelled differently (Deluxe etc.) the search is empty; the playing track's cover is still this album's.
                // Without a playing track (typed query) don't cache an empty result, or it blocks the listener's cover all day.
                let searched = lb.mb_search("release-group", &q).await;
                // If MusicBrainz is down, carry on with the playing track's cover; an incomplete result isn't cached.
                let complete = searched.is_ok() && !mapping.is_null();
                let Some(rg) = searched.inspect_err(|e| tracing::warn!(error = ?e, "musicbrainz album failed")).ok().flatten() else {
                    let url = mapping["release_group_mbid"].as_str().map(|m| crate::lb::page_url("album", m));
                    return Ok((Art { cover: crate::lb::cover_url(&mapping), tags: None, url }, complete));
                };
                // If the playing track is from this album its mapping's cover surely exists; otherwise the release-group front cover.
                let same = rg["id"].as_str().is_some_and(|id| mapping["release_group_mbid"].as_str() == Some(id));
                let cover = same.then(|| crate::lb::cover_url(&mapping)).flatten().or_else(|| {
                    rg["id"].as_str().map(|id| format!("https://coverartarchive.org/release-group/{id}/front-1200"))
                });
                let url = rg["id"].as_str().map(|m| crate::lb::page_url("album", m));
                (Art { cover, tags: crate::lb::tags(&rg), url }, true)
            }
            Kind::Track => {
                if mapping.get("recording_mbid").is_none() {
                    let Some(t) = track else { return Ok((Art::default(), true)) };
                    mapping = match self.stored_track_mbid(artist, t).await {
                        Some(m) => serde_json::json!({ "recording_mbid": m }),
                        None => lb.lookup(artist, t, album).await?,
                    };
                }
                let Some(rec) = mapping["recording_mbid"].as_str() else { return Ok((Art::default(), true)) };
                let url = Some(crate::lb::page_url("track", rec));
                // The cover comes from the same release as the track page; if that request fails, use the mapping's and don't cache.
                match lb.recording_release(rec).await {
                    // For a merged (old) id the service returns nothing while the page redirects: use the mapping's cover then.
                    Ok(release) => (Art { cover: crate::lb::cover_url(&release).or_else(|| crate::lb::cover_url(&mapping)), tags: None, url }, true),
                    Err(e) => {
                        tracing::warn!(error = ?e, "recording release failed");
                        (Art { cover: crate::lb::cover_url(&mapping), tags: None, url }, false)
                    }
                }
            }
        })
    }
}

/// fmbot's crown rule: server artist WhoKnows only, the leader needs at least 30 plays (fmbot's default
/// `DefaultPlaysForCrown`). fmbot also gives it to a single listener; with no one to compete a crown means nothing, so at least 2 listeners.
const CROWN_MIN_PLAYS: i32 = 30;

fn has_crown(kind: Kind, global: bool, plays: &[i32]) -> bool {
    kind == Kind::Artist && !global && plays.len() >= 2 && plays[0] >= CROWN_MIN_PLAYS
}

/// (read key, name key). The mapping's artist id is more certain than the name: when present it is the read key
/// (third field "mbid" so it can't collide with a name key); otherwise both are the same.
pub(crate) fn art_keys(kind: Kind, artist: &str, name: &str, mapping: &Value) -> (ArtKey, ArtKey) {
    let name_key = (kind, artist.to_lowercase(), name.to_lowercase());
    let mbid = (kind == Kind::Artist).then(|| mapping["artist_mbids"].get(0).and_then(Value::as_str)).flatten();
    (mbid.map_or_else(|| name_key.clone(), |m| (kind, m.to_owned(), "mbid".into())), name_key)
}

pub(crate) fn cache_get<K: Eq + std::hash::Hash, V: Clone>(cache: &HashMap<K, (std::time::Instant, V)>, key: &K) -> Option<V> {
    cache.get(key).filter(|(at, _)| at.elapsed() < ART_TTL).map(|(_, art)| art.clone())
}

pub(crate) fn cache_put<K: Eq + std::hash::Hash, V>(cache: &mut HashMap<K, (std::time::Instant, V)>, key: K, art: V, at: std::time::Instant) {
    if cache.len() >= ART_CACHE_MAX {
        cache.retain(|_, (at, _)| at.elapsed() < ART_TTL);
        if cache.len() >= ART_CACHE_MAX {
            // shortcut: clears everything when full; switch to an LRU if the hit rate looks low.
            cache.clear();
        }
    }
    cache.insert(key, (at, art));
}

/// Uses the cover as the thumbnail and the page link as the title link (a card without a title shows no link).
fn with_art(mut embed: EmbedBuilder, art: Art) -> Embed {
    if let Some(img) = art.cover.and_then(|u| ImageSource::url(u).ok()) {
        embed = embed.thumbnail(img);
    }
    if let Some(url) = art.url {
        embed = embed.url(url);
    }
    embed.build()
}

/// The answer of the form field `id`: a text, or the picked options.
fn modal_values(c: &ModalInteractionComponent, id: &str) -> Option<Vec<String>> {
    match c {
        ModalInteractionComponent::TextInput(t) if t.custom_id == id => Some(vec![t.value.clone()]),
        ModalInteractionComponent::StringSelect(s) if s.custom_id == id => Some(s.values.clone()),
        ModalInteractionComponent::CheckboxGroup(g) if g.custom_id == id => Some(g.values.clone()),
        ModalInteractionComponent::Label(l) => modal_values(&l.component, id),
        ModalInteractionComponent::ActionRow(r) => r.components.iter().find_map(|c| modal_values(c, id)),
        _ => None,
    }
}

/// Text of a `[text](url)` link. Discord shows backslash escapes there as they are ("Vignette \(Live\)"),
/// so nothing is escaped; square brackets, which would end the link early, become round ones.
fn link_text(s: &str) -> String {
    s.replace('[', "(").replace(']', ")")
}

pub(crate) fn escape(s: &str) -> String {
    s.chars().fold(String::with_capacity(s.len()), |mut out, c| {
        if "*_`~|>[]()\\".contains(c) {
            out.push('\\');
        }
        out.push(c);
        out
    })
}

fn error_reply(e: anyhow::Error) -> Embed {
    tracing::error!(error = ?e, "command failed");
    reply("Something went wrong. Try again in a bit.")
}

const HELP: &str = "**Commands** (default prefix `,`; each one also exists as a slash command)
`/login`: connect your ListenBrainz account (slash only, so the token stays private)
`,np`: your current track
`,w` `,wa` `,wt`: who listens to an artist / album / track in this server (`Artist - Name`)
`,gw` `,gwa` `,gwt`: the same across the bot
`,o`: last days · `,streak` `,streaks` · `,gaps` · `,discoveries` · `,dd` `,ll`: first / last listen
`,pace` `,apc`: when you'll hit a play count · `,ms`: your nth listen
`,year` `,recap` `,receipt` `,iceberg` · `,taste @user` · `,aff`: who listens like you here
`,od`: overdue favorites · `,comebacks` · `,movers` · `,otd`: on this day · `,obsessions`
`,growth` `,decades` `,clock`: charts · `,lt`: listening time · `,hidden` · `,ug`: how underground · `,sice` · `,unlinked`
`,timezone`: the time zone your days are counted in
`,c` `,ac` `,tc`: album / artist / track chart, e.g. `,c 4x4 month nt pc s`, with an Edit button
`,update`: fetch your latest listens
`,privacy`: show or hide yourself in global lists
`,remove`: delete your account and data
`/prefix`: change the server prefix
`/refreshmembers`: reload the server member list";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_aliases() {
        assert!(matches!(parse_text("wa", ""), Some(Cmd::Wk { kind: Kind::Album, global: false, query: None })));
        assert!(matches!(parse_text("gwt", "a - b"), Some(Cmd::Wk { kind: Kind::Track, global: true, query: Some(q) }) if q == "a - b"));
        assert_eq!(parse_text("remove", "confirm"), Some(Cmd::Remove { confirm: true }));
        assert_eq!(parse_text("remove", ""), Some(Cmd::Remove { confirm: false }));
        assert_eq!(parse_text("hello", ""), None);
        assert_eq!(parse_text("featured", "block"), Some(Cmd::FeaturedBlock));
        assert_eq!(parse_text("featured", ""), None);
        assert_eq!(parse_text("fm", ""), None, "renamed to np");
        assert_eq!(parse_text("np", ""), Some(Cmd::Np));
        assert_eq!(parse_text("dda", "a - b"), Some(Cmd::Hist { cmd: Hc::DiscoveryDate, kind: Kind::Album, arg: "a - b".into() }));
        assert!(matches!(parse_text("o", "3"), Some(Cmd::Hist { cmd: Hc::Overview, .. })));
        assert_eq!(parse_text("tz", "New York"), Some(Cmd::Timezone(Some("New York".into()))));
        assert_eq!(parse_text("timezone", ""), Some(Cmd::Timezone(None)));
        assert!(matches!(parse_text("od", ""), Some(Cmd::Hist { cmd: Hc::Overdue, kind: Kind::Track, .. })));
        assert!(matches!(parse_text("sice", ""), Some(Cmd::Hist { cmd: Hc::ServerIceberg, .. })));
        assert!(matches!(parse_text("c", "4x4 Radiohead"), Some(Cmd::Chart { chart, artist: Some(a), private: false }) if chart.cols == 4 && a == "Radiohead"));
        assert!(matches!(parse_text("ac", ""), Some(Cmd::Chart { chart, artist: None, .. }) if chart.kind == Kind::Artist));
        assert!(matches!(parse_text("tc", ""), Some(Cmd::Chart { chart, .. }) if chart.kind == Kind::Track));
        assert!(matches!(parse_text("aoty", "2020"), Some(Cmd::Chart { chart, artist: None, .. }) if chart.year == Some(2020)));
    }

    #[test]
    fn crown_rules() {
        assert!(has_crown(Kind::Artist, false, &[30, 5]));
        assert!(!has_crown(Kind::Artist, false, &[425]), "single listener");
        assert!(!has_crown(Kind::Artist, false, &[29, 5]), "under 30 plays");
        assert!(!has_crown(Kind::Artist, true, &[425, 5]), "global");
        assert!(!has_crown(Kind::Album, false, &[425, 5]), "album");
        assert!(!has_crown(Kind::Artist, false, &[]));
    }

    #[test]
    fn art_keys_prefer_mbid_for_artists() {
        let mapped = serde_json::json!({"artist_mbids": ["m1"]});
        let (key, name) = art_keys(Kind::Artist, "Eden", "", &mapped);
        assert_eq!(key, (Kind::Artist, "m1".into(), "mbid".into()));
        assert_eq!(name, (Kind::Artist, "eden".into(), String::new()));
        let (key, name) = art_keys(Kind::Artist, "Eden", "", &Value::Null);
        assert_eq!(key, name);
        // Albums and tracks don't use the id key.
        let (key, name) = art_keys(Kind::Album, "Eden", "Vertigo", &mapped);
        assert_eq!(key, name);
    }

    #[test]
    fn art_cache_ttl_and_cap() {
        let now = std::time::Instant::now();
        let old = now - ART_TTL - std::time::Duration::from_secs(1);
        let key = |i: usize| (Kind::Artist, format!("a{i}"), String::new());
        let art = Art { cover: Some("u".into()), ..Art::default() };
        let mut c = HashMap::new();
        cache_put(&mut c, key(0), art.clone(), old);
        assert_eq!(cache_get(&c, &key(0)), None, "expired entry is not served");
        cache_put(&mut c, key(1), art.clone(), now);
        assert_eq!(cache_get(&c, &key(1)), Some(art.clone()));
        // When full, expired entries go first and fresh ones stay.
        for i in 2..ART_CACHE_MAX {
            cache_put(&mut c, key(i), art.clone(), now);
        }
        cache_put(&mut c, key(ART_CACHE_MAX), art.clone(), now);
        assert_eq!(c.len(), ART_CACHE_MAX);
        assert!(!c.contains_key(&key(0)) && c.contains_key(&key(1)));
        // When full of fresh entries, it is cleared.
        cache_put(&mut c, key(ART_CACHE_MAX + 1), art.clone(), now);
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn slash_list_and_escape() {
        assert_eq!(slash_commands().len(), 45);
        assert_eq!(escape("a*b_c"), "a\\*b\\_c");
        assert_eq!(link_text("Vignette (Live In Mexico City)"), "Vignette (Live In Mexico City)");
        assert_eq!(link_text("Song [Remix]"), "Song (Remix)");
    }
}
