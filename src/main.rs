#[cfg(feature = "use-mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod cache;
mod commands;
mod ctx;
mod dispatch;
mod events;
mod hierarchy;
mod store;
#[cfg(test)]
mod tests;

use crate::store::{persist, registry};

pub use self::ctx::CTX;

use std::{env, time::Duration};
use twilight_gateway::{
    ConfigBuilder, EventTypeFlags, Intents, Shard, ShardId, queue::InMemoryQueue,
};
use twilight_http::Client;

#[rustfmt::skip]
const EVENT_TYPES: EventTypeFlags = EventTypeFlags::empty()
    .union(EventTypeFlags::GUILD_CREATE)
    .union(EventTypeFlags::MEMBER_ADD)
    .union(EventTypeFlags::MEMBER_REMOVE)
    .union(EventTypeFlags::MEMBER_UPDATE)
    .union(EventTypeFlags::MEMBER_CHUNK)
    .union(EventTypeFlags::ROLE_CREATE)
    .union(EventTypeFlags::ROLE_UPDATE)
    .union(EventTypeFlags::ROLE_DELETE)
    .union(EventTypeFlags::GUILD_UPDATE)
    .union(EventTypeFlags::INTERACTION_CREATE)
    .union(EventTypeFlags::GUILD_DELETE)
    .union(EventTypeFlags::GUILD_AUDIT_LOG_ENTRY_CREATE)
    .union(EventTypeFlags::READY);
const INTENTS: Intents = Intents::GUILDS
    .union(Intents::GUILD_MEMBERS)
    .union(Intents::GUILD_MODERATION);

const SWEEP_INTERVAL_SECS: u64 = 3600;
const COMPACT_INTERVAL_SECS: u64 = 60;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let token = env::var("DISCORD_TOKEN")?;

    let http = Client::new(token.clone());
    let app = http.current_user_application().await?.model().await?;
    let info = http.gateway().authed().await?.model().await?;
    http.interaction(app.id)
        .set_global_commands(&crate::commands::global_commands())
        .await?;
    ctx::init(app.id, http);

    match persist::ensure_data_dir().await {
        Ok(_dir) => {
            let loaded = persist::load_all().await;
            tracing::info!(count = loaded.len(), "restored guild stores from disk");
        }
        Err(e) => {
            tracing::warn!(error = %e, "data dir unavailable; running without persistence")
        }
    }

    let sweep_task = tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(SWEEP_INTERVAL_SECS));
        loop {
            tick.tick().await;
            let now = registry::now_secs();
            let _tombstoned = events::sweep_tombstones_hourly().await;
            let _removed = persist::expire_sweep_all(now).await;
            let _swept_audit = events::sweep_audit_recent();
            let _swept_cooldowns = crate::commands::sweep_all_cooldowns(std::time::Instant::now());
            let _swept_pending = registry::sweep_expired_pending_chunks(u64::from(now)).await;
        }
    });

    let compact_task = tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(COMPACT_INTERVAL_SECS));
        loop {
            tick.tick().await;
            persist::compact_check_all().await;
        }
    });

    let queue = InMemoryQueue::new(
        info.session_start_limit.max_concurrency,
        info.session_start_limit.remaining,
        Duration::from_millis(info.session_start_limit.reset_after),
        info.session_start_limit.total,
    );
    let config = ConfigBuilder::new(token, INTENTS).queue(queue).build();

    let shards: Vec<Shard> = (0..info.shards)
        .map(|id| {
            let shard_id = ShardId::new(id, info.shards);
            Shard::with_config(shard_id, config.clone())
        })
        .collect();

    let shutdown_token = dispatch::shutdown_signal().await;

    let mut shard_set = tokio::task::JoinSet::new();
    for shard in shards {
        shard_set.spawn(dispatch::run(shard, shutdown_token.clone()));
    }

    shutdown_token.cancelled().await;
    tracing::info!("shutting down; waiting for tasks to finish");

    sweep_task.abort();
    compact_task.abort();
    while shard_set.join_next().await.is_some() {}
    Ok(())
}
