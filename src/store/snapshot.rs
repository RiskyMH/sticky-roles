use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, ensure};
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};
use twilight_model::id::{
    Id,
    marker::{ChannelMarker, GuildMarker, RoleMarker, UserMarker},
};

use crate::store::{GuildConfig, GuildStore, MAX_NICK_BYTES, MAX_ROLES, StickyEntry};

pub(crate) const MAGIC: [u8; 4] = *b"STKY";
pub(crate) const VERSION: u16 = 1;
pub(crate) const HEADER_LEN: usize = 6;
pub(crate) const CRC_LEN: usize = 4;
pub(crate) const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone)]
pub(crate) struct SnapshotEntry {
    pub(crate) guild_id: Id<GuildMarker>,
    pub(crate) deleted_at: u64,
    pub(crate) store: GuildStore,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
struct SnapshotFile {
    format: u16,
    guilds: Vec<GuildWire>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
struct GuildWire {
    guild_id: u64,
    deleted_at: u64,
    log_channel: u64,
    expiry_secs: u32,
    flags: u8,
    roles: Vec<u64>,
    skipped_mask: u32,
    records: Vec<RecordWire>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
struct RecordWire {
    user: u64,
    bits: u32,
    left_at: u32,
    nick: Option<String>,
}

fn wire_nick(nick: Option<&str>) -> Option<String> {
    nick.filter(|s| !s.is_empty())
        .map(GuildStore::truncate_nick)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_owned())
}

pub(crate) fn encode_snapshot(entries: &[SnapshotEntry]) -> Vec<u8> {
    let mut order: Vec<usize> = (0..entries.len()).collect();
    order.sort_unstable_by_key(|&i| entries[i].guild_id.get());
    let mut guilds: Vec<GuildWire> = Vec::with_capacity(order.len());
    for i in order {
        let entry = &entries[i];
        let store = &entry.store;
        let mut ids: Vec<Id<UserMarker>> = store.map.keys().copied().collect();
        ids.sort_unstable();
        let mut records: Vec<RecordWire> = Vec::with_capacity(ids.len());
        for uid in ids {
            if let Some(e) = store.map.get(&uid) {
                records.push(RecordWire {
                    user: uid.get(),
                    bits: e.bits,
                    left_at: e.left_at,
                    nick: wire_nick(store.nick_for(uid)),
                });
            }
        }
        guilds.push(GuildWire {
            guild_id: entry.guild_id.get(),
            deleted_at: entry.deleted_at,
            log_channel: store.config.log_channel.map(|c| c.get()).unwrap_or(0),
            expiry_secs: store.config.expiry_secs,
            flags: store.config.flags(),
            roles: store.config.dict_ids().iter().map(|id| id.get()).collect(),
            skipped_mask: store.config.skipped_mask & store.config.mask(),
            records,
        });
    }
    let body = postcard::to_allocvec(&SnapshotFile {
        format: VERSION,
        guilds,
    })
    .expect("postcard snapshot encode infallible");
    let mut out = Vec::with_capacity(HEADER_LEN + body.len() + CRC_LEN);
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&body);
    let crc = crc32fast::hash(&out);
    out.extend_from_slice(&crc.to_le_bytes());
    out
}

fn guild_to_store(wire: GuildWire) -> anyhow::Result<(GuildStore, u64)> {
    let guild_id =
        Id::new_checked(wire.guild_id).ok_or_else(|| anyhow::anyhow!("guild id nonzero"))?;
    ensure!(wire.flags & !0b111 == 0, "unknown config flags");
    ensure!(wire.roles.len() <= MAX_ROLES, "dict too long");
    let mut role_ids: Vec<Id<RoleMarker>> = Vec::with_capacity(wire.roles.len());
    for raw in &wire.roles {
        let id = Id::new_checked(*raw).ok_or_else(|| anyhow::anyhow!("dict role id nonzero"))?;
        ensure!(!role_ids.contains(&id), "duplicate dict role");
        role_ids.push(id);
    }
    let total_mask = GuildConfig::mask_for_len(role_ids.len());
    ensure!(
        wire.skipped_mask & !total_mask == 0,
        "skipped mask beyond dict"
    );
    let mut map: FxHashMap<Id<UserMarker>, StickyEntry> = FxHashMap::default();
    let mut nicks: FxHashMap<Id<UserMarker>, Box<str>> = FxHashMap::default();
    let mut last_uid: u64 = 0;
    for rec in &wire.records {
        let uid =
            Id::new_checked(rec.user).ok_or_else(|| anyhow::anyhow!("record user nonzero"))?;
        ensure!(uid.get() > last_uid, "users unordered");
        last_uid = uid.get();
        ensure!(rec.bits & !total_mask == 0, "record bits beyond dict");
        if let Some(n) = &rec.nick {
            ensure!(n.len() <= MAX_NICK_BYTES, "nick too long");
            if !n.is_empty() {
                nicks.insert(uid, n.as_str().into());
            }
        }
        map.insert(
            uid,
            StickyEntry {
                bits: rec.bits,
                left_at: rec.left_at,
            },
        );
    }
    let mut config = GuildConfig::from_ids_dedup(&role_ids);
    config.log_channel = Id::<ChannelMarker>::new_checked(wire.log_channel);
    let (no_kicks, bots, nicks_flag) = GuildConfig::from_flags(wire.flags);
    config.dont_save_if_ban_kicked = no_kicks;
    config.include_bots = bots;
    config.save_nicknames = nicks_flag;
    config.expiry_secs = wire.expiry_secs;
    config.skipped_mask = wire.skipped_mask;
    Ok((
        GuildStore {
            guild_id,
            config,
            map,
            nicks,
        },
        wire.deleted_at,
    ))
}

pub(crate) fn decode_snapshot(bytes: &[u8]) -> anyhow::Result<Vec<(GuildStore, u64)>> {
    ensure!(!bytes.is_empty(), "snapshot empty");
    ensure!(bytes.len() >= HEADER_LEN + CRC_LEN, "snapshot too short");
    ensure!(bytes[0..4] == MAGIC, "bad magic");
    let version = u16::from_le_bytes([bytes[4], bytes[5]]);
    ensure!(version == VERSION, "unsupported version {version}");
    let body_end = bytes.len() - CRC_LEN;
    let stored = u32::from_le_bytes([
        bytes[body_end],
        bytes[body_end + 1],
        bytes[body_end + 2],
        bytes[body_end + 3],
    ]);
    ensure!(
        stored == crc32fast::hash(&bytes[..body_end]),
        "snapshot crc mismatch"
    );
    let body = &bytes[HEADER_LEN..body_end];
    ensure!(body.len() <= MAX_BODY_BYTES, "snapshot body too large");
    let (file, rest): (SnapshotFile, &[u8]) =
        postcard::take_from_bytes(body).context("snapshot decode failed")?;
    ensure!(rest.is_empty(), "trailing bytes");
    ensure!(file.format == VERSION, "unsupported format");
    let mut out = Vec::with_capacity(file.guilds.len().min(1024));
    let mut last_gid: u64 = 0;
    for wire in file.guilds {
        let (store, deleted_at) = guild_to_store(wire)?;
        ensure!(store.guild_id.get() > last_gid, "guilds unordered");
        last_gid = store.guild_id.get();
        out.push((store, deleted_at));
    }
    Ok(out)
}

pub(crate) fn sync_parent_dir(path: &Path) {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    if let Ok(dir) = File::open(parent) {
        let _ = dir.sync_all();
    }
}

pub(crate) fn write_snapshot_atomic(path: &Path, entries: &[SnapshotEntry]) -> std::io::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let bytes = encode_snapshot(entries);
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut last_err: Option<std::io::Error> = None;
    for _ in 0..10 {
        let uniq = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let tmp = PathBuf::from(format!("{}.tmp.{pid}.{nanos}.{uniq}", path.display()));
        let mut created = false;
        let res = (|| {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp)?;
            created = true;
            f.write_all(&bytes)?;
            f.sync_all()?;
            drop(f);
            std::fs::rename(&tmp, path)?;
            sync_parent_dir(path);
            Ok::<(), std::io::Error>(())
        })();
        match res {
            Ok(()) => return Ok(()),
            Err(e) => {
                if created {
                    let _ = std::fs::remove_file(&tmp);
                }
                if e.kind() == std::io::ErrorKind::AlreadyExists {
                    last_err = Some(e);
                    continue;
                }
                return Err(e);
            }
        }
    }
    Err(last_err
        .unwrap_or_else(|| std::io::Error::new(std::io::ErrorKind::AlreadyExists, "tmp collision")))
}

pub(crate) fn read_snapshot(path: &Path) -> anyhow::Result<Vec<(GuildStore, u64)>> {
    let bytes = std::fs::read(path).with_context(|| "snapshot read failed")?;
    decode_snapshot(&bytes).context("snapshot decode failed")
}
