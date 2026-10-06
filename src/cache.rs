use rustc_hash::FxHashMap;
use twilight_model::gateway::payload::incoming::{
    MemberAdd, MemberChunk, MemberRemove, MemberUpdate, RoleCreate, RoleDelete, RoleUpdate,
};
use twilight_model::guild::{Guild, Member, Permissions, Role};
use twilight_model::id::{
    Id,
    marker::{GuildMarker, RoleMarker, UserMarker},
};

#[derive(Debug, Clone, Copy)]
pub struct RoleInfo {
    pub position: i16,
    pub managed: bool,
    pub permissions: Permissions,
}

#[derive(Debug)]
pub struct GuildCache {
    pub roles: FxHashMap<Id<RoleMarker>, RoleInfo>,
    pub members: FxHashMap<Id<UserMarker>, Box<[Id<RoleMarker>]>>,
    pub nicks: Option<Box<FxHashMap<Id<UserMarker>, Box<str>>>>,
    pub owner_id: Id<UserMarker>,
    pub bot_top: i16,
    pub bot_perms: Permissions,
}

impl Default for GuildCache {
    fn default() -> Self {
        Self {
            roles: FxHashMap::default(),
            members: FxHashMap::default(),
            nicks: None,
            owner_id: Id::new(1),
            bot_top: 0,
            bot_perms: Permissions::empty(),
        }
    }
}

#[derive(Debug, Default)]
pub struct Cache {
    pub guilds: FxHashMap<Id<GuildMarker>, GuildCache>,
}

#[inline]
pub fn role_unassignable(role: &Role) -> bool {
    role.managed
        || role.tags.as_ref().is_some_and(|t| {
            t.available_for_purchase
                || t.guild_connections
                || t.premium_subscriber
                || t.bot_id.is_some()
                || t.integration_id.is_some()
                || t.subscription_listing_id.is_some()
        })
}

#[inline]
fn role_info(role: &Role) -> RoleInfo {
    RoleInfo {
        position: role.position.clamp(i16::MIN as i64, i16::MAX as i64) as i16,
        managed: role_unassignable(role),
        permissions: role.permissions,
    }
}

impl GuildCache {
    pub fn refresh_bot(&mut self, guild_id: Id<GuildMarker>, bot_id: Option<Id<UserMarker>>) {
        let mut top: i16 = 0;
        let mut perms = Permissions::empty();
        if let Some(everyone) = self.roles.get(&guild_id.cast()) {
            perms |= everyone.permissions;
        }
        if let Some(bot) = bot_id
            && let Some(roles) = self.members.get(&bot)
        {
            for rid in roles {
                if let Some(r) = self.roles.get(rid) {
                    top = top.max(r.position);
                    perms |= r.permissions;
                }
            }
        }
        self.bot_top = top;
        self.bot_perms = perms;
    }

    pub fn nick_for(&self, user_id: Id<UserMarker>) -> Option<&str> {
        self.nicks
            .as_deref()
            .and_then(|m| m.get(&user_id))
            .map(|s| &**s)
    }

    fn set_nick(&mut self, user_id: Id<UserMarker>, nick: Option<&str>, store_nick: bool) {
        let want = if store_nick {
            nick.filter(|n| !n.is_empty())
        } else {
            None
        };
        match want {
            Some(n) => {
                self.nicks
                    .get_or_insert_with(Default::default)
                    .insert(user_id, n.into());
            }
            None => {
                if let Some(m) = self.nicks.as_mut() {
                    m.remove(&user_id);
                    if m.is_empty() {
                        self.nicks = None;
                    }
                }
            }
        }
    }

    pub fn member_top(&self, user_id: Id<UserMarker>) -> i16 {
        self.members
            .get(&user_id)
            .map(|roles| {
                roles
                    .iter()
                    .filter_map(|rid| self.roles.get(rid))
                    .map(|r| r.position)
                    .max()
                    .unwrap_or(0)
            })
            .unwrap_or(0)
    }

    fn store_row(
        &mut self,
        user_id: Id<UserMarker>,
        roles: &[Id<RoleMarker>],
        nick: Option<&str>,
        store_nick: bool,
        bot_id: Option<Id<UserMarker>>,
    ) -> bool {
        if Some(user_id) != bot_id
            && roles.is_empty()
            && !(store_nick && nick.is_some_and(|n| !n.is_empty()))
        {
            self.members.remove(&user_id);
            self.set_nick(user_id, None, true);
            return false;
        }
        self.members
            .insert(user_id, roles.to_vec().into_boxed_slice());
        self.set_nick(user_id, nick, store_nick);
        true
    }
}

impl Cache {
    fn guild_mut(&mut self, guild_id: Id<GuildMarker>) -> &mut GuildCache {
        self.guilds.entry(guild_id).or_default()
    }

    #[allow(clippy::too_many_arguments)]
    pub fn upsert_guild_create(
        &mut self,
        guild_id: Id<GuildMarker>,
        roles: &[Role],
        members: &[Member],
        owner_id: Id<UserMarker>,
        bot_id: Option<Id<UserMarker>>,
        total_hint: u32,
        store_nick: bool,
    ) {
        let g = self.guild_mut(guild_id);
        g.roles.clear();
        g.roles.reserve(roles.len());
        for role in roles {
            g.roles.insert(role.id, role_info(role));
        }
        g.owner_id = owner_id;
        if Self::needs_chunk(total_hint, members.len()) {
            // Partial GUILD_CREATE: drop stale members from any previous
            // connection, otherwise departed users linger and later leave
            // events save the wrong roles/nicks.
            g.members.clear();
            if !store_nick {
                g.nicks = None;
            }
            g.members.reserve(members.len());
        } else {
            g.members.clear();
            g.nicks = None;
        }
        for m in members {
            g.store_row(m.user.id, &m.roles, m.nick.as_deref(), store_nick, bot_id);
        }
        g.refresh_bot(guild_id, bot_id);
    }

    pub fn upsert_from_guild(
        &mut self,
        guild: &Guild,
        bot_id: Option<Id<UserMarker>>,
        store_nick: bool,
    ) {
        self.upsert_guild_create(
            guild.id,
            &guild.roles,
            &guild.members,
            guild.owner_id,
            bot_id,
            guild.member_count.unwrap_or(0).min(u32::MAX as u64) as u32,
            store_nick,
        );
    }

    pub fn handle_chunk(
        &mut self,
        chunk: &MemberChunk,
        store_nick: bool,
        bot_id: Option<Id<UserMarker>>,
    ) -> bool {
        let gid = chunk.guild_id;
        let Some(g) = self.guilds.get_mut(&gid) else {
            return false;
        };
        g.members.reserve(chunk.members.len());
        for m in &chunk.members {
            g.store_row(m.user.id, &m.roles, m.nick.as_deref(), store_nick, bot_id);
        }
        g.refresh_bot(gid, bot_id);
        true
    }

    pub fn handle_add(
        &mut self,
        payload: &MemberAdd,
        store_nick: bool,
        bot_id: Option<Id<UserMarker>>,
    ) -> bool {
        let gid = payload.guild_id;
        let Some(g) = self.guilds.get_mut(&gid) else {
            return false;
        };
        let m = &payload.member;
        let kept = g.store_row(m.user.id, &m.roles, m.nick.as_deref(), store_nick, bot_id);
        if kept && Some(m.user.id) == bot_id {
            g.refresh_bot(gid, bot_id);
        }
        kept
    }

    pub fn handle_update(
        &mut self,
        payload: &MemberUpdate,
        store_nick: bool,
        bot_id: Option<Id<UserMarker>>,
    ) -> bool {
        let gid = payload.guild_id;
        // Never fabricate a guild on member updates: an unknown guild means
        // GUILD_CREATE hasn't populated the cache, and a default entry would
        // poison hierarchy gates (owner_id/bot_top).
        let Some(g) = self.guilds.get_mut(&gid) else {
            return false;
        };
        let kept = g.store_row(
            payload.user.id,
            &payload.roles,
            payload.nick.as_deref(),
            store_nick,
            bot_id,
        );
        if kept && Some(payload.user.id) == bot_id {
            g.refresh_bot(gid, bot_id);
        }
        kept
    }

    pub fn handle_remove(
        &mut self,
        payload: &MemberRemove,
        bot_id: Option<Id<UserMarker>>,
    ) -> bool {
        match self.guilds.get_mut(&payload.guild_id) {
            Some(g) => {
                let removed = g.members.remove(&payload.user.id).is_some();
                if let Some(m) = g.nicks.as_mut() {
                    m.remove(&payload.user.id);
                    if m.is_empty() {
                        g.nicks = None;
                    }
                }
                if removed && Some(payload.user.id) == bot_id {
                    g.refresh_bot(payload.guild_id, bot_id);
                }
                removed
            }
            None => false,
        }
    }

    pub fn handle_role_create(&mut self, payload: &RoleCreate, bot_id: Option<Id<UserMarker>>) {
        // Drop events for unknown guilds instead of fabricating a default
        // entry (see handle_update).
        let Some(g) = self.guilds.get_mut(&payload.guild_id) else {
            return;
        };
        g.roles.insert(payload.role.id, role_info(&payload.role));
        g.refresh_bot(payload.guild_id, bot_id);
    }

    pub fn handle_role_update(&mut self, payload: &RoleUpdate, bot_id: Option<Id<UserMarker>>) {
        let Some(g) = self.guilds.get_mut(&payload.guild_id) else {
            return;
        };
        g.roles.insert(payload.role.id, role_info(&payload.role));
        g.refresh_bot(payload.guild_id, bot_id);
    }

    pub fn handle_role_delete(
        &mut self,
        payload: &RoleDelete,
        bot_id: Option<Id<UserMarker>>,
    ) -> bool {
        match self.guilds.get_mut(&payload.guild_id) {
            Some(g) => {
                let existed = g.roles.remove(&payload.role_id).is_some();
                g.refresh_bot(payload.guild_id, bot_id);
                existed
            }
            None => false,
        }
    }

    pub fn needs_chunk(total_hint: u32, members_len: usize) -> bool {
        if total_hint == 0 {
            return members_len > 0;
        }
        (members_len as u64) < u64::from(total_hint)
    }

    pub fn remove_guild(&mut self, guild_id: Id<GuildMarker>) -> bool {
        self.guilds.remove(&guild_id).is_some()
    }

    pub fn clear_nicks(&mut self, guild_id: Id<GuildMarker>) {
        if let Some(g) = self.guilds.get_mut(&guild_id) {
            g.nicks = None;
        }
    }
}
