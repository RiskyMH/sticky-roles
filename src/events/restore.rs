use twilight_model::guild::Permissions;
use twilight_model::id::{
    Id,
    marker::{GuildMarker, RoleMarker, UserMarker},
};

use crate::CTX;
use crate::commands::messages as log;
use crate::hierarchy::{Actor, Target, can_assign, has_manage_grant, is_everyone};
use crate::store::{GuildConfig, registry};

use super::cache;

const CONCURRENCY: usize = 3;

#[derive(Debug, Default)]
pub(crate) struct RestoreOutcome {
    pub(crate) failed_roles: Vec<Id<RoleMarker>>,
    pub(crate) failed_nick: Option<String>,
}

struct Snapshot {
    bot_top: i16,
    bot_perms: Permissions,
    owner_id: Id<UserMarker>,
    member_top: i16,
    wanted: Vec<(Id<RoleMarker>, i16, bool)>,
}

struct NickDone {
    restored: Option<String>,
    skipped: Option<&'static str>,
    failed: Option<String>,
}

fn is_transient(status: Option<u16>) -> bool {
    status.is_none() || status == Some(429) || status.is_some_and(|s| s >= 500)
}

pub(crate) async fn restore_roles(
    guild_id: Id<GuildMarker>,
    user_id: Id<UserMarker>,
    role_ids: &[Id<RoleMarker>],
    nick: Option<String>,
    left_at: u32,
    cfg: &GuildConfig,
) -> Option<RestoreOutcome> {
    let snap = snapshot(guild_id, user_id, role_ids).await?;
    let nick_done = restore_nick(guild_id, user_id, nick, &snap, cfg.save_nicknames).await;
    if !has_manage_grant(&Actor {
        top: snap.bot_top,
        perms: snap.bot_perms,
        is_owner: false,
    }) {
        tracing::warn!("bot lacks Manage Roles; skipping restore");
        let skipped: Vec<(Id<RoleMarker>, &'static str)> = role_ids
            .iter()
            .map(|r| (*r, log::REASON_NO_GRANT))
            .collect();
        let failed_roles = role_ids.to_vec();
        tokio::spawn(async move {
            log::log_restore(
                guild_id,
                user_id,
                &[],
                &skipped,
                nick_done.restored.as_deref(),
                nick_done.skipped,
                left_at,
            )
            .await;
        });
        return Some(RestoreOutcome {
            failed_roles,
            failed_nick: nick_done.failed,
        });
    }
    let (to_restore, mut skipped) = plan(guild_id, role_ids, cfg, &snap);
    let (mut restored, failed_roles) = drive(guild_id, user_id, to_restore, &mut skipped).await;
    restored.sort_by_key(|r| r.get());
    if !failed_roles.is_empty() {
        tracing::warn!(
            failed = failed_roles.len(),
            restored = restored.len(),
            "restore finished with transient failures; survivors resaved"
        );
    }
    tokio::spawn(async move {
        log::log_restore(
            guild_id,
            user_id,
            &restored,
            &skipped,
            nick_done.restored.as_deref(),
            nick_done.skipped,
            left_at,
        )
        .await;
    });
    Some(RestoreOutcome {
        failed_roles,
        failed_nick: nick_done.failed,
    })
}

async fn snapshot(
    guild_id: Id<GuildMarker>,
    user_id: Id<UserMarker>,
    role_ids: &[Id<RoleMarker>],
) -> Option<Snapshot> {
    registry::with_guild(guild_id, |_| ()).await?;
    let guard = cache().read().await;
    let g = guard.guilds.get(&guild_id)?;
    let mut wanted = Vec::with_capacity(role_ids.len());
    for rid in role_ids {
        if let Some(r) = g.roles.get(rid) {
            wanted.push((*rid, r.position, r.managed));
        }
    }
    Some(Snapshot {
        bot_top: g.bot_top,
        bot_perms: g.bot_perms,
        owner_id: g.owner_id,
        member_top: g.member_top(user_id),
        wanted,
    })
}

async fn restore_nick(
    guild_id: Id<GuildMarker>,
    user_id: Id<UserMarker>,
    nick: Option<String>,
    snap: &Snapshot,
    save_nicks: bool,
) -> NickDone {
    let mut done = NickDone {
        restored: None,
        skipped: None,
        failed: None,
    };
    let Some(n) = nick.filter(|n| !n.is_empty()) else {
        return done;
    };
    if !save_nicks {
        return done;
    }
    if user_id == snap.owner_id {
        done.skipped = Some(log::REASON_NICK_OWNER);
        return done;
    }
    if !(snap.bot_perms.contains(Permissions::MANAGE_NICKNAMES)
        || snap.bot_perms.contains(Permissions::ADMINISTRATOR))
    {
        done.skipped = Some(log::REASON_NICK_NO_GRANT);
        return done;
    }
    if snap.bot_top <= snap.member_top {
        done.skipped = Some(log::hierarchy_reason(snap.bot_top, snap.member_top).as_str());
        return done;
    }
    match CTX
        .http
        .update_guild_member(guild_id, user_id)
        .nick(Some(&n))
        .await
    {
        Ok(_) => done.restored = Some(n),
        Err(e) => {
            let (status, code) = log::http_codes(&e);
            tracing::warn!(status, code, "failed to restore nickname; skipped");
            done.skipped = Some(log::REASON_RESTORE_FAILED);
            if is_transient(status) {
                done.failed = Some(n);
            }
        }
    }
    done
}

type RestorePlan = (Vec<Id<RoleMarker>>, Vec<(Id<RoleMarker>, &'static str)>);

fn plan(
    guild_id: Id<GuildMarker>,
    role_ids: &[Id<RoleMarker>],
    cfg: &GuildConfig,
    snap: &Snapshot,
) -> RestorePlan {
    let actor = Actor {
        top: snap.bot_top,
        perms: snap.bot_perms,
        is_owner: false,
    };
    let mut to_restore = Vec::with_capacity(role_ids.len());
    let mut skipped = Vec::new();
    for rid in role_ids {
        if is_everyone(*rid, guild_id) {
            skipped.push((*rid, log::REASON_EVERYONE));
            continue;
        }
        if cfg.index_of(*rid).is_none() {
            skipped.push((*rid, log::REASON_UNTRACKED_REMOVED));
            continue;
        }
        let Some((_, pos, managed)) = snap.wanted.iter().find(|(id, _, _)| id == rid) else {
            skipped.push((*rid, log::REASON_UNKNOWN_DELETED));
            continue;
        };
        let target = Target {
            is_everyone: false,
            pos: *pos,
            managed: *managed,
        };
        if let Err(deny) = can_assign(&actor, &target) {
            skipped.push((*rid, log::deny_reason(deny, snap.bot_top, *pos).as_str()));
            continue;
        }
        to_restore.push(*rid);
    }
    (to_restore, skipped)
}

async fn drive(
    guild_id: Id<GuildMarker>,
    user_id: Id<UserMarker>,
    to_restore: Vec<Id<RoleMarker>>,
    skipped: &mut Vec<(Id<RoleMarker>, &'static str)>,
) -> (Vec<Id<RoleMarker>>, Vec<Id<RoleMarker>>) {
    let mut restored = Vec::with_capacity(to_restore.len());
    let mut failed = Vec::new();
    let mut set = tokio::task::JoinSet::new();
    let mut iter = to_restore.into_iter();
    for _ in 0..CONCURRENCY {
        if let Some(rid) = iter.next() {
            set.spawn(async move {
                (
                    rid,
                    CTX.http.add_guild_member_role(guild_id, user_id, rid).await,
                )
            });
        }
    }
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok((rid, Ok(_))) => restored.push(rid),
            Ok((rid, Err(e))) => {
                let (status, code) = log::http_codes(&e);
                tracing::debug!(status, code, "failed to restore role; skipped");
                skipped.push((rid, log::REASON_RESTORE_FAILED));
                if is_transient(status) {
                    failed.push(rid);
                }
            }
            Err(_) => {
                tracing::debug!("restore task panicked; old data kept");
            }
        }
        if let Some(rid) = iter.next() {
            set.spawn(async move {
                (
                    rid,
                    CTX.http.add_guild_member_role(guild_id, user_id, rid).await,
                )
            });
        }
    }
    (restored, failed)
}
