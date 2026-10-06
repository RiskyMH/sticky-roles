//! Log messages for role/nickname restores.

use twilight_http::error::Error;
use twilight_model::{
    channel::message::AllowedMentions,
    id::{
        Id,
        marker::{ChannelMarker, GuildMarker, RoleMarker, UserMarker},
    },
};

use crate::store::registry;

pub const REASON_UNTRACKED_REMOVED: &str = "removed from tracking";
pub const REASON_UNKNOWN_DELETED: &str = "role no longer exists";
pub const REASON_MANAGED: &str = "managed by Discord";
pub const REASON_HIERARCHY_ABOVE: &str = "above my top role";
pub const REASON_NO_GRANT: &str = "I lack Manage Roles";
pub const REASON_EQUAL_POSITION: &str = "same level as my top role";
pub const REASON_EVERYONE: &str = "@everyone can't be tracked";
pub const REASON_RESTORE_FAILED: &str = "couldn't be restored";
pub const REASON_NICK_NO_GRANT: &str = "I lack Manage Nicknames";
pub const REASON_NICK_OWNER: &str = "can't change server owner";

/// Typed skip reason; `REASON_*` consts are the literal source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SkipReason {
    Managed,
    HierarchyAbove,
    NoGrant,
    EqualPosition,
    Everyone,
}

impl SkipReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Managed => REASON_MANAGED,
            Self::HierarchyAbove => REASON_HIERARCHY_ABOVE,
            Self::NoGrant => REASON_NO_GRANT,
            Self::EqualPosition => REASON_EQUAL_POSITION,
            Self::Everyone => REASON_EVERYONE,
        }
    }
}

/// Generic note for untracked skips (no IDs/pings).
/// All `REASON_UNTRACKED_REMOVED` entries fold into one sentence.
pub const UNTRACKED_NOTE: &str =
    "Some saved roles aren't tracked anymore — enable them in /sticky-roles to restore them.";

/// Equality split: equal => "same level", otherwise "above".
pub fn hierarchy_reason(bot_top: i16, target_pos: i16) -> SkipReason {
    if bot_top == target_pos {
        SkipReason::EqualPosition
    } else {
        SkipReason::HierarchyAbove
    }
}

/// Map Deny to a user-facing reason (Hierarchy splits on equality).
pub fn deny_reason(deny: crate::hierarchy::Deny, bot_top: i16, target_pos: i16) -> SkipReason {
    use crate::hierarchy::Deny;
    match deny {
        Deny::Managed => SkipReason::Managed,
        Deny::NoGrant => SkipReason::NoGrant,
        Deny::Everyone => SkipReason::Everyone,
        Deny::Hierarchy => hierarchy_reason(bot_top, target_pos),
    }
}

/// Escape a nickname for the `"..."` slot (backslash/quote/backtick; newlines flattened).
fn escape_nick(nick: &str) -> String {
    let mut out = String::with_capacity(nick.len() + 2);
    for c in nick.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '`' => out.push_str("\\`"),
            '\n' | '\r' => out.push(' '),
            _ => out.push(c),
        }
    }
    out
}

/// Append `<@&id>` mentions joined with `", "`, last gap uses `last_sep`.
fn push_role_list(msg: &mut String, roles: &[Id<RoleMarker>], last_sep: &str) {
    use std::fmt::Write as _;
    for (i, r) in roles.iter().enumerate() {
        if i > 0 {
            if i + 1 == roles.len() {
                msg.push_str(last_sep);
            } else {
                msg.push_str(", ");
            }
        }
        let _ = write!(msg, "<@&{}>", r.get());
    }
}

/// First line of the restore summary (restored roles + optional nickname).
fn push_restore_head(
    msg: &mut String,
    user_id: Id<UserMarker>,
    restored: &[Id<RoleMarker>],
    nick: Option<&str>,
    left_at: u32,
) {
    use std::fmt::Write as _;
    match (restored.is_empty(), nick) {
        (true, None) => {
            let _ = write!(
                msg,
                "Restored *no roles* to <@{}> that previously left <t:{left_at}:R>",
                user_id.get(),
            );
        }
        (true, Some(nick)) => {
            let _ = write!(
                msg,
                "Restored nickname \"{nick}\" to <@{}> that previously left <t:{left_at}:R>",
                user_id.get(),
            );
        }
        (false, None) => {
            msg.push_str("Restored ");
            push_role_list(msg, restored, " and ");
            let _ = write!(
                msg,
                " to <@{}> that previously left <t:{left_at}:R>",
                user_id.get(),
            );
        }
        (false, Some(nick)) => {
            msg.push_str("Restored ");
            push_role_list(msg, restored, ", ");
            let _ = write!(
                msg,
                " and nickname \"{nick}\" to <@{}> that previously left <t:{left_at}:R>",
                user_id.get(),
            );
        }
    }
}

/// Skips go on `-# ` subtext lines; untracked shares the line when listed skips exist.
fn push_restore_skips(
    msg: &mut String,
    skipped: &[(Id<RoleMarker>, &str)],
    nick_skipped: Option<&str>,
    untracked: usize,
) {
    use std::fmt::Write as _;
    let listed_total = skipped.len() - untracked + usize::from(nick_skipped.is_some());
    let mut other_count = 0;
    for (r, why) in skipped
        .iter()
        .filter(|(_, why)| *why != REASON_UNTRACKED_REMOVED)
    {
        if other_count == 0 {
            let _ = write!(msg, "\n-# Couldn't restore {listed_total}: ");
        } else {
            msg.push_str(", ");
        }
        let _ = write!(msg, "<@&{}> ({why})", r.get());
        other_count += 1;
    }
    if let Some(reason) = nick_skipped {
        if other_count == 0 {
            let _ = write!(msg, "\n-# Couldn't restore nickname ({reason})");
        } else {
            let _ = write!(msg, ", couldn't restore nickname ({reason})");
        }
        other_count += 1;
    }
    if untracked > 0 {
        if other_count > 0 {
            msg.push_str(". ");
            msg.push_str(UNTRACKED_NOTE);
        } else {
            msg.push_str("\n-# ");
            msg.push_str(UNTRACKED_NOTE);
        }
    }
}

/// One-line restore summary (no ping). Untracked folds into one generic sentence.
pub fn restore_message(
    user_id: Id<UserMarker>,
    restored: &[Id<RoleMarker>],
    skipped: &[(Id<RoleMarker>, &str)],
    restored_nick: Option<&str>,
    nick_skipped: Option<&str>,
    left_at: u32,
) -> String {
    let untracked = skipped
        .iter()
        .filter(|(_, why)| *why == REASON_UNTRACKED_REMOVED)
        .count();
    let mut msg = String::new();
    let nick_escaped = restored_nick.map(escape_nick);
    push_restore_head(
        &mut msg,
        user_id,
        restored,
        nick_escaped.as_deref(),
        left_at,
    );
    push_restore_skips(&mut msg, skipped, nick_skipped, untracked);
    msg
}

/// Per-guild restore-log cooldown: mass rejoins collapse to 1 message per window.
pub const LOG_COOLDOWN_SECS: u64 = 5;

static LOG_LAST: std::sync::OnceLock<super::CooldownMap> = std::sync::OnceLock::new();

pub(crate) fn log_last() -> &'static super::CooldownMap {
    LOG_LAST.get_or_init(|| super::CooldownMap::new(rustc_hash::FxHashMap::default()))
}

/// Discord HTTP status + API code from a twilight-http error (no IDs, no bodies).
pub(crate) fn http_codes(e: &Error) -> (Option<u16>, Option<u64>) {
    use twilight_http::{api_error::ApiError, error::ErrorType};
    match e.kind() {
        ErrorType::Response { status, error, .. } => {
            let api = match error {
                ApiError::General(g) => Some(g.code),
                _ => None,
            };
            (Some(status.get()), api)
        }
        _ => (None, None),
    }
}

/// Shared warn for Discord HTTP failures (status + api code, no IDs/bodies).
pub(crate) fn warn_http(e: &Error, msg: &'static str) {
    let (status, api_code) = http_codes(e);
    tracing::warn!(status, api_code, "{}", msg);
}
/// Log only when something was expected (else no message at all).
/// Nick-only restores (no roles) still log.
#[inline]
pub fn should_log(
    restored: &[Id<RoleMarker>],
    skipped: &[(Id<RoleMarker>, &str)],
    restored_nick: Option<&str>,
    nick_skipped: Option<&str>,
) -> bool {
    !restored.is_empty() || !skipped.is_empty() || restored_nick.is_some() || nick_skipped.is_some()
}

/// Pre-removal backup notice (`counts` = `(role, holders)`; empty => None).
pub fn removal_backup_message(
    counts: &[(Id<RoleMarker>, usize)],
    has_nicks: bool,
) -> Option<String> {
    use std::fmt::Write as _;
    let total = counts.iter().filter(|(_, n)| *n > 0).count();
    if total == 0 && !has_nicks {
        return None;
    }
    let mut out = String::with_capacity(256 + total * 32);
    out.push_str(
        "Heads up — some roles were removed but have saved history. A full backup is attached.\n\
         To get these back: re-enable the role(s) in `/sticky-roles` BEFORE restoring from backup.\n\
         Affected: ",
    );
    let mut first = true;
    for (r, n) in counts.iter().filter(|(_, n)| *n > 0) {
        if !first {
            out.push_str(", ");
        }
        first = false;
        let user_word = if *n == 1 { "user" } else { "users" };
        let _ = write!(out, "<@&{}> ({n} {user_word})", r.get());
    }
    out.push('.');
    if has_nicks {
        out.push_str(" It also includes saved nicknames — keep Save nicknames on to restore them.");
    }
    Some(out)
}

/// Send one restore line to the live log channel (never errors the join).
/// `left_at` renders as `<t:{left_at}:R>`.
pub async fn log_restore(
    guild_id: Id<GuildMarker>,
    user_id: Id<UserMarker>,
    restored: &[Id<RoleMarker>],
    skipped: &[(Id<RoleMarker>, &str)],
    restored_nick: Option<&str>,
    nick_skipped: Option<&str>,
    left_at: u32,
) {
    if !should_log(restored, skipped, restored_nick, nick_skipped) {
        return;
    }
    // Channel first: no-channel guilds must never burn the log budget.
    // Peek/record split (like the submit path).
    let channel = {
        let guard = registry::stores().read().await;
        guard.get(&guild_id).and_then(|s| s.config.log_channel)
    };
    let Some(channel) = channel else {
        return;
    };
    if !super::cooldown_peek_at(
        log_last(),
        guild_id.get(),
        LOG_COOLDOWN_SECS,
        std::time::Instant::now(),
    ) {
        return;
    }
    if !super::cooldown_check_at(
        log_last(),
        guild_id.get(),
        LOG_COOLDOWN_SECS,
        std::time::Instant::now(),
    ) {
        return;
    }

    let content = restore_message(
        user_id,
        restored,
        skipped,
        restored_nick,
        nick_skipped,
        left_at,
    );
    if let Err(e) = crate::CTX
        .http
        .create_message(channel)
        .content(&content)
        .allowed_mentions(Some(&AllowedMentions::default()))
        .await
    {
        let (status, api_code) = http_codes(&e);
        tracing::warn!(
            status,
            api_code,
            restored = restored.len(),
            skipped = skipped.len(),
            nick_restored = restored_nick.is_some(),
            nick_skipped = nick_skipped.is_some(),
            "failed to send restore log message; restore itself kept, log skipped"
        );
    }
}

/// `/sticky-roles` success summary (`none`/`*none*` when unset/empty).
pub(crate) fn success_message(
    roles: &[Id<RoleMarker>],
    log: Option<Id<ChannelMarker>>,
    dont_save_if_ban_kicked: bool,
    include_bots: bool,
    save_nicknames: bool,
    expiry_secs: u32,
) -> String {
    use std::fmt::Write as _;
    let mut out = String::from("Sticky roles updated\n-# - Sticky roles: ");
    if roles.is_empty() {
        out.push_str("none");
    } else {
        for (i, r) in roles.iter().enumerate() {
            if i > 0 {
                out.push(' ');
            }
            let _ = write!(out, "<@&{}>", r.get());
        }
    }
    out.push_str("\n-# - Log channel: ");
    match log {
        Some(ch) => {
            let _ = write!(out, "<#{ch}>", ch = ch.get());
        }
        None => out.push_str("none"),
    }
    out.push_str("\n-# - Options: ");
    out.push_str(&options_field(
        dont_save_if_ban_kicked,
        include_bots,
        save_nicknames,
    ));
    out.push_str("\n-# - Save data for: ");
    out.push_str(&format!("**{}**", expiry_label(expiry_secs)));
    out
}

/// Enabled options only, as `code`; `*none*` when all off.
fn options_field(
    dont_save_if_ban_kicked: bool,
    include_bots: bool,
    save_nicknames: bool,
) -> String {
    let mut parts: Vec<&str> = Vec::with_capacity(3);
    if dont_save_if_ban_kicked {
        parts.push("`exclude-kicks`");
    }
    if include_bots {
        parts.push("`include-bots`");
    }
    if save_nicknames {
        parts.push("`save-nicknames`");
    }
    if parts.is_empty() {
        "*none*".to_owned()
    } else {
        parts.join(", ")
    }
}

fn expiry_label(secs: u32) -> &'static str {
    if secs == 0 {
        return "never";
    }
    crate::commands::sticky::EXPIRY_CHOICES
        .iter()
        .find_map(|(s, label)| (*s == secs).then_some(*label))
        .unwrap_or("custom")
}

/// Ephemeral early-error: every issue on its own `- ` line; nothing applied.
pub(crate) fn validation_error_message(issues: &[String]) -> String {
    let mut out = String::from("Couldn't save sticky roles — fix these and try again:");
    for issue in issues {
        out.push_str("\n- ");
        out.push_str(issue);
    }
    out
}

/// Terse restore-complete message; detail sentences only for nonzero counts.
pub(crate) fn format_restore_complete(
    remembered: usize,
    skipped_invalid: usize,
    skipped_expired: usize,
    skipped_truncated: usize,
    skipped_current_members: usize,
    skipped_untracked_roles: usize,
    skipped_nicknames: usize,
) -> String {
    let user_word = if remembered == 1 { "user" } else { "users" };
    let mut msg = format!(
        "Restore complete — previous entries were cleared. Remembering {remembered} {user_word}."
    );
    if skipped_invalid > 0 {
        let row_word = if skipped_invalid == 1 { "row" } else { "rows" };
        msg.push_str(&format!(
            " Skipped {skipped_invalid} bad {row_word} (including future-dated)."
        ));
    }
    if skipped_expired > 0 {
        let row_word = if skipped_expired == 1 { "row" } else { "rows" };
        msg.push_str(&format!(" Skipped {skipped_expired} expired {row_word}."));
    }
    if skipped_current_members > 0 {
        let row_word = if skipped_current_members == 1 {
            "member"
        } else {
            "members"
        };
        msg.push_str(&format!(
            " Skipped {skipped_current_members} current {row_word} (already in server)."
        ));
    }
    if skipped_truncated > 0 {
        msg.push_str(&format!(
            " Kept newest 25,000 entries, dropped {skipped_truncated} oldest."
        ));
    }
    if skipped_nicknames > 0 {
        let nick_word = if skipped_nicknames == 1 {
            "nickname"
        } else {
            "nicknames"
        };
        msg.push_str(&format!(
            " Skipped {skipped_nicknames} {nick_word} (nickname saving is off)."
        ));
    }
    if skipped_untracked_roles > 0 {
        msg.push(' ');
        msg.push_str(UNTRACKED_NOTE);
    }
    msg
}
