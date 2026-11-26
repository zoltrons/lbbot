mod bot;
mod crypto;
mod db;
mod featured;
mod history;
mod ingest;
mod lb;

use std::{env, sync::Arc};

use anyhow::{Context, Result};
use twilight_gateway::{Config, EventTypeFlags, Intents, Shard, ShardId, StreamExt as _};
use tracing_subscriber::EnvFilter;

fn var(name: &str) -> Result<String> {
    env::var(name).with_context(|| format!("{name} is not set"))
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into())).init();
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let token = var("DISCORD_TOKEN")?;
    let db = db::connect(&var("DATABASE_URL")?).await?;
    let ingest = ingest::Ingest::new(db.clone(), lb::Lb::new(), crypto::Crypto::from_base64(&var("TOKEN_KEY")?)?);
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

    let featured = Arc::new(featured::Featured::default());
    tokio::spawn(featured::run(featured.clone(), http.clone(), ingest.clone()));

    let bot = Arc::new(bot::Bot {
        http: http.clone(),
        app_id,
        ingest,
        prefixes: std::sync::RwLock::new(db.prefixes().await?.into_iter().collect()),
        guild_names: Default::default(),
        art_cache: Default::default(),
        featured,
        owner,
    });

    let intents = Intents::GUILDS | Intents::GUILD_MEMBERS | Intents::GUILD_MESSAGES | Intents::MESSAGE_CONTENT;
    let total = http.gateway().authed().await?.model().await?.shards;
    let config = Config::new(token, intents);
    let shards = (0..total).map(|i| Shard::with_config(ShardId::new(i, total), config.clone()));
    let mut tasks = tokio::task::JoinSet::new();
    for shard in shards {
        tasks.spawn(run_shard(shard, bot.clone()));
    }
    tracing::info!(shards = tasks.len(), "running");
    tokio::signal::ctrl_c().await?;
    Ok(())
}

async fn run_shard(mut shard: Shard, bot: Arc<bot::Bot>) {
    let wanted = EventTypeFlags::GUILD_CREATE
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
