use twilight_model::gateway::payload::outgoing::RequestGuildMembers;
use twilight_model::guild::Permissions;
use twilight_model::id::{
    Id,
    marker::{GuildMarker, RoleMarker},
};

#[derive(Debug, Clone, Copy)]
pub(crate) struct Actor {
    pub(crate) top: i16,
    pub(crate) perms: Permissions,
    pub(crate) is_owner: bool,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Target {
    pub(crate) is_everyone: bool,
    pub(crate) pos: i16,
    pub(crate) managed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Deny {
    NoGrant,
    Managed,
    Everyone,
    Hierarchy,
}

pub(crate) fn has_manage_grant(actor: &Actor) -> bool {
    actor.is_owner
        || actor.perms.contains(Permissions::ADMINISTRATOR)
        || actor.perms.contains(Permissions::MANAGE_ROLES)
}

#[inline]
pub(crate) fn is_everyone(role_id: Id<RoleMarker>, guild_id: Id<GuildMarker>) -> bool {
    role_id.get() == guild_id.get()
}

pub(crate) fn can_assign(actor: &Actor, target: &Target) -> Result<(), Deny> {
    if !has_manage_grant(actor) {
        return Err(Deny::NoGrant);
    }
    if target.is_everyone {
        return Err(Deny::Everyone);
    }
    if target.managed {
        return Err(Deny::Managed);
    }
    if actor.is_owner {
        return Ok(());
    }
    if actor.top > target.pos {
        Ok(())
    } else {
        Err(Deny::Hierarchy)
    }
}

pub(crate) const CHUNK_NONCE_PREFIX: &str = "sticky-roles:chunk:";

pub(crate) fn build_chunk_request(guild_id: Id<GuildMarker>) -> RequestGuildMembers {
    RequestGuildMembers::builder(guild_id)
        .nonce(format!("{CHUNK_NONCE_PREFIX}{}", guild_id.get()))
        .presences(false)
        .query("", None)
}
