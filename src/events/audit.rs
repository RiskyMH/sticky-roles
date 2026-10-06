use std::collections::VecDeque;
use std::sync::{
    Mutex, OnceLock,
    atomic::{AtomicUsize, Ordering},
};

use rustc_hash::FxHashMap;
use twilight_model::gateway::payload::incoming::GuildAuditLogEntryCreate;
use twilight_model::guild::audit_log::{AuditLogEntry, AuditLogEventType};
use twilight_model::id::{
    Id,
    marker::{GuildMarker, UserMarker},
};

use crate::store::{persist, registry};

const TTL_SECS: u32 = 90;
const MAX: usize = 10_000;
const PER_KEY: usize = 4;

type AuditMap = FxHashMap<(u64, u64), VecDeque<(AuditLogEventType, u32)>>;

static AUDIT: OnceLock<Mutex<AuditMap>> = OnceLock::new();
static BOT_SAVES: OnceLock<Mutex<FxHashMap<(u64, u64), u32>>> = OnceLock::new();
static AUDIT_LEN: AtomicUsize = AtomicUsize::new(0);

fn lock() -> std::sync::MutexGuard<'static, AuditMap> {
    AUDIT
        .get_or_init(|| Mutex::new(FxHashMap::default()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn bot_save_lock() -> std::sync::MutexGuard<'static, FxHashMap<(u64, u64), u32>> {
    BOT_SAVES
        .get_or_init(|| Mutex::new(FxHashMap::default()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn fresh(at: u32, now: u32) -> bool {
    now.saturating_sub(at) < TTL_SECS
}

pub(crate) fn insert(
    guild_id: Id<GuildMarker>,
    user_id: Id<UserMarker>,
    action: AuditLogEventType,
) {
    let mut guard = lock();
    let now = registry::now_secs();
    if AUDIT_LEN.load(Ordering::Relaxed) >= MAX {
        guard.retain(|_, q| {
            let before = q.len();
            q.retain(|(_, at)| fresh(*at, now));
            let dropped = before - q.len();
            if dropped > 0 {
                AUDIT_LEN.fetch_sub(dropped, Ordering::Relaxed);
            }
            !q.is_empty()
        });
        while AUDIT_LEN.load(Ordering::Relaxed) >= MAX {
            let Some(oldest) = guard
                .iter()
                .filter_map(|(k, q)| q.front().map(|(_, at)| (*k, *at)))
                .min_by_key(|(_, at)| *at)
                .map(|(k, _)| k)
            else {
                break;
            };
            let empty = match guard.get_mut(&oldest) {
                Some(q) => {
                    q.pop_front();
                    AUDIT_LEN.fetch_sub(1, Ordering::Relaxed);
                    q.is_empty()
                }
                None => false,
            };
            if empty {
                guard.remove(&oldest);
            }
        }
    }
    let q = guard.entry((guild_id.get(), user_id.get())).or_default();
    if q.len() >= PER_KEY {
        // Bounded per-key queue: pop+push nets zero, counter untouched.
        q.pop_front();
    } else {
        AUDIT_LEN.fetch_add(1, Ordering::Relaxed);
    }
    q.push_back((action, now));
}

pub(crate) fn consume(
    guild_id: Id<GuildMarker>,
    user_id: Id<UserMarker>,
) -> Option<AuditLogEventType> {
    let mut guard = lock();
    let key = (guild_id.get(), user_id.get());
    let now = registry::now_secs();
    // Entries accounted up front: every path below removes the whole queue
    // (popped + remainder), so the key's pre-loop length is the exact drop.
    let queued = guard.get(&key).map(VecDeque::len).unwrap_or(0);
    let found = loop {
        let q = guard.get_mut(&key)?;
        match q.pop_front() {
            Some((action, at)) if fresh(at, now) => break Some(action),
            Some(_) if !q.is_empty() => {}
            _ => break None,
        }
    };
    guard.remove(&key);
    if queued > 0 {
        AUDIT_LEN.fetch_sub(queued, Ordering::Relaxed);
    }
    found
}

pub(crate) fn sweep() -> usize {
    let now = registry::now_secs();
    let before = AUDIT_LEN.load(Ordering::Relaxed);
    {
        let mut guard = lock();
        guard.retain(|_, q| {
            let len = q.len();
            q.retain(|(_, at)| fresh(*at, now));
            let dropped = len - q.len();
            if dropped > 0 {
                AUDIT_LEN.fetch_sub(dropped, Ordering::Relaxed);
            }
            !q.is_empty()
        });
    }
    let removed = before.saturating_sub(AUDIT_LEN.load(Ordering::Relaxed));
    let mut shields = bot_save_lock();
    let before = shields.len();
    shields.retain(|_, at| fresh(*at, now));
    removed + (before - shields.len())
}

pub(crate) fn evict_guild(guild_id: Id<GuildMarker>) -> usize {
    let before = AUDIT_LEN.load(Ordering::Relaxed);
    {
        let mut guard = lock();
        guard.retain(|(g, _), q| {
            if *g != guild_id.get() {
                return true;
            }
            let dropped = q.len();
            if dropped > 0 {
                AUDIT_LEN.fetch_sub(dropped, Ordering::Relaxed);
            }
            false
        });
    }
    let removed = before.saturating_sub(AUDIT_LEN.load(Ordering::Relaxed));
    let mut shields = bot_save_lock();
    let before = shields.len();
    shields.retain(|(g, _), _| *g != guild_id.get());
    removed + (before - shields.len())
}

pub(crate) fn note_bot_save(guild_id: Id<GuildMarker>, user_id: Id<UserMarker>) {
    bot_save_lock().insert((guild_id.get(), user_id.get()), registry::now_secs());
}

pub(crate) fn take_bot_save(guild_id: Id<GuildMarker>, user_id: Id<UserMarker>) -> bool {
    bot_save_lock()
        .remove(&(guild_id.get(), user_id.get()))
        .is_some()
}

pub(crate) fn was_banned_or_kicked(
    guild_id: Id<GuildMarker>,
    user_id: Id<UserMarker>,
    lacks_audit: bool,
) -> bool {
    if lacks_audit {
        return false;
    }
    consume(guild_id, user_id).is_some()
}

pub(crate) async fn on_audit_entry(payload: &GuildAuditLogEntryCreate) {
    let entry: &AuditLogEntry = &payload.0;
    if !matches!(
        entry.action_type,
        AuditLogEventType::MemberBanAdd | AuditLogEventType::MemberKick
    ) {
        return;
    }
    let (Some(guild_id), Some(target_id)) = (entry.guild_id, entry.target_id) else {
        return;
    };
    let guild_id = Id::<GuildMarker>::new(guild_id.get());
    let user_id = Id::<UserMarker>::new(target_id.get());
    if take_bot_save(guild_id, user_id) {
        return;
    }
    insert(guild_id, user_id, entry.action_type);
    let now = registry::now_secs();
    let purge = registry::with_guild(guild_id, |s| {
        s.config.dont_save_if_ban_kicked
            && s.map
                .get(&user_id)
                .is_some_and(|e| now.saturating_sub(e.left_at) < TTL_SECS)
    })
    .await
    .unwrap_or(false);
    if purge && let Err(e) = persist::append_delete(guild_id, user_id).await {
        tracing::warn!(error = ?e, "wal delete failed; stale ban/kick row kept");
    }
}
