use twilight_model::gateway::payload::incoming::{MemberAdd, MemberRemove};
use twilight_model::guild::Permissions;
use twilight_model::id::{
    Id,
    marker::{GuildMarker, RoleMarker, UserMarker},
};

use crate::store::{GuildConfig, GuildStore, MAX_ROLES, StickyEntry, persist, registry};

use super::{audit, cache, restore};

fn encode_bits(roles: &[Id<RoleMarker>], dict: &[Id<RoleMarker>]) -> u32 {
    let mut bits: u32 = 0;
    for r in roles {
        if let Some(i) = dict.iter().position(|id| id == r)
            && i < 32
        {
            bits |= 1u32 << (i as u32);
        }
    }
    bits
}

type SavedRow = (Vec<Id<RoleMarker>>, Option<Box<str>>);

/// True when the stored row can never restore roles: its bits decode to
/// nothing (zero bits or only untracked/skipped dict entries).
async fn decodes_to_no_roles(guild_id: Id<GuildMarker>, user_id: Id<UserMarker>) -> bool {
    registry::with_guild(guild_id, |s| {
        s.map.get(&user_id).is_some_and(|e| {
            let mut out = Vec::new();
            s.config.decode_into(e.bits, &mut out);
            out.is_empty()
        })
    })
    .await
    .unwrap_or(false)
}

pub(crate) async fn on_member_add(payload: &MemberAdd) {
    let guild_id = payload.guild_id;
    let user_id = payload.member.user.id;
    let cfg: GuildConfig = registry::with_guild(guild_id, |s| s.config.clone())
        .await
        .unwrap_or_default();

    if payload.member.user.bot && !cfg.include_bots {
        cache().write().await.handle_remove(
            &MemberRemove {
                guild_id,
                user: payload.member.user.clone(),
            },
            super::bot_id(),
        );
        if registry::has_entry(guild_id, user_id).await
            && let Err(e) = persist::append_delete(guild_id, user_id).await
        {
            tracing::warn!(error = ?e, "wal delete failed; stale excluded-bot row kept");
        }
        return;
    }
    cache()
        .write()
        .await
        .handle_add(payload, cfg.save_nicknames, super::bot_id());

    let now = registry::now_secs();
    let stored: Option<(Vec<Id<RoleMarker>>, Option<String>)> =
        registry::with_guild(guild_id, |s| {
            let mut out = Vec::new();
            let nick = s.restore_into(user_id, now, &mut out)?;
            let mut roles = std::mem::take(&mut out);
            roles.truncate(MAX_ROLES);
            Some((roles, nick.filter(|n| !n.is_empty()).map(str::to_owned)))
        })
        .await
        .flatten();
    let (roles, nick) = match stored {
        Some(v) => v,
        None if cfg.save_nicknames => {
            let fallback: Option<(Option<String>, bool)> = registry::with_guild(guild_id, |s| {
                let e = s.map.get(&user_id)?;
                if GuildStore::is_expired(e.left_at, now, cfg.expiry_secs) {
                    return Some((None, true));
                }
                Some((
                    s.nick_for(user_id)
                        .filter(|n| !n.is_empty())
                        .map(str::to_owned),
                    false,
                ))
            })
            .await
            .flatten();
            match fallback {
                Some((Some(n), _)) => (Vec::new(), Some(n)),
                _ => {
                    // No restorable nick either: drop the row when it's
                    // expired or decodes to nothing (zero-bit nick-less
                    // leftover), so dead rows can't linger until expiry.
                    let dead = decodes_to_no_roles(guild_id, user_id).await;
                    if (dead
                        || registry::remove_expired_entry_on_join(guild_id, user_id, now).await)
                        && let Err(e) = persist::append_delete(guild_id, user_id).await
                    {
                        tracing::warn!(error = ?e, "wal delete-after-expiry failed");
                    }
                    return;
                }
            }
        }
        None => {
            // Nick saving is off, so nicks never restore: a row decoding
            // to no roles is dead (e.g. nick-only leftover from when the
            // flag was on). Clean it alongside expired rows.
            let dead = decodes_to_no_roles(guild_id, user_id).await;
            if (dead || registry::remove_expired_entry_on_join(guild_id, user_id, now).await)
                && let Err(e) = persist::append_delete(guild_id, user_id).await
            {
                tracing::warn!(error = ?e, "wal delete-after-expiry failed");
            }
            return;
        }
    };
    if roles.is_empty() && nick.is_none() {
        return;
    }
    let left_at = registry::get_left_at(guild_id, user_id)
        .await
        .unwrap_or(now);
    let Some(outcome) =
        restore::restore_roles(guild_id, user_id, &roles, nick, left_at, &cfg).await
    else {
        return;
    };
    if outcome.failed_roles.is_empty() {
        if outcome.failed_nick.is_some() {
            return;
        }
        if let Err(e) = persist::append_delete(guild_id, user_id).await {
            tracing::warn!(error = ?e, "wal delete-after-restore failed");
        }
        return;
    }
    let dict = cfg.dict_ids();
    let bits = encode_bits(&outcome.failed_roles, &dict) & cfg.active_mask();
    if bits == 0 {
        if outcome.failed_nick.is_some() {
            return;
        }
        if let Err(e) = persist::append_delete(guild_id, user_id).await {
            tracing::warn!(error = ?e, "wal delete-after-restore failed");
        }
        return;
    }
    let entry = StickyEntry { bits, left_at };
    let failed_nick: Option<Box<str>> = outcome.failed_nick.as_deref().map(Box::from);
    if let Err(e) = persist::append_upsert(guild_id, user_id, &entry, failed_nick.as_deref()).await
    {
        tracing::warn!(error = ?e, "wal resave-after-partial-restore failed");
    }
}

pub(crate) async fn on_member_remove(payload: &MemberRemove) {
    let guild_id = payload.guild_id;
    let user_id = payload.user.id;
    registry::ensure_guild(guild_id, GuildConfig::default()).await;
    let cfg: GuildConfig = registry::with_guild(guild_id, |s| s.config.clone())
        .await
        .unwrap_or_default();

    let (saved, lacks_audit): (Option<SavedRow>, bool) = {
        let guard = cache().read().await;
        let g = guard.guilds.get(&guild_id);
        let lacks_audit = g
            .map(|g| {
                !g.bot_perms.contains(Permissions::VIEW_AUDIT_LOG)
                    && !g.bot_perms.contains(Permissions::ADMINISTRATOR)
            })
            .unwrap_or(true);
        let saved = g.and_then(|g| {
            let roles = g.members.get(&user_id)?.to_vec();
            let nick = if cfg.save_nicknames {
                g.nick_for(user_id)
                    .filter(|n| !n.is_empty())
                    .map(GuildStore::truncate_nick)
                    .filter(|n| !n.is_empty())
                    .map(Box::from)
            } else {
                None
            };
            Some((roles, nick))
        });
        (saved, lacks_audit)
    };

    let Some((roles, nick)) = saved else {
        cache()
            .write()
            .await
            .handle_remove(payload, super::bot_id());
        return;
    };
    if payload.user.bot && !cfg.include_bots {
        if registry::has_entry(guild_id, user_id).await
            && let Err(e) = persist::append_delete(guild_id, user_id).await
        {
            tracing::warn!(error = ?e, "wal delete failed; stale excluded-bot row kept");
        }
    } else {
        // Bots save even when kicked: the kick check never applies to them.
        let banned_or_kicked = cfg.dont_save_if_ban_kicked
            && !payload.user.bot
            && audit::was_banned_or_kicked(guild_id, user_id, lacks_audit);
        if !banned_or_kicked {
            let bits = encode_bits(&roles, &cfg.dict_ids()) & cfg.mask();
            let nick_ref: Option<&str> = if cfg.save_nicknames {
                nick.as_deref()
            } else {
                None
            };
            // Role-less leavers with no nick leave no row; nick-only leavers
            // keep a zero-bit row so the nick restores on rejoin.
            if bits == 0 && nick_ref.is_none() {
                if registry::has_entry(guild_id, user_id).await
                    && let Err(e) = persist::append_delete(guild_id, user_id).await
                {
                    tracing::warn!(error = ?e, "wal delete failed; stale empty-bits row kept");
                }
            } else {
                let entry = StickyEntry {
                    bits,
                    left_at: registry::now_secs(),
                };
                let saved = persist::append_upsert(guild_id, user_id, &entry, nick_ref).await;
                if let Err(e) = &saved {
                    tracing::warn!(error = ?e, "wal upsert failed; leave not saved");
                }
                // Shield bot saves from a late-arriving ban/kick purge.
                if saved.is_ok() && payload.user.bot {
                    audit::note_bot_save(guild_id, user_id);
                }
            }
        }
    }
    cache()
        .write()
        .await
        .handle_remove(payload, super::bot_id());
}
