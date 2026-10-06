use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, ensure};
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use twilight_model::id::{
    Id,
    marker::{ChannelMarker, GuildMarker, RoleMarker, UserMarker},
};

use crate::store::{GuildConfig, GuildStore, MAX_NICK_BYTES, MAX_ROLES, StickyEntry};

pub(crate) const OP_UPSERT: u8 = 1;
pub(crate) const OP_DELETE: u8 = 2;
pub(crate) const OP_REMAP: u8 = 3;
pub(crate) const OP_EXPIRE: u8 = 5;
pub(crate) const OP_CONFIG: u8 = 7;
pub(crate) const OP_TOMBSTONE: u8 = 8;
pub(crate) const OP_PURGE: u8 = 9;

pub(crate) const FRAME_OVERHEAD: usize = 9;
pub(crate) const MAX_FRAME_PAYLOAD: usize = 1024;
pub(crate) const WAL_COMPACT_BYTES: u64 = 4 * 1024 * 1024;
pub(crate) const WAL_COMPACT_OPS: usize = 50_000;

pub(crate) fn should_compact(wal_bytes: u64, ops: usize) -> bool {
    wal_bytes >= WAL_COMPACT_BYTES || ops >= WAL_COMPACT_OPS
}

pub(crate) fn is_known_op(op: u8) -> bool {
    matches!(
        op,
        OP_UPSERT | OP_DELETE | OP_REMAP | OP_EXPIRE | OP_CONFIG | OP_TOMBSTONE | OP_PURGE
    )
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub(crate) struct UpsertPayload {
    pub(crate) guild: u64,
    pub(crate) user: u64,
    pub(crate) bits: u32,
    pub(crate) left_at: u32,
    pub(crate) nick: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub(crate) struct DeletePayload {
    pub(crate) guild: u64,
    pub(crate) user: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub(crate) struct RemapPayload {
    pub(crate) guild: u64,
    pub(crate) role: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub(crate) struct ExpirePayload {
    pub(crate) guild: u64,
    pub(crate) now_secs: u32,
    pub(crate) expiry_secs: u32,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub(crate) struct ConfigPayload {
    pub(crate) guild: u64,
    pub(crate) log_channel: u64,
    pub(crate) expiry_secs: u32,
    pub(crate) flags: u8,
    pub(crate) roles: Vec<u64>,
    pub(crate) skipped_mask: u32,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub(crate) struct TombstonePayload {
    pub(crate) guild: u64,
    pub(crate) deleted_at: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub(crate) struct PurgePayload {
    pub(crate) guild: u64,
}

pub(crate) fn frame_crc(op: u8, payload: &[u8]) -> u32 {
    let mut h = crc32fast::Hasher::new();
    h.update(&[op]);
    h.update(&(payload.len() as u32).to_le_bytes());
    h.update(payload);
    h.finalize()
}

pub(crate) fn encode_frame_into(buf: &mut Vec<u8>, op: u8, payload: &[u8]) {
    // Enforced in release too: replay treats len > MAX as corruption and
    // truncates the tail, so an oversize write would brick everything after it.
    assert!(
        payload.len() <= MAX_FRAME_PAYLOAD,
        "wal frame payload {} exceeds {MAX_FRAME_PAYLOAD}",
        payload.len()
    );
    buf.push(op);
    buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    buf.extend_from_slice(payload);
    buf.extend_from_slice(&frame_crc(op, payload).to_le_bytes());
}

pub(crate) fn encode_frame(op: u8, payload: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(FRAME_OVERHEAD + payload.len());
    encode_frame_into(&mut buf, op, payload);
    buf
}

fn push_postcard<'a, T: Serialize>(buf: &'a mut Vec<u8>, value: &T) -> &'a [u8] {
    let start = buf.len();
    let enc = postcard::to_allocvec(value).expect("wal encode infallible");
    buf.extend_from_slice(&enc);
    &buf[start..]
}

pub(crate) fn encode_upsert_payload<'a>(
    buf: &'a mut Vec<u8>,
    guild_id: Id<GuildMarker>,
    user_id: Id<UserMarker>,
    entry: &StickyEntry,
    nick: Option<&str>,
) -> &'a [u8] {
    let wire: Option<String> = nick
        .filter(|s| !s.is_empty())
        .map(GuildStore::truncate_nick)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_owned());
    push_postcard(
        buf,
        &UpsertPayload {
            guild: guild_id.get(),
            user: user_id.get(),
            bits: entry.bits,
            left_at: entry.left_at,
            nick: wire,
        },
    )
}

pub(crate) fn encode_delete_payload(
    buf: &mut Vec<u8>,
    guild_id: Id<GuildMarker>,
    user_id: Id<UserMarker>,
) -> &[u8] {
    push_postcard(
        buf,
        &DeletePayload {
            guild: guild_id.get(),
            user: user_id.get(),
        },
    )
}

pub(crate) fn encode_remap_payload(
    buf: &mut Vec<u8>,
    guild_id: Id<GuildMarker>,
    role_id: Id<RoleMarker>,
) -> &[u8] {
    push_postcard(
        buf,
        &RemapPayload {
            guild: guild_id.get(),
            role: role_id.get(),
        },
    )
}

pub(crate) fn encode_expire_payload(
    buf: &mut Vec<u8>,
    guild_id: Id<GuildMarker>,
    now_secs: u32,
    expiry_secs: u32,
) -> &[u8] {
    push_postcard(
        buf,
        &ExpirePayload {
            guild: guild_id.get(),
            now_secs,
            expiry_secs,
        },
    )
}

pub(crate) fn encode_config_payload<'a>(
    buf: &'a mut Vec<u8>,
    guild_id: Id<GuildMarker>,
    config: &GuildConfig,
) -> &'a [u8] {
    push_postcard(
        buf,
        &ConfigPayload {
            guild: guild_id.get(),
            log_channel: config.log_channel.map(|c| c.get()).unwrap_or(0),
            expiry_secs: config.expiry_secs,
            flags: config.flags(),
            roles: config.dict_ids().iter().map(|id| id.get()).collect(),
            skipped_mask: config.skipped_mask & config.mask(),
        },
    )
}

pub(crate) fn encode_tombstone_payload(
    buf: &mut Vec<u8>,
    guild_id: Id<GuildMarker>,
    deleted_at: u64,
) -> &[u8] {
    push_postcard(
        buf,
        &TombstonePayload {
            guild: guild_id.get(),
            deleted_at,
        },
    )
}

pub(crate) fn encode_purge_payload(buf: &mut Vec<u8>, guild_id: Id<GuildMarker>) -> &[u8] {
    push_postcard(
        buf,
        &PurgePayload {
            guild: guild_id.get(),
        },
    )
}

enum Decoded {
    Upsert {
        guild: Id<GuildMarker>,
        user: Id<UserMarker>,
        bits: u32,
        left_at: u32,
        nick: Option<String>,
    },
    Delete {
        guild: Id<GuildMarker>,
        user: Id<UserMarker>,
    },
    Remap {
        guild: Id<GuildMarker>,
        role: Id<RoleMarker>,
    },
    Expire {
        guild: Id<GuildMarker>,
        now_secs: u32,
        expiry_secs: u32,
    },
    Config {
        guild: Id<GuildMarker>,
        log_channel: Option<Id<ChannelMarker>>,
        expiry_secs: u32,
        flags: u8,
        roles: Vec<Id<RoleMarker>>,
        skipped_mask: u32,
    },
    Tombstone {
        guild: Id<GuildMarker>,
        deleted_at: u64,
    },
    Purge {
        guild: Id<GuildMarker>,
    },
}

fn take<T: DeserializeOwned>(payload: &[u8], what: &str) -> anyhow::Result<T> {
    // Trailing bytes are a newer field append, not corruption: the frame CRC
    // already authenticated the whole payload, so ignore them (forward compat
    // for additive fields; the snapshot stays strict and version-gated).
    let (v, _rest): (T, &[u8]) =
        postcard::take_from_bytes(payload).with_context(|| format!("{what} decode failed"))?;
    Ok(v)
}

fn decode_payload(op: u8, payload: &[u8]) -> anyhow::Result<Decoded> {
    let guild_of = |g: u64| Id::new_checked(g).ok_or_else(|| anyhow::anyhow!("guild id nonzero"));
    match op {
        OP_UPSERT => {
            let w: UpsertPayload = take(payload, "upsert")?;
            let guild = guild_of(w.guild)?;
            let user = Id::new_checked(w.user).ok_or_else(|| anyhow::anyhow!("user id nonzero"))?;
            ensure!(w.left_at != 0, "upsert left_at nonzero");
            ensure!(
                w.nick.as_ref().map(|n| n.len()).unwrap_or(0) <= MAX_NICK_BYTES,
                "nick too long"
            );
            Ok(Decoded::Upsert {
                guild,
                user,
                bits: w.bits,
                left_at: w.left_at,
                nick: w.nick,
            })
        }
        OP_DELETE => {
            let w: DeletePayload = take(payload, "delete")?;
            Ok(Decoded::Delete {
                guild: guild_of(w.guild)?,
                user: Id::new_checked(w.user).ok_or_else(|| anyhow::anyhow!("user id nonzero"))?,
            })
        }
        OP_REMAP => {
            let w: RemapPayload = take(payload, "remap")?;
            Ok(Decoded::Remap {
                guild: guild_of(w.guild)?,
                role: Id::new_checked(w.role).ok_or_else(|| anyhow::anyhow!("role id nonzero"))?,
            })
        }
        OP_EXPIRE => {
            let w: ExpirePayload = take(payload, "expire")?;
            Ok(Decoded::Expire {
                guild: guild_of(w.guild)?,
                now_secs: w.now_secs,
                expiry_secs: w.expiry_secs,
            })
        }
        OP_CONFIG => {
            let w: ConfigPayload = take(payload, "config")?;
            let guild = guild_of(w.guild)?;
            ensure!(w.flags & !0b111 == 0, "unknown flags");
            ensure!(w.roles.len() <= MAX_ROLES, "dict too long");
            let mut roles: Vec<Id<RoleMarker>> = Vec::with_capacity(w.roles.len());
            for raw in &w.roles {
                let id = Id::new_checked(*raw).ok_or_else(|| anyhow::anyhow!("role id nonzero"))?;
                ensure!(!roles.contains(&id), "duplicate role");
                roles.push(id);
            }
            ensure!(
                w.skipped_mask & !GuildConfig::mask_for_len(roles.len()) == 0,
                "skipped beyond dict"
            );
            Ok(Decoded::Config {
                guild,
                log_channel: Id::<ChannelMarker>::new_checked(w.log_channel),
                expiry_secs: w.expiry_secs,
                flags: w.flags,
                roles,
                skipped_mask: w.skipped_mask,
            })
        }
        OP_TOMBSTONE => {
            let w: TombstonePayload = take(payload, "tombstone")?;
            Ok(Decoded::Tombstone {
                guild: guild_of(w.guild)?,
                deleted_at: w.deleted_at,
            })
        }
        OP_PURGE => {
            let w: PurgePayload = take(payload, "purge")?;
            Ok(Decoded::Purge {
                guild: guild_of(w.guild)?,
            })
        }
        other => Err(anyhow::anyhow!("unknown op {other}")),
    }
}

pub(crate) fn remap_bits_through_fwd(bits: u32, fwd: &[i16; 32], mask: u32) -> u32 {
    let mut next: u32 = 0;
    let mut b = bits;
    while b != 0 {
        let i = b.trailing_zeros() as usize;
        if i < 32 {
            let nj = fwd[i];
            if nj >= 0 && (nj as usize) < 32 {
                next |= 1u32 << (nj as u32);
            }
        }
        b &= b - 1;
    }
    next & mask
}

fn store_for(
    stores: &mut FxHashMap<Id<GuildMarker>, GuildStore>,
    guild_id: Id<GuildMarker>,
) -> &mut GuildStore {
    stores
        .entry(guild_id)
        .or_insert_with(|| GuildStore::new(guild_id, GuildConfig::default()))
}

fn apply_upsert(
    stores: &mut FxHashMap<Id<GuildMarker>, GuildStore>,
    guild: Id<GuildMarker>,
    user: Id<UserMarker>,
    bits: u32,
    left_at: u32,
    nick: Option<String>,
) {
    let active = stores
        .get(&guild)
        .map(|s| s.config.active_mask())
        .unwrap_or(0);
    let bits = bits & active;
    // Mirrors append_upsert: zero-bit rows live on only as nick-only rows.
    if bits == 0 && nick.is_none() {
        if let Some(store) = stores.get_mut(&guild) {
            store.remove_user(user);
        }
        return;
    }
    let store = store_for(stores, guild);
    store.map.insert(user, StickyEntry { bits, left_at });
    store.set_nick(user, nick.map(|s| s.into_boxed_str()));
}

fn apply_config(
    stores: &mut FxHashMap<Id<GuildMarker>, GuildStore>,
    guild: Id<GuildMarker>,
    log_channel: Option<Id<ChannelMarker>>,
    expiry_secs: u32,
    flags: u8,
    roles: &[Id<RoleMarker>],
    skipped_mask: u32,
) {
    let mut next = GuildConfig::from_ids_dedup(roles);
    next.log_channel = log_channel;
    next.expiry_secs = expiry_secs;
    let (a, b, c) = GuildConfig::from_flags(flags);
    next.dont_save_if_ban_kicked = a;
    next.include_bots = b;
    next.save_nicknames = c;
    next.skipped_mask = skipped_mask;
    let new_ids = next.dict_ids();
    let store = store_for(stores, guild);
    let mut fwd: [i16; 32] = [-1; 32];
    for (i, rid) in store.config.dict_ids().iter().enumerate().take(32) {
        if let Some(j) = new_ids.iter().position(|n| n == rid)
            && j < 32
        {
            fwd[i] = j as i16;
        }
    }
    store.config = next;
    let total = store.config.mask();
    for e in store.map.values_mut() {
        e.bits = remap_bits_through_fwd(e.bits, &fwd, total);
    }
    if !store.config.save_nicknames {
        store.nicks.clear();
    }
    store.purge_zeroed();
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReplayStats {
    pub(crate) ops_applied: usize,
    pub(crate) unknown_skipped: usize,
    pub(crate) torn_tail: bool,
    pub(crate) bytes_consumed: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReplayOutcome {
    pub(crate) stats: ReplayStats,
    pub(crate) corrupt_at: Option<usize>,
}

pub(crate) fn replay_wal(
    bytes: &[u8],
    stores: &mut FxHashMap<Id<GuildMarker>, GuildStore>,
    tombs: &mut FxHashMap<Id<GuildMarker>, u64>,
) -> ReplayOutcome {
    let done = |ops, unknown, torn, consumed, corrupt| ReplayOutcome {
        stats: ReplayStats {
            ops_applied: ops,
            unknown_skipped: unknown,
            torn_tail: torn,
            bytes_consumed: consumed,
        },
        corrupt_at: corrupt,
    };
    let mut off: usize = 0;
    let (mut ops, mut unknown) = (0usize, 0usize);
    while off < bytes.len() {
        if bytes.len() - off < 5 {
            return done(ops, unknown, true, off, None);
        }
        let op = bytes[off];
        let len = u32::from_le_bytes([
            bytes[off + 1],
            bytes[off + 2],
            bytes[off + 3],
            bytes[off + 4],
        ]) as usize;
        if len > MAX_FRAME_PAYLOAD {
            return done(ops, unknown, false, off, Some(off));
        }
        if bytes.len() - off < 5 + len + 4 {
            return done(ops, unknown, true, off, None);
        }
        let payload = &bytes[off + 5..off + 5 + len];
        let stored = u32::from_le_bytes([
            bytes[off + 5 + len],
            bytes[off + 5 + len + 1],
            bytes[off + 5 + len + 2],
            bytes[off + 5 + len + 3],
        ]);
        if stored != frame_crc(op, payload) {
            return done(ops, unknown, false, off, Some(off));
        }
        if !is_known_op(op) {
            unknown += 1;
            off += 5 + len + 4;
            continue;
        }
        match decode_payload(op, payload) {
            Ok(Decoded::Upsert {
                guild,
                user,
                bits,
                left_at,
                nick,
            }) => apply_upsert(stores, guild, user, bits, left_at, nick),
            Ok(Decoded::Delete { guild, user }) => {
                if let Some(store) = stores.get_mut(&guild) {
                    store.remove_user(user);
                }
            }
            Ok(Decoded::Remap { guild, role }) => {
                if let Some(store) = stores.get_mut(&guild) {
                    store.remove_role(role);
                }
            }
            Ok(Decoded::Expire {
                guild,
                now_secs,
                expiry_secs,
            }) => {
                if expiry_secs != 0
                    && let Some(store) = stores.get_mut(&guild)
                {
                    let gone: Vec<Id<UserMarker>> = store
                        .map
                        .iter()
                        .filter(|(_, e)| GuildStore::is_expired(e.left_at, now_secs, expiry_secs))
                        .map(|(uid, _)| *uid)
                        .collect();
                    for uid in gone {
                        store.remove_user(uid);
                    }
                }
            }
            Ok(Decoded::Config {
                guild,
                log_channel,
                expiry_secs,
                flags,
                roles,
                skipped_mask,
            }) => apply_config(
                stores,
                guild,
                log_channel,
                expiry_secs,
                flags,
                &roles,
                skipped_mask,
            ),
            Ok(Decoded::Tombstone { guild, deleted_at }) => {
                if deleted_at == 0 {
                    tombs.remove(&guild);
                } else {
                    tombs.insert(guild, deleted_at);
                }
            }
            Ok(Decoded::Purge { guild }) => {
                stores.remove(&guild);
                tombs.remove(&guild);
            }
            Err(_) => return done(ops, unknown, false, off, Some(off)),
        }
        ops += 1;
        off += 5 + len + 4;
    }
    done(ops, unknown, false, off, None)
}

pub(crate) fn truncate_to(path: &Path, offset: usize) -> std::io::Result<()> {
    let f = std::fs::OpenOptions::new().write(true).open(path)?;
    f.set_len(offset as u64)?;
    f.sync_all()
}

pub(crate) fn truncate_torn_tail(path: &Path) -> std::io::Result<usize> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    let out = replay_wal(&bytes, &mut FxHashMap::default(), &mut FxHashMap::default());
    let good = out.corrupt_at.unwrap_or(out.stats.bytes_consumed);
    if good < bytes.len() {
        truncate_to(path, good)?;
        Ok(bytes.len() - good)
    } else {
        Ok(0)
    }
}

pub(crate) fn append_frames_noscan(path: &Path, frames: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    f.write_all(frames)?;
    f.sync_all()
}
