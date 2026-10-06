use rustc_hash::FxHashMap;
use twilight_model::id::{
    Id,
    marker::{ChannelMarker, GuildMarker, RoleMarker, UserMarker},
};

pub mod csv;
pub mod persist;
pub mod registry;
pub mod snapshot;
pub mod wal;

pub const DURATION_1D_SECS: u32 = 86_400;
pub const DURATION_1W_SECS: u32 = 604_800;
pub const DURATION_1MO_SECS: u32 = 2_592_000;
pub const DURATION_6MO_SECS: u32 = 15_552_000;
pub const DURATION_1Y_SECS: u32 = 31_536_000;

pub const DEFAULT_EXPIRY_SECS: u32 = DURATION_1MO_SECS;

pub const MAX_ROLES: usize = 25;
const _: () = assert!(MAX_ROLES <= 25);
pub const MAX_NICK_BYTES: usize = 64;
/// Discord nickname limit in chars (restores past this are rejected).
pub const MAX_NICK_CHARS: usize = 32;

#[inline]
pub(crate) fn bit(idx: usize) -> u32 {
    debug_assert!(idx < 32);
    1u32 << (idx as u32)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuildConfig {
    pub roles: Vec<Id<RoleMarker>>,
    pub skipped_mask: u32,
    pub log_channel: Option<Id<ChannelMarker>>,
    pub expiry_secs: u32,
    pub dont_save_if_ban_kicked: bool,
    pub include_bots: bool,
    pub save_nicknames: bool,
}

impl Default for GuildConfig {
    fn default() -> Self {
        Self {
            roles: Vec::new(),
            skipped_mask: 0,
            log_channel: None,
            expiry_secs: DEFAULT_EXPIRY_SECS,
            dont_save_if_ban_kicked: true,
            include_bots: true,
            save_nicknames: false,
        }
    }
}

impl GuildConfig {
    pub fn index_of(&self, role_id: Id<RoleMarker>) -> Option<usize> {
        self.roles.iter().position(|&r| r == role_id)
    }

    pub fn mask_for_len(n: usize) -> u32 {
        if n == 0 {
            0
        } else if n >= 32 {
            u32::MAX
        } else {
            (1u32 << n).wrapping_sub(1)
        }
    }

    pub fn mask(&self) -> u32 {
        Self::mask_for_len(self.roles.len().min(MAX_ROLES))
    }

    pub fn active_mask(&self) -> u32 {
        self.mask() & !self.skipped_mask
    }

    pub fn is_skipped_idx(&self, idx: usize) -> bool {
        idx < 32 && (self.skipped_mask >> idx) & 1 == 1
    }

    pub fn role_ids(&self) -> Vec<Id<RoleMarker>> {
        self.roles
            .iter()
            .enumerate()
            .filter(|(i, _)| !self.is_skipped_idx(*i))
            .map(|(_, &r)| r)
            .collect()
    }

    pub fn dict_ids(&self) -> Vec<Id<RoleMarker>> {
        self.roles.clone()
    }

    pub fn decode_into(&self, bits: u32, out: &mut Vec<Id<RoleMarker>>) {
        out.clear();
        let masked = bits & self.active_mask();
        for (i, &rid) in self.roles.iter().enumerate().take(MAX_ROLES) {
            if (masked >> i) & 1 == 1 {
                out.push(rid);
            }
        }
    }

    pub fn from_ids_dedup(ids: &[Id<RoleMarker>]) -> Self {
        let mut cfg = Self::default();
        cfg.set_roles(ids);
        cfg
    }

    pub fn set_roles(&mut self, ids: &[Id<RoleMarker>]) {
        let mut roles = Vec::with_capacity(ids.len().min(MAX_ROLES));
        for &id in ids {
            if roles.contains(&id) {
                continue;
            }
            if roles.len() >= MAX_ROLES {
                break;
            }
            roles.push(id);
        }
        self.roles = roles;
        self.skipped_mask = 0;
    }

    pub fn flags(&self) -> u8 {
        let mut flags: u8 = 0;
        if self.dont_save_if_ban_kicked {
            flags |= 1 << 0;
        }
        if self.include_bots {
            flags |= 1 << 1;
        }
        if self.save_nicknames {
            flags |= 1 << 2;
        }
        flags
    }

    pub fn from_flags(flags: u8) -> (bool, bool, bool) {
        (
            flags & (1 << 0) != 0,
            flags & (1 << 1) != 0,
            flags & (1 << 2) != 0,
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StickyEntry {
    pub bits: u32,
    pub left_at: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuildStore {
    pub guild_id: Id<GuildMarker>,
    pub config: GuildConfig,
    pub map: FxHashMap<Id<UserMarker>, StickyEntry>,
    pub nicks: FxHashMap<Id<UserMarker>, Box<str>>,
}

impl GuildStore {
    pub fn new(guild_id: Id<GuildMarker>, config: GuildConfig) -> Self {
        Self {
            guild_id,
            config,
            map: FxHashMap::default(),
            nicks: FxHashMap::default(),
        }
    }

    #[inline]
    pub fn nick_for(&self, user_id: Id<UserMarker>) -> Option<&str> {
        self.nicks.get(&user_id).map(|s| &**s)
    }

    fn clean_nick(nick: Option<&str>) -> Option<Box<str>> {
        nick.filter(|s| !s.is_empty())
            .map(Self::truncate_nick)
            .filter(|s| !s.is_empty())
            .map(|s| s.into())
    }

    pub fn set_nick(&mut self, user_id: Id<UserMarker>, nick: Option<Box<str>>) {
        match Self::clean_nick(nick.as_deref()) {
            Some(n) => {
                self.nicks.insert(user_id, n);
            }
            None => {
                self.nicks.remove(&user_id);
            }
        }
    }

    pub fn remove_user(&mut self, user_id: Id<UserMarker>) -> bool {
        let removed = self.map.remove(&user_id).is_some();
        self.nicks.remove(&user_id);
        removed
    }

    pub fn is_expired(left_at: u32, now_secs: u32, expiry_secs: u32) -> bool {
        if expiry_secs == 0 || left_at == 0 {
            return false;
        }
        now_secs.saturating_sub(left_at) > expiry_secs
    }

    pub fn truncate_nick(nick: &str) -> &str {
        // Discord rejects nicknames past 32 chars; also cap bytes for the wire.
        let mut s = nick;
        if s.chars().count() > MAX_NICK_CHARS {
            let end = s
                .char_indices()
                .nth(MAX_NICK_CHARS)
                .map(|(i, _)| i)
                .unwrap_or(s.len());
            s = &s[..end];
        }
        if s.len() <= MAX_NICK_BYTES {
            return s;
        }
        let mut end = MAX_NICK_BYTES;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        &s[..end]
    }

    pub fn restore_into(
        &self,
        user_id: Id<UserMarker>,
        now_secs: u32,
        out_roles: &mut Vec<Id<RoleMarker>>,
    ) -> Option<Option<&str>> {
        let e = self.map.get(&user_id)?;
        if Self::is_expired(e.left_at, now_secs, self.config.expiry_secs) {
            return None;
        }
        self.config.decode_into(e.bits, out_roles);
        if out_roles.is_empty() {
            return None;
        }
        Some(self.nick_for(user_id))
    }

    /// Drop zero-bit rows, keeping nick-only ones while `save_nicknames` is on
    /// (they still restore the nick). Returns rows removed.
    pub fn purge_zeroed(&mut self) -> usize {
        let keep_nicks = self.config.save_nicknames;
        let mut gone: Vec<Id<UserMarker>> = Vec::new();
        for (uid, e) in self.map.iter() {
            if e.bits == 0 && !(keep_nicks && self.nicks.contains_key(uid)) {
                gone.push(*uid);
            }
        }
        let removed = gone.len();
        for uid in gone {
            self.remove_user(uid);
        }
        removed
    }

    pub fn expire_sweep(&mut self, now_secs: u32) -> usize {
        let expiry = self.config.expiry_secs;
        if expiry == 0 {
            return 0;
        }
        let before = self.map.len();
        self.map
            .retain(|_, e| !Self::is_expired(e.left_at, now_secs, expiry));
        let removed = before - self.map.len();
        if removed > 0 {
            self.nicks.retain(|uid, _| self.map.contains_key(uid));
        }
        removed
    }

    pub fn remove_role(&mut self, role_id: Id<RoleMarker>) -> usize {
        let Some(idx) = self.config.index_of(role_id) else {
            return 0;
        };
        if self.config.is_skipped_idx(idx) {
            return 0;
        }
        let mask = bit(idx);
        let held = self.map.values().filter(|e| e.bits & mask != 0).count();
        self.config.skipped_mask |= mask;
        held
    }

    pub fn holder_counts_for(
        &self,
        roles: &[Id<RoleMarker>],
        now_secs: u32,
    ) -> Vec<(Id<RoleMarker>, usize)> {
        let expiry = self.config.expiry_secs;
        roles
            .iter()
            .map(|&rid| {
                let n = match self.config.index_of(rid) {
                    Some(idx) if !self.config.is_skipped_idx(idx) => {
                        let mask = bit(idx);
                        self.map
                            .values()
                            .filter(|e| {
                                e.bits & mask != 0 && !Self::is_expired(e.left_at, now_secs, expiry)
                            })
                            .count()
                    }
                    _ => 0,
                };
                (rid, n)
            })
            .collect()
    }
}

fn oldest_first(map: &FxHashMap<Id<UserMarker>, StickyEntry>) -> Vec<(u32, Id<UserMarker>)> {
    let mut order: Vec<(u32, Id<UserMarker>)> =
        map.iter().map(|(uid, e)| (e.left_at, *uid)).collect();
    order.sort_by_key(|(left, uid)| (*left, uid.get()));
    order
}

pub fn export_guild(store: &GuildStore) -> String {
    let mut rows: Vec<csv::StickyRow> = Vec::with_capacity(store.map.len());
    let mut roles: Vec<Id<RoleMarker>> = Vec::new();
    for (_, uid) in oldest_first(&store.map) {
        let Some(e) = store.map.get(&uid) else {
            continue;
        };
        store.config.decode_into(e.bits, &mut roles);
        let nickname = if store.config.save_nicknames {
            store.nick_for(uid).map(Box::from)
        } else {
            None
        };
        rows.push(csv::StickyRow {
            guild_id: store.guild_id,
            user_id: uid,
            roles: std::mem::take(&mut roles),
            nickname,
            left_at: e.left_at,
        });
    }
    csv::export_csv(&rows)
}

pub fn import_replace(
    store: &mut GuildStore,
    csv_text: &str,
    expected_guild: Id<GuildMarker>,
    now_secs: u32,
    expiry_secs: u32,
    is_member: impl Fn(Id<UserMarker>) -> bool,
) -> Result<csv::ImportReport, csv::ImportReject> {
    if expected_guild != store.guild_id {
        return Err(csv::ImportReject::new(
            csv::ImportRejectKind::StoreMismatch,
            0,
        ));
    }
    let mut report = csv::parse_import(csv_text, expected_guild, now_secs)?;

    let mut kept = Vec::with_capacity(report.rows.len());
    for row in report.rows.drain(..) {
        if row.left_at == 0 || GuildStore::is_expired(row.left_at, now_secs, expiry_secs) {
            report.skipped_expired += 1;
            continue;
        }
        if is_member(row.user_id) {
            report.skipped_current_members += 1;
            continue;
        }
        kept.push(row);
    }
    report.rows = kept;

    // Never expand the dict: resolve only against the current active roles.
    // Zero-bit rows survive as nick-only rows (flag on + nick present);
    // newest zero-bit without a nick still clears an older entry.
    let active = store.config.role_ids();
    let save_nicks = store.config.save_nicknames;
    let mut next_map: FxHashMap<Id<UserMarker>, StickyEntry> = FxHashMap::default();
    let mut next_nicks: FxHashMap<Id<UserMarker>, Box<str>> = FxHashMap::default();
    for row in &report.rows {
        let mut bits: u32 = 0;
        let mut untracked = false;
        for r in &row.roles {
            match active.iter().position(|&a| a == *r) {
                Some(j) => bits |= bit(j),
                None => untracked = true,
            }
        }
        // Count once per row: the UI note is generic, and per-role counts
        // mislead (one row with 3 untracked roles read as "3").
        if untracked {
            report.skipped_untracked_roles += 1;
        }
        if bits == 0 {
            match GuildStore::clean_nick(row.nickname.as_deref()) {
                Some(n) if save_nicks => {
                    next_map.insert(
                        row.user_id,
                        StickyEntry {
                            bits: 0,
                            left_at: row.left_at,
                        },
                    );
                    next_nicks.insert(row.user_id, n);
                }
                Some(_) => {
                    report.skipped_nicknames += 1;
                    next_map.remove(&row.user_id);
                    next_nicks.remove(&row.user_id);
                }
                None => {
                    next_map.remove(&row.user_id);
                    next_nicks.remove(&row.user_id);
                }
            }
            continue;
        }
        next_map.insert(
            row.user_id,
            StickyEntry {
                bits,
                left_at: row.left_at,
            },
        );
        match GuildStore::clean_nick(row.nickname.as_deref()) {
            Some(n) if save_nicks => {
                next_nicks.insert(row.user_id, n);
            }
            Some(_) => {
                report.skipped_nicknames += 1;
            }
            None => {}
        }
    }

    if next_map.len() > csv::MAX_RESTORE_ROWS {
        let mut order: Vec<(u32, u64, Id<UserMarker>)> = next_map
            .iter()
            .map(|(uid, e)| (e.left_at, uid.get(), *uid))
            .collect();
        order.sort_unstable();
        let drop_n = order.len() - csv::MAX_RESTORE_ROWS;
        let mut dropped: Vec<Id<UserMarker>> =
            order.drain(..drop_n).map(|(_, _, uid)| uid).collect();
        dropped.sort_unstable();
        for uid in &dropped {
            next_map.remove(uid);
            next_nicks.remove(uid);
        }
        report.skipped_truncated += drop_n;
        report
            .rows
            .retain(|r| dropped.binary_search(&r.user_id).is_err());
    }

    store.config.set_roles(&active);
    store.map = next_map;
    store.nicks = next_nicks;
    Ok(report)
}
