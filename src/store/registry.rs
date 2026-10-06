use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use rustc_hash::FxHashMap;
use tokio::sync::RwLock;
use twilight_model::id::{
    Id,
    marker::{GuildMarker, UserMarker},
};

use super::{GuildConfig, GuildStore};

static STORES: OnceLock<RwLock<FxHashMap<Id<GuildMarker>, GuildStore>>> = OnceLock::new();
static PENDING: OnceLock<RwLock<HashMap<Id<GuildMarker>, PendingChunk>>> = OnceLock::new();

#[derive(Clone, Copy, Debug)]
struct PendingChunk {
    marked_at: u64,
    attempts: u32,
    last_sent_ms: u64,
}

pub const PENDING_CHUNK_TTL_SECS: u64 = 86400;
/// Base resend backoff per guild: chunk requests are limited to one per
/// 30 s per server, so the first retry waits that out.
pub const BACKOFF_BASE_MS: u64 = 30_000;
/// Max resend backoff per guild.
pub const BACKOFF_MAX_MS: u64 = 300_000;
pub const SNOWFLAKE_WORKER_SHIFT: u32 = 22;

pub fn shard_for_guild(guild_id: Id<GuildMarker>, total: u32) -> u32 {
    let total = total.max(1) as u64;
    ((guild_id.get() >> SNOWFLAKE_WORKER_SHIFT) % total) as u32
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

pub fn now_secs() -> u32 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs().min(u64::from(u32::MAX)) as u32)
        .unwrap_or(0)
}

pub(crate) fn stores() -> &'static RwLock<FxHashMap<Id<GuildMarker>, GuildStore>> {
    STORES.get_or_init(|| RwLock::new(FxHashMap::default()))
}

fn pending() -> &'static RwLock<HashMap<Id<GuildMarker>, PendingChunk>> {
    PENDING.get_or_init(|| RwLock::new(HashMap::new()))
}

pub async fn ensure_guild(guild_id: Id<GuildMarker>, config: GuildConfig) {
    stores()
        .write()
        .await
        .entry(guild_id)
        .or_insert_with(|| GuildStore::new(guild_id, config));
}

pub async fn remove_guild(guild_id: Id<GuildMarker>) -> bool {
    let removed = stores().write().await.remove(&guild_id).is_some();
    pending().write().await.remove(&guild_id);
    removed
}

pub async fn with_guild<T>(
    guild_id: Id<GuildMarker>,
    f: impl FnOnce(&GuildStore) -> T,
) -> Option<T> {
    stores().read().await.get(&guild_id).map(f)
}

pub async fn guild_ids() -> Vec<Id<GuildMarker>> {
    stores().read().await.keys().copied().collect()
}

pub async fn has_entry(guild_id: Id<GuildMarker>, user_id: Id<UserMarker>) -> bool {
    with_guild(guild_id, |s| s.map.contains_key(&user_id))
        .await
        .unwrap_or(false)
}

pub async fn get_left_at(guild_id: Id<GuildMarker>, user_id: Id<UserMarker>) -> Option<u32> {
    with_guild(guild_id, |s| s.map.get(&user_id).map(|e| e.left_at)).await?
}

pub async fn remove_expired_entry_on_join(
    guild_id: Id<GuildMarker>,
    user_id: Id<UserMarker>,
    now_secs: u32,
) -> bool {
    let mut guard = stores().write().await;
    let Some(store) = guard.get_mut(&guild_id) else {
        return false;
    };
    let expired = store
        .map
        .get(&user_id)
        .is_some_and(|e| GuildStore::is_expired(e.left_at, now_secs, store.config.expiry_secs));
    if expired {
        store.remove_user(user_id);
    }
    expired
}

pub async fn mark_pending_chunk(guild_id: Id<GuildMarker>) {
    pending()
        .write()
        .await
        .entry(guild_id)
        .or_insert(PendingChunk {
            marked_at: u64::from(now_secs()),
            attempts: 0,
            last_sent_ms: 0,
        });
}

pub async fn remove_pending_chunk(guild_id: Id<GuildMarker>) -> bool {
    pending().write().await.remove(&guild_id).is_some()
}

pub async fn has_pending_chunks() -> bool {
    !pending().read().await.is_empty()
}

pub fn chunk_backoff_ms(attempts: u32) -> u64 {
    if attempts == 0 {
        return 0;
    }
    BACKOFF_BASE_MS
        .saturating_mul(1u64 << attempts.saturating_sub(1).min(6))
        .min(BACKOFF_MAX_MS)
}

pub async fn pending_chunks_due_for_shard(
    shard_number: u32,
    total: u32,
    now_ms: u64,
) -> Vec<Id<GuildMarker>> {
    pending()
        .read()
        .await
        .iter()
        .filter(|(gid, st)| {
            shard_for_guild(**gid, total) == shard_number
                && (st.last_sent_ms == 0
                    || now_ms.saturating_sub(st.last_sent_ms) >= chunk_backoff_ms(st.attempts))
        })
        .map(|(gid, _)| *gid)
        .collect()
}

pub async fn note_pending_chunk_sent(guild_id: Id<GuildMarker>, sent_ms: u64) {
    if let Some(st) = pending().write().await.get_mut(&guild_id) {
        st.attempts = st.attempts.saturating_add(1);
        st.last_sent_ms = sent_ms;
    }
}

pub async fn sweep_expired_pending_chunks(now: u64) -> usize {
    let mut guard = pending().write().await;
    let before = guard.len();
    guard.retain(|_, st| now.saturating_sub(st.marked_at) <= PENDING_CHUNK_TTL_SECS);
    before - guard.len()
}
