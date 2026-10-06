use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use anyhow::Context;
use dashmap::DashMap;
use rustc_hash::FxHashMap;
use thiserror::Error;
use twilight_model::id::{
    Id,
    marker::{ChannelMarker, GuildMarker, RoleMarker, UserMarker},
};

use super::{GuildConfig, GuildStore, StickyEntry, registry, snapshot, wal};

pub(crate) const GRACE_SECS: u64 = 7 * 24 * 3600;

pub(crate) fn now_u64() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub(crate) fn is_grace_lapsed(deleted_at: u64, now: u64) -> bool {
    if deleted_at == 0 {
        return false;
    }
    now.saturating_sub(deleted_at) >= GRACE_SECS
}

static TOMBSTONES: OnceLock<DashMap<Id<GuildMarker>, u64>> = OnceLock::new();

fn tombstones() -> &'static DashMap<Id<GuildMarker>, u64> {
    TOMBSTONES.get_or_init(DashMap::new)
}

pub(crate) fn is_tombstoned(guild_id: Id<GuildMarker>) -> bool {
    tombstones().contains_key(&guild_id)
}

static DATA_DIR: std::sync::Mutex<OnceLock<PathBuf>> = std::sync::Mutex::new(OnceLock::new());

/// Drop the cached data dir so the next `data_dir()` re-reads the env (tests only).
#[cfg(test)]
pub(crate) fn invalidate_data_dir_for_test() {
    DATA_DIR.lock().expect("DATA_DIR lock poisoned").take();
}
static OPS_PENDING: AtomicU64 = AtomicU64::new(0);
static WAL_INFLIGHT: AtomicU64 = AtomicU64::new(0);
static LAST_COMPACT_AT: AtomicU64 = AtomicU64::new(0);
static TAIL_CHECKED: AtomicBool = AtomicBool::new(false);

static WAL_LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
static SNAPSHOT_LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

fn wal_lock() -> &'static tokio::sync::Mutex<()> {
    WAL_LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

fn snapshot_lock() -> &'static tokio::sync::Mutex<()> {
    SNAPSHOT_LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

fn bump_ops() {
    OPS_PENDING.fetch_add(1, Ordering::Relaxed);
}

struct Inflight;

impl Inflight {
    fn enter() -> Self {
        WAL_INFLIGHT.fetch_add(1, Ordering::Relaxed);
        Self
    }
}

impl Drop for Inflight {
    fn drop(&mut self) {
        WAL_INFLIGHT.fetch_sub(1, Ordering::Relaxed);
    }
}

pub(crate) const SNAPSHOT_FILENAME: &str = "sticky.bin";
pub(crate) const WAL_FILENAME: &str = "sticky.wal";
pub(crate) const COMPACT_MIN_INTERVAL_SECS: u64 = 60;

pub(crate) fn data_dir() -> PathBuf {
    DATA_DIR
        .lock()
        .expect("DATA_DIR lock poisoned")
        .get_or_init(|| {
            let dir = std::env::var("DATA_DIR").unwrap_or_else(|_| "./data".into());
            std::fs::create_dir_all(&dir).ok();
            PathBuf::from(dir)
        })
        .clone()
}

pub(crate) fn bin_path() -> PathBuf {
    data_dir().join(SNAPSHOT_FILENAME)
}

pub(crate) fn wal_path() -> PathBuf {
    data_dir().join(WAL_FILENAME)
}

pub(crate) async fn ensure_data_dir() -> std::io::Result<PathBuf> {
    let dir = data_dir();
    tokio::fs::create_dir_all(&dir).await?;
    Ok(dir)
}

fn quarantine_file(path: &std::path::Path) -> std::io::Result<PathBuf> {
    static QUAR_COUNTER: AtomicU64 = AtomicU64::new(0);
    let ts = now_u64() * 1000;
    let pid = std::process::id();
    for _ in 0..100 {
        let uniq = QUAR_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dest = PathBuf::from(format!("{}.corrupt-{ts}-{pid}-{uniq}", path.display()));
        if dest.exists() {
            continue;
        }
        std::fs::rename(path, &dest)?;
        snapshot::sync_parent_dir(path);
        return Ok(dest);
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "quarantine collision",
    ))
}

pub(crate) async fn tombstone_guild(guild_id: Id<GuildMarker>, now: u64) -> bool {
    if tombstones().contains_key(&guild_id) {
        return true;
    }
    let has_data = registry::with_guild(guild_id, |s| {
        !s.map.is_empty() || !s.config.roles.is_empty()
    })
    .await
    .unwrap_or(false);
    if !has_data {
        tombstones().remove(&guild_id);
        registry::remove_guild(guild_id).await;
        return false;
    }
    let mut payload = Vec::new();
    wal::encode_tombstone_payload(&mut payload, guild_id, now);
    let _order = wal_lock().lock().await;
    let inflight = match wal_append_locked(wal::OP_TOMBSTONE, &payload).await {
        Ok(inflight) => inflight,
        Err(e) => {
            tracing::warn!(error = ?e, "tombstone append failed; data kept in RAM");
            return false;
        }
    };
    tombstones().insert(guild_id, now);
    bump_ops();
    drop(inflight);
    drop(_order);
    maybe_compact().await;
    true
}

#[derive(Error, Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CancelOutcome {
    #[error("no-tombstone")]
    NoTombstone,
    #[error("cancelled-fresh")]
    CancelledFresh,
    #[error("expired-purged")]
    ExpiredPurged,
}

pub(crate) async fn cancel_tombstone(guild_id: Id<GuildMarker>, now: u64) -> CancelOutcome {
    let _order = wal_lock().lock().await;
    let Some(deleted_at) = tombstones().get(&guild_id).map(|r| *r) else {
        return CancelOutcome::NoTombstone;
    };
    if is_grace_lapsed(deleted_at, now) {
        let mut payload = Vec::new();
        wal::encode_purge_payload(&mut payload, guild_id);
        let Ok(inflight) = wal_append_locked(wal::OP_PURGE, &payload).await else {
            return CancelOutcome::ExpiredPurged;
        };
        tombstones().remove(&guild_id);
        registry::remove_guild(guild_id).await;
        bump_ops();
        drop(inflight);
        drop(_order);
        maybe_compact().await;
        return CancelOutcome::ExpiredPurged;
    }
    let mut payload = Vec::new();
    wal::encode_tombstone_payload(&mut payload, guild_id, 0);
    let Ok(inflight) = wal_append_locked(wal::OP_TOMBSTONE, &payload).await else {
        return CancelOutcome::CancelledFresh;
    };
    tombstones().remove(&guild_id);
    bump_ops();
    drop(inflight);
    drop(_order);
    maybe_compact().await;
    CancelOutcome::CancelledFresh
}

pub(crate) async fn sweep_tombstones(now: u64) -> Vec<Id<GuildMarker>> {
    let mut expired: Vec<Id<GuildMarker>> = tombstones()
        .iter()
        .filter(|e| is_grace_lapsed(*e.value(), now))
        .map(|e| *e.key())
        .collect();
    expired.sort_unstable();
    let mut purged = Vec::with_capacity(expired.len());
    for gid in expired {
        let _order = wal_lock().lock().await;
        let still = tombstones()
            .get(&gid)
            .map(|r| *r)
            .is_some_and(|d| is_grace_lapsed(d, now));
        if !still {
            continue;
        }
        let mut payload = Vec::new();
        wal::encode_purge_payload(&mut payload, gid);
        let Ok(inflight) = wal_append_locked(wal::OP_PURGE, &payload).await else {
            continue;
        };
        tombstones().remove(&gid);
        registry::remove_guild(gid).await;
        bump_ops();
        drop(inflight);
        drop(_order);
        purged.push(gid);
    }
    if !purged.is_empty() {
        maybe_compact().await;
    }
    purged
}

pub(crate) async fn load_all() -> Vec<Id<GuildMarker>> {
    let bin = bin_path();
    let wal_path = wal_path();
    let _snap = snapshot_lock().lock().await;

    let mut stores: FxHashMap<Id<GuildMarker>, GuildStore> = FxHashMap::default();
    let mut tombs: FxHashMap<Id<GuildMarker>, u64> = FxHashMap::default();

    let snapshot: Vec<(GuildStore, u64)> = {
        let bin_clone = bin.clone();
        let out = tokio::task::spawn_blocking(move || snapshot::read_snapshot(&bin_clone)).await;
        match out {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => {
                let msg = format!("{e:?}");
                if msg.contains("No such file") || msg.contains("NotFound") {
                    Vec::new()
                } else {
                    tracing::warn!(error = ?e, "snapshot corrupt; quarantining");
                    let q = tokio::task::spawn_blocking(move || quarantine_file(&bin))
                        .await
                        .map(|r| r.is_ok())
                        .unwrap_or(false);
                    tracing::warn!(quarantined = q, "starting empty, WAL replay recovers");
                    Vec::new()
                }
            }
            Err(e) => {
                tracing::warn!(error = ?e, "snapshot task failed; starting empty");
                Vec::new()
            }
        }
    };
    let now_u = now_u64();
    for (store, deleted_at) in snapshot {
        let gid = store.guild_id;
        if deleted_at != 0 && is_grace_lapsed(deleted_at, now_u) {
            continue;
        }
        if deleted_at != 0 {
            tombs.insert(gid, deleted_at);
        }
        stores.insert(gid, store);
    }

    match tokio::fs::read(&wal_path).await {
        Ok(bytes) if bytes.is_empty() => {
            TAIL_CHECKED.store(true, Ordering::Relaxed);
        }
        Ok(bytes) => {
            let out = wal::replay_wal(&bytes, &mut stores, &mut tombs);
            if out.corrupt_at.is_some() || out.stats.torn_tail {
                let at = out.corrupt_at.unwrap_or(out.stats.bytes_consumed);
                tracing::warn!(
                    at,
                    ops = out.stats.ops_applied,
                    "WAL truncated; prefix kept"
                );
                let p = wal_path.clone();
                let truncated = tokio::task::spawn_blocking(move || wal::truncate_to(&p, at)).await;
                if matches!(truncated, Ok(Ok(()))) {
                    TAIL_CHECKED.store(true, Ordering::Relaxed);
                }
            } else {
                TAIL_CHECKED.store(true, Ordering::Relaxed);
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            TAIL_CHECKED.store(true, Ordering::Relaxed);
        }
        Err(e) => {
            tracing::warn!(error = %e, "WAL read failed; keeping snapshot");
        }
    }

    let now_s = registry::now_secs();
    let mut ids = Vec::with_capacity(stores.len());
    for (gid, store) in stores {
        let deleted_at = tombs.get(&gid).copied().unwrap_or(0);
        if deleted_at != 0 && is_grace_lapsed(deleted_at, now_u) {
            continue;
        }
        if deleted_at != 0 {
            tombstones().insert(gid, deleted_at);
        }
        ids.push(gid);
        let mut guard = registry::stores().write().await;
        guard.entry(gid).or_insert(store);
    }
    ids.sort_unstable();
    // Persist load-time expiry through the WAL: sweeping only the in-RAM
    // copies above would let expired rows resurrect from replay after a
    // crash before the next hourly sweep. append_expire early-returns
    // without WAL I/O for guilds with nothing expired.
    drop(_snap);
    expire_sweep_all(now_s).await;
    ids
}

pub(crate) async fn save_snapshot_all() -> anyhow::Result<()> {
    let now_u = now_u64();
    let (entries, lapsed, ops_before) = {
        let _snap = snapshot_lock().lock().await;
        let guard = registry::stores().read().await;
        let mut entries = Vec::with_capacity(guard.len());
        let mut lapsed = Vec::new();
        for (gid, store) in guard.iter() {
            let deleted_at = tombstones().get(gid).map(|r| *r).unwrap_or(0);
            if deleted_at != 0 && is_grace_lapsed(deleted_at, now_u) {
                lapsed.push(*gid);
                continue;
            }
            entries.push(snapshot::SnapshotEntry {
                guild_id: *gid,
                deleted_at,
                store: store.clone(),
            });
        }
        (entries, lapsed, OPS_PENDING.load(Ordering::Relaxed))
    };

    let bin = bin_path();
    tokio::task::spawn_blocking(move || snapshot::write_snapshot_atomic(&bin, &entries))
        .await
        .context("snapshot join failed")?
        .context("snapshot write failed")?;

    {
        let _guard = wal_lock().lock().await;
        if OPS_PENDING.load(Ordering::Relaxed) != ops_before
            || WAL_INFLIGHT.load(Ordering::Relaxed) != 0
        {
            LAST_COMPACT_AT.store(now_u, Ordering::Relaxed);
            tracing::info!("snapshot written; WAL kept (concurrent appends)");
        } else {
            let wal = wal_path();
            tokio::task::spawn_blocking(move || {
                let r = match std::fs::remove_file(&wal) {
                    Ok(()) => Ok(()),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                    Err(e) => Err(e),
                };
                if r.is_ok() {
                    snapshot::sync_parent_dir(&wal);
                }
                r
            })
            .await
            .context("wal truncate join failed")?
            .context("wal truncate failed")?;
            OPS_PENDING.store(0, Ordering::Relaxed);
            LAST_COMPACT_AT.store(now_u, Ordering::Relaxed);
        }
    }
    for gid in lapsed {
        let still = tombstones()
            .get(&gid)
            .map(|r| *r)
            .is_some_and(|d| is_grace_lapsed(d, now_u64()));
        if !still {
            continue;
        }
        tombstones().remove(&gid);
        registry::remove_guild(gid).await;
    }
    Ok(())
}

async fn maybe_compact() {
    let ops = OPS_PENDING.load(Ordering::Relaxed) as usize;
    if ops == 0 {
        return;
    }
    if ops >= wal::WAL_COMPACT_OPS {
        if let Err(e) = save_snapshot_all().await {
            tracing::warn!(error = ?e, "compaction failed; will retry");
        }
        return;
    }
    if ops % 32 != 0 {
        return;
    }
    let bytes = tokio::fs::metadata(wal_path())
        .await
        .map(|m| m.len())
        .unwrap_or(0);
    if wal::should_compact(bytes, ops)
        && let Err(e) = save_snapshot_all().await
    {
        tracing::warn!(error = ?e, "compaction failed; will retry");
    }
}

pub(crate) async fn compact_check_all() -> usize {
    let now = now_u64();
    if now.saturating_sub(LAST_COMPACT_AT.load(Ordering::Relaxed)) < COMPACT_MIN_INTERVAL_SECS {
        return 0;
    }
    let ops = OPS_PENDING.load(Ordering::Relaxed) as usize;
    if ops == 0 {
        return 0;
    }
    let bytes = tokio::fs::metadata(wal_path())
        .await
        .map(|m| m.len())
        .unwrap_or(0);
    if !wal::should_compact(bytes, ops) {
        return 0;
    }
    match save_snapshot_all().await {
        Ok(()) => 1,
        Err(e) => {
            tracing::warn!(error = ?e, "periodic compaction failed; will retry");
            0
        }
    }
}

fn ensure_tail_checked_blocking(path: &std::path::Path) {
    if TAIL_CHECKED.load(Ordering::Relaxed) {
        return;
    }
    if !path.exists() {
        TAIL_CHECKED.store(true, Ordering::Relaxed);
        return;
    }
    if wal::truncate_torn_tail(path).is_ok() {
        TAIL_CHECKED.store(true, Ordering::Relaxed);
    }
}

/// Append one frame. Caller must hold [`wal_lock`]; the returned guard must be
/// held across the caller's RAM mutation and dropped only after [`bump_ops`]:
/// the snapshot unlink gate must never see a bumped counter for bytes whose
/// mutation hasn't landed yet.
async fn wal_append_locked(op: u8, payload: &[u8]) -> anyhow::Result<Inflight> {
    let path = wal_path();
    let frame = wal::encode_frame(op, payload);
    let inflight = Inflight::enter();
    let result = tokio::task::spawn_blocking(move || {
        ensure_tail_checked_blocking(&path);
        wal::append_frames_noscan(&path, &frame)
    })
    .await;
    match result
        .context("wal append join failed")?
        .context("wal append failed")
    {
        // Caller bumps after its RAM swap; dropping here would unlink-gate early.
        Ok(()) => Ok(inflight),
        // WAL gained nothing, so there is nothing for a snapshot to miss.
        Err(e) => Err(e),
    }
}

/// Append pre-encoded `batch` frames. Same contract as above, minus the
/// counter bump (caller bumps by frame count after its swap).
async fn wal_append_batch_locked(batch: &[u8]) -> anyhow::Result<Inflight> {
    let path = wal_path();
    let batch = batch.to_vec();
    let inflight = Inflight::enter();
    let result = tokio::task::spawn_blocking(move || {
        ensure_tail_checked_blocking(&path);
        wal::append_frames_noscan(&path, &batch)
    })
    .await;
    match result
        .context("wal append join failed")?
        .context("wal append failed")
    {
        Ok(()) => Ok(inflight),
        Err(e) => Err(e),
    }
}

pub(crate) async fn append_upsert(
    guild_id: Id<GuildMarker>,
    user_id: Id<UserMarker>,
    entry: &StickyEntry,
    nick: Option<&str>,
) -> anyhow::Result<bool> {
    if entry.left_at == 0 {
        anyhow::bail!("upsert left_at must be non-zero");
    }
    let entry = *entry;
    let nick_owned: Option<String> = nick
        .filter(|s| !s.is_empty())
        .map(GuildStore::truncate_nick)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_owned());
    let mut payload = Vec::new();
    wal::encode_upsert_payload(
        &mut payload,
        guild_id,
        user_id,
        &entry,
        nick_owned.as_deref(),
    );
    let _order = wal_lock().lock().await;
    let inflight = wal_append_locked(wal::OP_UPSERT, &payload).await?;

    let out = {
        let mut guard = registry::stores().write().await;
        let store = guard
            .entry(guild_id)
            .or_insert_with(|| GuildStore::new(guild_id, GuildConfig::default()));
        let mask = store.config.active_mask();
        let bits = entry.bits & mask;
        // Zero-bit rows live on only as nick-only rows; otherwise the user is gone.
        let has_nick = nick_owned.is_some();
        if bits == 0 && !has_nick {
            store.remove_user(user_id);
            false
        } else {
            store.map.insert(user_id, StickyEntry { bits, ..entry });
            store.set_nick(user_id, nick_owned.map(|s| s.into_boxed_str()));
            true
        }
    };
    bump_ops();
    drop(inflight);
    drop(_order);
    maybe_compact().await;
    Ok(out)
}

pub(crate) async fn append_delete(
    guild_id: Id<GuildMarker>,
    user_id: Id<UserMarker>,
) -> anyhow::Result<bool> {
    let mut payload = Vec::new();
    wal::encode_delete_payload(&mut payload, guild_id, user_id);
    let _order = wal_lock().lock().await;
    let inflight = wal_append_locked(wal::OP_DELETE, &payload).await?;
    let out = {
        let mut guard = registry::stores().write().await;
        match guard.get_mut(&guild_id) {
            Some(store) => store.remove_user(user_id),
            None => false,
        }
    };
    bump_ops();
    drop(inflight);
    drop(_order);
    maybe_compact().await;
    Ok(out)
}

pub(crate) async fn append_remap(
    guild_id: Id<GuildMarker>,
    role_id: Id<RoleMarker>,
) -> anyhow::Result<usize> {
    let mut payload = Vec::new();
    wal::encode_remap_payload(&mut payload, guild_id, role_id);
    let _order = wal_lock().lock().await;
    let inflight = wal_append_locked(wal::OP_REMAP, &payload).await?;
    let out = {
        let mut guard = registry::stores().write().await;
        match guard.get_mut(&guild_id) {
            Some(store) => store.remove_role(role_id),
            None => 0,
        }
    };
    bump_ops();
    drop(inflight);
    drop(_order);
    maybe_compact().await;
    Ok(out)
}

fn compute_next_config(
    cur_full: &[Id<RoleMarker>],
    cur_skipped: u32,
    roles: &[Id<RoleMarker>],
) -> (Vec<Id<RoleMarker>>, u32, [i16; 32]) {
    let wanted = GuildConfig::from_ids_dedup(roles).dict_ids();
    let old_mask = GuildConfig::mask_for_len(cur_full.len().min(super::MAX_ROLES));
    let old_skipped = cur_skipped & old_mask;
    let mut old_active: Vec<Id<RoleMarker>> = Vec::new();
    for (i, rid) in cur_full.iter().enumerate() {
        if (old_skipped >> i) & 1 == 0 {
            old_active.push(*rid);
        }
    }
    let mut newly_skipped: Vec<Id<RoleMarker>> = Vec::new();
    for rid in &old_active {
        if !wanted.contains(rid) {
            newly_skipped.push(*rid);
        }
    }
    let room = super::MAX_ROLES.saturating_sub(wanted.len());
    let active_n = wanted.len().min(super::MAX_ROLES);
    let mut next_full = wanted;
    next_full.extend(newly_skipped.into_iter().take(room));
    let mut next_skipped: u32 = 0;
    for i in active_n..next_full.len().min(32) {
        next_skipped |= super::bit(i);
    }
    let mut fwd: [i16; 32] = [-1; 32];
    for (i, rid) in cur_full.iter().enumerate().take(32) {
        for (j, nrid) in next_full.iter().enumerate() {
            if rid == nrid && j < 32 {
                fwd[i] = j as i16;
                break;
            }
        }
    }
    (next_full, next_skipped, fwd)
}

pub(crate) async fn append_config(
    guild_id: Id<GuildMarker>,
    roles: &[Id<RoleMarker>],
    log_channel: Option<Id<ChannelMarker>>,
    dont_save_if_ban_kicked: bool,
    include_bots: bool,
    save_nicknames: bool,
    expiry_secs: u32,
) -> anyhow::Result<(GuildConfig, usize)> {
    // Whole op holds one WAL lock: the pre-image read, the WAL bytes, and the
    // RAM swap observe a single atomic state (no concurrent writer forks live
    // vs replay).
    let _order = wal_lock().lock().await;
    let (cur_full, cur_skipped, old_save_nicks, cur_config) = {
        let guard = registry::stores().read().await;
        guard
            .get(&guild_id)
            .map(|s| {
                (
                    s.config.dict_ids(),
                    s.config.skipped_mask,
                    s.config.save_nicknames,
                    s.config.clone(),
                )
            })
            .unwrap_or((Vec::new(), 0, false, GuildConfig::default()))
    };
    let (next_ids, next_skipped, fwd) = compute_next_config(&cur_full, cur_skipped, roles);
    let mut next_config = GuildConfig::default();
    next_config.set_roles(&next_ids);
    next_config.skipped_mask = next_skipped & next_config.mask();
    next_config.log_channel = log_channel;
    next_config.dont_save_if_ban_kicked = dont_save_if_ban_kicked;
    next_config.include_bots = include_bots;
    next_config.save_nicknames = save_nicknames;
    next_config.expiry_secs = expiry_secs;

    if next_config == cur_config {
        return Ok((next_config, 0));
    }

    let mut config_payload = Vec::new();
    wal::encode_config_payload(&mut config_payload, guild_id, &next_config);
    let inflight = wal_append_locked(wal::OP_CONFIG, &config_payload).await?;

    let purged = {
        let mut guard = registry::stores().write().await;
        let store = guard
            .entry(guild_id)
            .or_insert_with(|| GuildStore::new(guild_id, GuildConfig::default()));
        store.config = next_config.clone();
        let mask = store.config.mask();
        for e in store.map.values_mut() {
            e.bits = wal::remap_bits_through_fwd(e.bits, &fwd, mask);
        }
        if !store.config.save_nicknames {
            store.nicks.clear();
        }
        store.purge_zeroed()
    };
    bump_ops();
    drop(inflight);
    drop(_order);

    if !old_save_nicks && save_nicknames {
        registry::mark_pending_chunk(guild_id).await;
    }
    maybe_compact().await;
    Ok((next_config, purged))
}

#[derive(Error, Debug)]
pub(crate) enum ImportRestoreError {
    #[error("{0}")]
    Rejected(#[from] super::csv::ImportReject),
    #[error("persist-io")]
    Persist,
}

impl From<anyhow::Error> for ImportRestoreError {
    fn from(_: anyhow::Error) -> Self {
        ImportRestoreError::Persist
    }
}

impl ImportRestoreError {
    pub(crate) fn kind_str(&self) -> &'static str {
        match self {
            Self::Rejected(r) => r.kind_str(),
            Self::Persist => "persist-io",
        }
    }

    pub(crate) fn bad_rows(&self) -> usize {
        match self {
            Self::Rejected(r) => r.bad_rows,
            Self::Persist => 0,
        }
    }
}

pub(crate) async fn append_import_replace(
    guild_id: Id<GuildMarker>,
    csv_text: &str,
    expiry_secs: u32,
    is_member: impl Fn(Id<UserMarker>) -> bool + Send + Sync,
) -> Result<super::csv::ImportReport, ImportRestoreError> {
    if csv_text.len() as u64 > super::csv::MAX_CSV_BYTES {
        return Err(
            super::csv::ImportReject::new(super::csv::ImportRejectKind::BodyTooLarge, 0).into(),
        );
    }

    // Phase 1 (no WAL lock): CSV parse + diff is the expensive part (up to
    // 25k rows) and must not head-of-line-block every other guild's WAL
    // appends. Only the append+swap below needs the lock.
    let now = registry::now_secs();
    let pre = {
        let guard = registry::stores().read().await;
        guard
            .get(&guild_id)
            .cloned()
            .unwrap_or_else(|| GuildStore::new(guild_id, GuildConfig::default()))
    };
    let mut full = pre.clone();
    let mut report =
        super::import_replace(&mut full, csv_text, guild_id, now, expiry_secs, &is_member)?;
    let mut new_cfg = full.config.clone();
    let mut new_map = std::mem::take(&mut full.map);
    let mut new_nicks = std::mem::take(&mut full.nicks);
    if new_cfg == pre.config && new_map == pre.map && new_nicks == pre.nicks {
        return Ok(report);
    }

    // Phase 2 (WAL lock held): single atomic batch + RAM swap observe one
    // state. The batch is one write_all+fsync so a crash replays the whole
    // replace or nothing (torn tail is truncated). Stale deletes are computed
    // against live — not the phase-1 pre-image — so a writer that raced the
    // parse can't fork live vs replay.
    let _order = wal_lock().lock().await;
    {
        let guard = registry::stores().read().await;
        let live_active_changed = guard
            .get(&guild_id)
            .map(|s| {
                s.config.dict_ids() != pre.config.dict_ids()
                    || s.config.save_nicknames != pre.config.save_nicknames
            })
            .unwrap_or(!pre.map.is_empty() || !pre.config.roles.is_empty());
        if live_active_changed {
            // Rare: config raced the parse; re-resolve against live so bits
            // match the dict the CONFIG frame will carry.
            let mut relive = guard
                .get(&guild_id)
                .cloned()
                .unwrap_or_else(|| GuildStore::new(guild_id, GuildConfig::default()));
            drop(guard);
            report = super::import_replace(
                &mut relive,
                csv_text,
                guild_id,
                now,
                expiry_secs,
                &is_member,
            )?;
            new_cfg = relive.config.clone();
            new_map = std::mem::take(&mut relive.map);
            new_nicks = std::mem::take(&mut relive.nicks);
        }
    }
    let mut users: Vec<Id<UserMarker>> = new_map.keys().copied().collect();
    users.sort_unstable();
    let stale: Vec<Id<UserMarker>> = {
        let guard = registry::stores().read().await;
        match guard.get(&guild_id) {
            Some(live) => live
                .map
                .keys()
                .filter(|uid| !new_map.contains_key(uid))
                .copied()
                .collect(),
            None => pre
                .map
                .keys()
                .filter(|uid| !new_map.contains_key(uid))
                .copied()
                .collect(),
        }
    };
    let mut stale = stale;
    stale.sort_unstable();

    let frames = (1 + users.len() + stale.len()) as u64;
    let mut batch = Vec::new();
    let mut payload = Vec::new();
    wal::encode_frame_into(
        &mut batch,
        wal::OP_CONFIG,
        wal::encode_config_payload(&mut payload, guild_id, &new_cfg),
    );
    payload.clear();
    for uid in &users {
        let entry = &new_map[uid];
        let nick = new_nicks.get(uid).map(|s| &**s);
        wal::encode_frame_into(
            &mut batch,
            wal::OP_UPSERT,
            wal::encode_upsert_payload(&mut payload, guild_id, *uid, entry, nick),
        );
        payload.clear();
    }
    for uid in &stale {
        wal::encode_frame_into(
            &mut batch,
            wal::OP_DELETE,
            wal::encode_delete_payload(&mut payload, guild_id, *uid),
        );
        payload.clear();
    }
    let inflight = wal_append_batch_locked(&batch).await?;

    {
        let mut guard = registry::stores().write().await;
        let store = guard
            .entry(guild_id)
            .or_insert_with(|| GuildStore::new(guild_id, GuildConfig::default()));
        store.config = new_cfg;
        store.map = new_map;
        store.nicks = new_nicks;
    }
    OPS_PENDING.fetch_add(frames, Ordering::Relaxed);
    drop(inflight);
    drop(_order);
    maybe_compact().await;
    Ok(report)
}

pub(crate) async fn append_expire(
    guild_id: Id<GuildMarker>,
    now_secs: u32,
) -> anyhow::Result<usize> {
    let expiry = registry::with_guild(guild_id, |s| s.config.expiry_secs)
        .await
        .unwrap_or(0);
    if expiry == 0 {
        return Ok(0);
    }
    let needs = registry::with_guild(guild_id, |s| {
        s.map
            .values()
            .any(|e| GuildStore::is_expired(e.left_at, now_secs, expiry))
    })
    .await
    .unwrap_or(false);
    if !needs {
        return Ok(0);
    }

    let _order = wal_lock().lock().await;
    // Re-validate under the lock and encode from the live config, so the WAL
    // bytes, the RAM sweep, and a later replay all agree on one expiry.
    let live_expiry = {
        let guard = registry::stores().read().await;
        match guard.get(&guild_id) {
            Some(s) => {
                let expiry = s.config.expiry_secs;
                (expiry != 0
                    && s.map
                        .values()
                        .any(|e| GuildStore::is_expired(e.left_at, now_secs, expiry)))
                .then_some(expiry)
            }
            None => None,
        }
    };
    let Some(expiry) = live_expiry else {
        return Ok(0);
    };
    let mut expire_payload = Vec::new();
    wal::encode_expire_payload(&mut expire_payload, guild_id, now_secs, expiry);
    let inflight = wal_append_locked(wal::OP_EXPIRE, &expire_payload).await?;

    let removed = {
        let mut guard = registry::stores().write().await;
        match guard.get_mut(&guild_id) {
            Some(store) => store.expire_sweep(now_secs),
            None => 0,
        }
    };
    bump_ops();
    drop(inflight);
    drop(_order);
    maybe_compact().await;
    Ok(removed)
}

pub(crate) async fn expire_sweep_all(now_secs: u32) -> usize {
    let mut total = 0usize;
    for gid in registry::guild_ids().await {
        match append_expire(gid, now_secs).await {
            Ok(n) => total += n,
            Err(e) => {
                tracing::warn!(error = ?e, "expire sweep failed for guild; kept")
            }
        }
    }
    total
}
