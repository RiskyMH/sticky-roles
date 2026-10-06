use twilight_model::gateway::payload::incoming::{RoleDelete, RoleUpdate};

use crate::store::{persist, registry};

use super::cache;

pub(crate) async fn on_role_delete(payload: &RoleDelete) {
    cache()
        .write()
        .await
        .handle_role_delete(payload, super::bot_id());
    if let Err(e) = persist::append_remap(payload.guild_id, payload.role_id).await {
        tracing::warn!(error = ?e, "wal remap failed on role delete; old bits kept");
    }
}

pub(crate) async fn on_role_update(payload: &RoleUpdate) {
    let guild_id = payload.guild_id;
    let role_id = payload.role.id;
    cache()
        .write()
        .await
        .handle_role_update(payload, super::bot_id());
    let tracked = registry::with_guild(guild_id, |s| {
        s.config
            .index_of(role_id)
            .map(|i| s.config.is_skipped_idx(i))
    })
    .await
    .flatten();
    if tracked != Some(false) {
        return;
    }
    let unassignable = cache()
        .read()
        .await
        .guilds
        .get(&guild_id)
        .and_then(|g| g.roles.get(&role_id))
        .is_some_and(|r| r.managed);
    if !unassignable {
        return;
    }
    if let Err(e) = persist::append_remap(guild_id, role_id).await {
        tracing::warn!(error = ?e, "wal remap failed on role update; old bits kept");
    }
}
