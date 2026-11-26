mod bot;
mod crypto;
mod db;
mod featured;
mod hist;
mod iceberg;
mod history;
mod ingest;
mod lb;
mod stats;

use std::{env, sync::Arc};

use anyhow::{Context, Result};
use twilight_gateway::{ConfigBuilder, EventTypeFlags, Intents, Shard, ShardId, StreamExt as _};
use tracing_subscriber::EnvFilter;
use twilight_model::gateway::{
    payload::outgoing::{UpdatePresence, update_presence::UpdatePresencePayload},
    presence::{Activity, ActivityType, Status},
};

fn var(name: &str) -> Result<String> {
    env::var(name).with_context(|| format!("{name} is not set"))
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into())).init();
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let token = var("DISCORD_TOKEN")?;
    let crypto = Arc::new(crypto::Crypto::from_base64(&var("TOKEN_KEY")?)?);
    let db = db::connect(&var("DATABASE_URL")?, crypto).await?;
    let lb = lb::Lb::new(var("LB_TOKEN")?);
    // Card images need a valid token for LB's metadata endpoints; fail at startup rather than on every card.
    // An LB outage shouldn't keep the bot down: WhoKnows works from the database.
    match lb.validate_token(&var("LB_TOKEN")?).await {
        Ok(Some(_)) => {}
        Ok(None) => anyhow::bail!("LB_TOKEN is not a valid ListenBrainz token"),
        Err(e) => tracing::warn!(error = ?e, "couldn't check LB_TOKEN, ListenBrainz may be down"),
    }
    let ingest = ingest::Ingest::new(db.clone(), lb);
    // Indexes cut off by a restart start over from the beginning.
    for id in db.unindexed_users().await? {
        ingest.spawn_index(id);
    }

    let http = Arc::new(twilight_http::Client::new(token.clone()));
    let app = http.current_user_application().await?.model().await?;
    let app_id = app.id;
    // For a team app, `owner` is a placeholder user; the team owner is the real one.
    let owner = app.team.map(|t| t.owner_user_id).or(app.owner.map(|u| u.id)).map(|id| id.get() as i64);
    http.interaction(app_id).set_global_commands(&bot::slash_commands()).await?;
    // Kept in the code so every deployment shows the same profile text; edits in the Developer Portal are overwritten.
    if let Err(e) = http.update_current_user_application().description(DESCRIPTION).await {
        tracing::warn!(error = ?e, "couldn't set the app description");
    }

    let featured = Arc::new(featured::Featured::default());
    tokio::spawn(featured::run(featured.clone(), http.clone(), ingest.clone()));
    tokio::spawn(ingest::refresh_loop(ingest.clone()));

    let bot = Arc::new(bot::Bot {
        http: http.clone(),
        app_id,
        ingest,
        prefixes: Default::default(),
        guild_names: Default::default(),
        art_cache: Default::default(),
        featured,
        owner,
    });

    let intents = Intents::GUILDS | Intents::GUILD_MEMBERS | Intents::GUILD_MESSAGES | Intents::MESSAGE_CONTENT;
    let total = http.gateway().authed().await?.model().await?.shards;
    // Also given at login, so a shard that reconnects shows a status before the next count.
    let presence = UpdatePresencePayload::new(vec![status("/np · lbbot".into())], false, None, Status::Online)?;
    let config = ConfigBuilder::new(token, intents).presence(presence).build();
    let shards: Vec<Shard> = (0..total).map(|i| Shard::with_config(ShardId::new(i, total), config.clone())).collect();
    tokio::spawn(presence_loop(shards.iter().map(Shard::sender).collect(), bot.clone()));
    let mut tasks = tokio::task::JoinSet::new();
    for shard in shards {
        tasks.spawn(run_shard(shard, bot.clone()));
    }
    tracing::info!(shards = tasks.len(), "running");
    tokio::signal::ctrl_c().await?;
    Ok(())
}

const DESCRIPTION: &str = "Shows what you're listening to and who in your server listens to the same artist, album or track, from your ListenBrainz history. Start with /login.\n\nOpen source: https://github.com/zoltrons/lbbot";

/// fmbot-style status: "/np · lbbot · N servers", refreshed every 10 minutes on every shard.
async fn presence_loop(senders: Vec<twilight_gateway::MessageSender>, bot: Arc<bot::Bot>) {
    // Let the shards connect and the GUILD_CREATE events arrive before the first count.
    tokio::time::sleep(std::time::Duration::from_secs(30)).await;
    loop {
        let servers = bot.guild_names.read().unwrap().len();
        match UpdatePresence::new(vec![status(format!("/np · lbbot · {servers} servers"))], false, None, Status::Online) {
            Ok(presence) => senders.iter().for_each(|s| {
                let _ = s.command(&presence);
            }),
            Err(e) => tracing::warn!(error = ?e, "bad presence"),
        }
        tokio::time::sleep(std::time::Duration::from_secs(600)).await;
    }
}

fn status(text: String) -> Activity {
    Activity {
        application_id: None,
        assets: None,
        buttons: Vec::new(),
        created_at: None,
        details: None,
        details_url: None,
        emoji: None,
        flags: None,
        id: None,
        instance: None,
        kind: ActivityType::Custom,
        name: "Custom Status".into(),
        party: None,
        secrets: None,
        state: Some(text),
        state_url: None,
        timestamps: None,
        url: None,
        status_display_type: None,
    }
}

async fn run_shard(mut shard: Shard, bot: Arc<bot::Bot>) {
    let wanted = EventTypeFlags::GUILD_CREATE
        | EventTypeFlags::GUILD_DELETE
        | EventTypeFlags::GUILD_UPDATE
        | EventTypeFlags::MEMBER_CHUNK
        | EventTypeFlags::MEMBER_ADD
        | EventTypeFlags::MEMBER_REMOVE
        | EventTypeFlags::MEMBER_UPDATE
        | EventTypeFlags::MESSAGE_CREATE
        | EventTypeFlags::INTERACTION_CREATE;
    while let Some(item) = shard.next_event(wanted).await {
        match item {
            Ok(event) => {
                tokio::spawn(bot.clone().handle(event, shard.sender()));
            }
            Err(e) => tracing::warn!(error = ?e, "gateway error"),
        }
    }
}
