//! Discord side: prefix and slash commands are parsed into the same `Cmd` and run by `run`.

use std::{collections::HashMap, sync::{Arc, RwLock}};

use anyhow::Result;
use serde_json::Value;
use twilight_gateway::{Event, MessageSender};
use twilight_http::Client;
use twilight_model::{
    application::{
        command::{Command, CommandType},
        interaction::{
            Interaction, InteractionData,
            application_command::{CommandData, CommandOptionValue},
            modal::ModalInteractionComponent,
        },
    },
    channel::{
        Message,
        message::{
            Component, Embed, MessageFlags,
            component::{Label, TextInput, TextInputStyle},
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
    command::{BooleanBuilder, CommandBuilder, StringBuilder},
    embed::{EmbedAuthorBuilder, EmbedBuilder, EmbedFooterBuilder, ImageSource},
};

use crate::{db::{Db, Kind}, ingest::Ingest};

const DEFAULT_PREFIX: &str = ".";
const COLOR: u32 = 0x353070;

pub struct Bot {
    pub http: Arc<Client>,
    pub app_id: Id<ApplicationMarker>,
    pub ingest: Arc<Ingest>,
    pub prefixes: RwLock<HashMap<i64, String>>,
    /// For the WhoKnows title; filled by GUILD_CREATE.
    pub guild_names: RwLock<HashMap<i64, String>>,
    /// Card image/tag cache, so popular artists don't fill the MusicBrainz queue.
    pub art_cache: std::sync::Mutex<HashMap<ArtKey, (std::time::Instant, Art)>>,
}

/// (kind, lowercase artist, lowercase album/track)
pub type ArtKey = (Kind, String, String);
/// Card extras: cover, tags and the entity's ListenBrainz page.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Art {
    pub cover: Option<String>,
    pub tags: Option<String>,
    pub url: Option<String>,
}
const ART_TTL: std::time::Duration = std::time::Duration::from_secs(24 * 3600);
const ART_CACHE_MAX: usize = 10_000;

#[derive(Debug, PartialEq)]
enum Cmd {
    Login,
    Update,
    Fm,
    Wk { kind: Kind, global: bool, query: Option<String> },
    Prefix(Option<String>),
    Privacy,
    Remove { confirm: bool },
    Help,
    RefreshMembers,
}

struct Ctx {
    discord_id: i64,
    guild: Option<i64>,
    name: String,
}

fn reply(text: impl Into<String>) -> Embed {
    EmbedBuilder::new().color(COLOR).description(text).build()
}

/// fmbot's text aliases.
fn parse_text(word: &str, rest: &str) -> Option<Cmd> {
    let query = (!rest.is_empty()).then(|| rest.to_owned());
    let wk = |kind, global| Some(Cmd::Wk { kind, global, query: query.clone() });
    match word {
        "login" | "set" | "setusername" | "connect" => Some(Cmd::Login),
        "update" | "u" => Some(Cmd::Update),
        "fm" | "np" | "nowplaying" => Some(Cmd::Fm),
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
        _ => None,
    }
}

pub fn slash_commands() -> Vec<Command> {
    let cmd = |name: &str, desc: &str| CommandBuilder::new(name, desc, CommandType::ChatInput);
    let query = |desc: &str| StringBuilder::new("query", desc).required(false);
    vec![
        cmd("login", "Connect your ListenBrainz account").build(),
        cmd("update", "Fetch your latest listens").build(),
        cmd("fm", "Your current or last played track").build(),
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
    ]
}

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
        "fm" => Some(Cmd::Fm),
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
        _ => None,
    }
}

fn display_name(user: &User, nick: Option<&str>) -> String {
    nick.or(user.global_name.as_deref()).unwrap_or(&user.name).to_owned()
}

impl Bot {
    fn db(&self) -> &Db {
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
                self.db().clear_guild(id.get() as i64).await.and_then(|()| {
                    Ok(sender.command(&RequestGuildMembers::builder(id).query("", None))?)
                })
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
        tracing::info!(user = msg.author.id.get(), ?cmd, "text command");

        let ctx = Ctx {
            discord_id: msg.author.id.get() as i64,
            guild,
            name: display_name(&msg.author, msg.member.as_ref().and_then(|m| m.nick.as_deref())),
        };
        let typing = self.http.create_typing_trigger(msg.channel_id);
        let _ = typing.await;
        let out = match cmd {
            Cmd::Login => reply("Use `/login` so your token stays private."),
            Cmd::Prefix(_) => reply(format!("Prefix: `{prefix}`. Use `/prefix` to change it (needs Manage Server).")),
            Cmd::RefreshMembers => reply("Use `/refreshmembers` to reload the member list (needs Manage Server)."),
            cmd => self.run(&ctx, cmd).await.unwrap_or_else(error_reply),
        };
        tracing::info!(title = ?out.title, desc = ?out.description.as_deref().map(|d| d.chars().take(120).collect::<String>()), "reply");
        self.http.create_message(msg.channel_id).embeds(&[out]).reply(msg.id).await?;
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
                    let resp = ChannelMessageBuilder::new().embeds([reply(text)]).flags(MessageFlags::EPHEMERAL).build();
                    client.create_response(i.id, &i.token, &resp).await?;
                    return Ok(());
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
                    let label = Label {
                        component: Box::new(Component::TextInput(input)),
                        description: Some("Stored encrypted. Used to download your history as an export.".into()),
                        id: None,
                        label: "ListenBrainz token".into(),
                    };
                    let resp = ModalBuilder::new("login", "Connect to ListenBrainz", [Component::Label(label)]).build();
                    client.create_response(i.id, &i.token, &resp).await?;
                    return Ok(());
                }
                let ephemeral = matches!(cmd, Cmd::Privacy | Cmd::Remove { .. } | Cmd::Prefix(_));
                self.defer(i, ephemeral).await?;
                let out = self.run(&ctx, cmd).await.unwrap_or_else(error_reply);
                client.update_response(&i.token).embeds(Some(&[out])).await?;
            }
            Some(InteractionData::ModalSubmit(data)) if data.custom_id == "login" => {
                self.defer(i, true).await?;
                let token = data.components.iter().find_map(text_value).unwrap_or_default();
                let out = self.login(&ctx, token.trim()).await.unwrap_or_else(error_reply);
                client.update_response(&i.token).embeds(Some(&[out])).await?;
            }
            _ => {}
        }
        Ok(())
    }

    async fn defer(&self, i: &Interaction, ephemeral: bool) -> Result<()> {
        let data = ephemeral.then(|| InteractionResponseData { flags: Some(MessageFlags::EPHEMERAL), ..Default::default() });
        let resp = InteractionResponse { kind: InteractionResponseType::DeferredChannelMessageWithSource, data };
        self.http.interaction(self.app_id).create_response(i.id, &i.token, &resp).await?;
        Ok(())
    }

    async fn login(&self, ctx: &Ctx, token: &str) -> Result<Embed> {
        // A running index would write the old account's data over the new one.
        if let Some(old) = self.db().user_by_discord(ctx.discord_id).await?
            && self.ingest.is_busy(old.id)
        {
            return Ok(reply("Your history is still downloading. Try again when it finishes."));
        }
        let Some(lb_name) = self.ingest.lb.validate_token(token).await? else {
            return Ok(reply("Invalid token. Paste the **User token** from listenbrainz.org/settings."));
        };
        let enc = self.ingest.crypto.encrypt(token);
        let user_id = self.db().upsert_user(ctx.discord_id, &lb_name, &enc).await?;
        if let Some(g) = ctx.guild {
            self.db().add_members(g, &[(ctx.discord_id, ctx.name.clone())]).await?;
        }
        let user = self.db().user(user_id).await?;
        let mut text = format!("Connected as **{lb_name}**.");
        if !user.indexed {
            self.ingest.spawn_index(user_id);
            text.push_str(" Your full history is downloading as a ListenBrainz export. Large accounts can take a few minutes.");
        }
        if ctx.guild.is_none() {
            // Users who connect in a DM join server lists with their first command there, or via /refreshmembers.
            text.push_str(" To show up in a server's WhoKnows, use any command in that server.");
        }
        Ok(reply(text))
    }

    async fn run(&self, ctx: &Ctx, cmd: Cmd) -> Result<Embed> {
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

        let Some(user) = self.db().user_by_discord(ctx.discord_id).await? else {
            return Ok(reply("Connect your ListenBrainz account with `/login` first."));
        };
        if let Some(g) = ctx.guild {
            self.db().add_members(g, &[(ctx.discord_id, ctx.name.clone())]).await?;
        }
        let indexing_note = (!user.indexed).then_some("Your history is still downloading, so counts may be incomplete.");

        match cmd {
            Cmd::Update => {
                if self.ingest.is_busy(user.id) {
                    return Ok(reply("Your history is downloading now. It will be up to date when it finishes."));
                }
                if !user.indexed {
                    // The previous index stopped on an error.
                    self.ingest.spawn_index(user.id);
                    return Ok(reply("Restarted the download of your history."));
                }
                Ok(match self.ingest.sync(&user).await? {
                    (_, true) => reply("Your history on ListenBrainz changed (an import or a deletion). Downloading it again."),
                    (n, false) => reply(format!("Updated: {n} new listens.")),
                })
            }
            Cmd::Fm => {
                let (now, updated) = tokio::join!(self.ingest.lb.now_or_last(&user.lb_name), self.ingest.update(&user));
                if let Err(e) = updated {
                    tracing::warn!(error = ?e, "update during fm failed");
                }
                let Some((t, playing, mapping)) = now? else { return Ok(reply("No listens yet.")) };
                let plays = async {
                    Ok::<_, anyhow::Error>(match self.db().find(Kind::Artist, &t.artist_name, None).await? {
                        Some((id, _)) => self.db().plays(Kind::Artist, id, user.id).await?,
                        None => 0,
                    })
                };
                let art = self.art(&user, Kind::Track, &t.artist_name, Some(&t.track_name), t.release_name.as_deref(), mapping);
                let (artist_plays, art) = tokio::join!(plays, art);
                let artist_plays = artist_plays?;
                let track = match &art.url {
                    Some(url) => format!("[{}]({url})", escape(&t.track_name)),
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
                // The playing track's name and mapping, for the cover; empty for a typed query.
                let mut now = (None, Value::Null);
                let (artist, name) = match query {
                    Some(q) => match kind {
                        Kind::Artist => (q, None),
                        _ => match q.split_once(" - ") {
                            Some((a, n)) => (a.trim().to_owned(), Some(n.trim().to_owned())),
                            None => return Ok(reply("Format: `Artist - Name`")),
                        },
                    },
                    None => {
                        let Some((t, _, mapping)) = self.ingest.lb.now_or_last(&user.lb_name).await? else {
                            return Ok(reply("No listens yet."));
                        };
                        let name = match kind {
                            Kind::Artist => None,
                            Kind::Album => match t.release_name.filter(|a| !a.is_empty()) {
                                Some(a) => Some(a),
                                None => return Ok(reply("The playing track has no album.")),
                            },
                            Kind::Track => Some(t.track_name.clone()),
                        };
                        now = (Some(t.track_name), mapping);
                        (t.artist_name, name)
                    }
                };
                if let Err(e) = self.ingest.update(&user).await {
                    tracing::warn!(error = ?e, "update during wk failed");
                }
                let Some((id, title)) = self.db().find(kind, &artist, name.as_deref()).await? else {
                    return Ok(reply(format!("No one on the bot has listened to **{}**.", name.unwrap_or(artist))));
                };
                let guild = if global { None } else { ctx.guild };
                let rows = self.db().who_knows(kind, id, guild).await?;
                let (track, album) = match kind {
                    Kind::Artist => (now.0, None),
                    Kind::Album => (now.0, name.as_deref()),
                    Kind::Track => (name.clone(), None),
                };
                let art = self.art(&user, kind, &artist, track.as_deref(), album, now.1);
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
                Ok(reply("All your data will be deleted. To confirm, run `/remove confirm:True` or `.remove confirm`."))
            }
            Cmd::Remove { confirm: true } => {
                if self.ingest.is_busy(user.id) {
                    return Ok(reply("Your data can't be deleted while your history is downloading. Try again shortly."));
                }
                self.db().delete_user(user.id).await?;
                Ok(reply("Your account and all your data were deleted."))
            }
            Cmd::Login | Cmd::Help | Cmd::Prefix(_) | Cmd::RefreshMembers => unreachable!("handled above"),
        }
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
            let link = format!("[{}]({})", escape(name), crate::lb::profile_url(lb_name));
            if me { format!("**{rank} {link}** - **{plays}** plays") } else { format!("{rank} {link} - **{plays}** plays") }
        };
        let crown = has_crown(kind, global, &rows.iter().map(|r| r.plays).collect::<Vec<_>>());
        let mut lines: Vec<String> = rows
            .iter()
            .enumerate()
            .map(|(n, r)| {
                let rank = if n == 0 && crown { "👑".to_owned() } else { format!("{}.", n + 1) };
                row(rank, &r.name, &r.lb_name, r.plays, r.discord_id == user.discord_id)
            })
            .collect();
        if own > 0 && !rows.iter().any(|r| r.discord_id == user.discord_id) {
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
    async fn art(&self, user: &crate::db::User, kind: Kind, artist: &str, track: Option<&str>, album: Option<&str>, mapping: Value) -> Art {
        let name = match kind {
            Kind::Artist => "",
            Kind::Album => album.unwrap_or_default(),
            Kind::Track => track.unwrap_or_default(),
        };
        let (key, name_key) = art_keys(kind, artist, name, &mapping);
        let cached = cache_get(&self.art_cache.lock().unwrap(), &key);
        let mut art = match cached {
            Some(art) => art,
            None => self.fetch_art_cached(user, kind, artist, track, album, mapping, key, name_key).await,
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
        user: &crate::db::User,
        kind: Kind,
        artist: &str,
        track: Option<&str>,
        album: Option<&str>,
        mapping: Value,
        key: ArtKey,
        name_key: ArtKey,
    ) -> Art {
        match self.fetch_art(user, kind, artist, track, album, mapping).await {
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
        user: &crate::db::User,
        kind: Kind,
        artist: &str,
        track: Option<&str>,
        album: Option<&str>,
        mut mapping: Value,
    ) -> Result<(Art, bool)> {
        let lb = &self.ingest.lb;
        let token = self.ingest.crypto.decrypt(&user.lb_token)?;
        Ok(match kind {
            Kind::Artist => {
                // Without a mapping (playing-now) it comes from a lookup: the first id is the main artist ("A feat. B" → A).
                if mapping["artist_mbids"].get(0).is_none()
                    && let Some(t) = track
                {
                    match lb.lookup(&token, artist, t, album).await {
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
                    (None, Some(mbid)) => lb.artist_cover(&token, mbid).await.unwrap_or_else(|e| {
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
                    mapping = lb.lookup(&token, artist, t, album).await?;
                }
                let Some(rec) = mapping["recording_mbid"].as_str() else { return Ok((Art::default(), true)) };
                let url = Some(crate::lb::page_url("track", rec));
                // The cover comes from the same release as the track page; if that request fails, use the mapping's and don't cache.
                match lb.recording_release(&token, rec).await {
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
fn art_keys(kind: Kind, artist: &str, name: &str, mapping: &Value) -> (ArtKey, ArtKey) {
    let name_key = (kind, artist.to_lowercase(), name.to_lowercase());
    let mbid = (kind == Kind::Artist).then(|| mapping["artist_mbids"].get(0).and_then(Value::as_str)).flatten();
    (mbid.map_or_else(|| name_key.clone(), |m| (kind, m.to_owned(), "mbid".into())), name_key)
}

fn cache_get(cache: &HashMap<ArtKey, (std::time::Instant, Art)>, key: &ArtKey) -> Option<Art> {
    cache.get(key).filter(|(at, _)| at.elapsed() < ART_TTL).map(|(_, art)| art.clone())
}

fn cache_put(cache: &mut HashMap<ArtKey, (std::time::Instant, Art)>, key: ArtKey, art: Art, at: std::time::Instant) {
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

fn text_value(c: &ModalInteractionComponent) -> Option<String> {
    match c {
        ModalInteractionComponent::TextInput(t) if t.custom_id == "token" => Some(t.value.clone()),
        ModalInteractionComponent::Label(l) => text_value(&l.component),
        ModalInteractionComponent::ActionRow(r) => r.components.iter().find_map(text_value),
        _ => None,
    }
}

fn escape(s: &str) -> String {
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

const HELP: &str = "**Commands** (default prefix `.`; each one also exists as a slash command)
`/login`: connect your ListenBrainz account (slash only, so the token stays private)
`.fm`: your current track
`.w` `.wa` `.wt`: who listens to an artist / album / track in this server (`Artist - Name`)
`.gw` `.gwa` `.gwt`: the same across the bot
`.update`: fetch your latest listens
`.privacy`: show or hide yourself in global lists
`.remove`: delete your account and data
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
        assert_eq!(slash_commands().len(), 14);
        assert_eq!(escape("a*b_c"), "a\\*b\\_c");
    }
}
