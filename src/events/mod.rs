pub mod audit;
pub mod guild;
pub mod member;
pub mod restore;
pub mod roles;

use audit::sweep;

use std::sync::{
    OnceLock,
    atomic::{AtomicU64, Ordering},
};
use tokio::sync::RwLock;
use twilight_gateway::Event;
use twilight_model::gateway::payload::incoming::GuildCreate as GuildCreateEvent;
use twilight_model::id::{Id, marker::UserMarker};

use crate::cache::Cache;
use crate::commands;
use crate::store::{persist, registry};

static CACHE: OnceLock<RwLock<Cache>> = OnceLock::new();
static BOT_ID: AtomicU64 = AtomicU64::new(0);

pub(crate) fn cache() -> &'static RwLock<Cache> {
    CACHE.get_or_init(|| RwLock::new(Cache::default()))
}

pub(crate) fn bot_id() -> Option<Id<UserMarker>> {
    Id::new_checked(BOT_ID.load(Ordering::Acquire))
}

pub(crate) fn sweep_audit_recent() -> usize {
    sweep()
}

pub async fn event_handler(event: Event, _state: ()) {
    match event {
        Event::InteractionCreate(interaction) => {
            commands::handle_interaction(interaction.0).await;
        }
        Event::Ready(ready) => {
            BOT_ID.store(ready.user.id.get(), Ordering::Release);
        }
        Event::GuildCreate(payload) => match &*payload {
            GuildCreateEvent::Available(guild) => guild::on_guild_create(guild).await,
            GuildCreateEvent::Unavailable(_) => {}
        },
        Event::MemberChunk(chunk) => guild::on_chunk(&chunk).await,
        Event::MemberAdd(payload) => member::on_member_add(&payload).await,
        Event::MemberRemove(payload) => member::on_member_remove(&payload).await,
        Event::MemberUpdate(payload) => {
            let save = registry::with_guild(payload.guild_id, |s| s.config.save_nicknames)
                .await
                .unwrap_or_default();
            cache()
                .write()
                .await
                .handle_update(&payload, save, bot_id());
        }
        Event::RoleCreate(payload) => {
            cache().write().await.handle_role_create(&payload, bot_id());
        }
        Event::RoleUpdate(payload) => roles::on_role_update(&payload).await,
        Event::RoleDelete(payload) => roles::on_role_delete(&payload).await,
        Event::GuildUpdate(payload) => guild::on_guild_update(&payload).await,
        Event::GuildAuditLogEntryCreate(payload) => audit::on_audit_entry(&payload).await,
        Event::GuildDelete(guild) if !is_guild_outage(guild.unavailable) => {
            guild::on_guild_delete(guild.id).await;
        }
        _ => {}
    }
}

pub(crate) fn is_guild_outage(unavailable: Option<bool>) -> bool {
    unavailable == Some(true)
}

pub(crate) fn is_last_chunk(chunk_index: u32, chunk_count: u32) -> bool {
    chunk_count > 0 && chunk_index < chunk_count && chunk_index.saturating_add(1) == chunk_count
}

pub async fn sweep_tombstones_hourly() -> usize {
    let purged = persist::sweep_tombstones(persist::now_u64()).await;
    for gid in &purged {
        if persist::is_tombstoned(*gid) {
            continue;
        }
        if registry::with_guild(*gid, |_| ()).await.is_some() {
            continue;
        }
        cache().write().await.remove_guild(*gid);
        registry::remove_guild(*gid).await;
        registry::remove_pending_chunk(*gid).await;
        audit::evict_guild(*gid);
        commands::evict_guild_cooldowns(*gid);
    }
    purged.len()
}
