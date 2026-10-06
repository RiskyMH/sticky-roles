use twilight_model::gateway::payload::incoming::{GuildUpdate, MemberChunk};
use twilight_model::guild::Guild;
use twilight_model::id::{Id, marker::GuildMarker};

use crate::cache::Cache;
use crate::commands;
use crate::store::{GuildConfig, persist, registry};

use super::{audit, bot_id, cache, is_last_chunk};

pub(crate) async fn on_guild_create(guild: &Guild) {
    let guild_id = guild.id;
    let _ = persist::cancel_tombstone(guild_id, persist::now_u64()).await;
    registry::ensure_guild(guild_id, GuildConfig::default()).await;
    let save_nicknames = registry::with_guild(guild_id, |s| s.config.save_nicknames)
        .await
        .unwrap_or_default();
    cache()
        .write()
        .await
        .upsert_from_guild(guild, bot_id(), save_nicknames);
    let total = guild.member_count.unwrap_or(0).min(u32::MAX as u64) as u32;
    if Cache::needs_chunk(total, guild.members.len()) {
        registry::mark_pending_chunk(guild_id).await;
    } else {
        registry::remove_pending_chunk(guild_id).await;
    }
}

pub(crate) async fn on_guild_update(payload: &GuildUpdate) {
    let mut guard = cache().write().await;
    if let Some(g) = guard.guilds.get_mut(&payload.0.id) {
        g.owner_id = payload.0.owner_id;
    }
}

pub(crate) async fn on_guild_delete(guild_id: Id<GuildMarker>) {
    cache().write().await.remove_guild(guild_id);
    registry::remove_pending_chunk(guild_id).await;
    audit::evict_guild(guild_id);
    commands::evict_guild_cooldowns(guild_id);
    // Registry store stays: tombstone keeps it for the grace window, and the
    // hourly sweep evicts it only once purged. Removing it here would break
    // tombstone_guild's has_data check (no grace, fresh server on rejoin).
    persist::tombstone_guild(guild_id, persist::now_u64()).await;
}

pub(crate) async fn on_chunk(chunk: &MemberChunk) {
    let save_nicknames = registry::with_guild(chunk.guild_id, |s| s.config.save_nicknames)
        .await
        .unwrap_or_default();
    cache()
        .write()
        .await
        .handle_chunk(chunk, save_nicknames, bot_id());
    if is_last_chunk(chunk.chunk_index, chunk.chunk_count) {
        registry::remove_pending_chunk(chunk.guild_id).await;
    }
}
