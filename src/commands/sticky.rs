//! `/sticky-roles` single-modal config (`sticky:cfg`); direct public ack, no-ping.

use twilight_model::{
    application::interaction::{
        Interaction,
        modal::{ModalInteractionComponent, ModalInteractionData},
    },
    channel::{
        ChannelType,
        message::{
            Component,
            component::{SelectDefaultValue, SelectMenuType},
        },
    },
    guild::Permissions,
    id::{
        Id,
        marker::{ChannelMarker, GuildMarker, RoleMarker},
    },
};
use twilight_util::builder::message::{
    CheckboxGroupBuilder, CheckboxGroupOptionBuilder, LabelBuilder, SelectMenuBuilder,
    SelectMenuOptionBuilder,
};

use crate::store::{
    DEFAULT_EXPIRY_SECS, DURATION_1D_SECS, DURATION_1MO_SECS, DURATION_1W_SECS, DURATION_1Y_SECS,
    DURATION_6MO_SECS, GuildConfig, export_guild, registry,
};
use crate::{
    commands::messages,
    hierarchy::{Actor, Target, can_assign, is_everyone},
};

use super::{
    CUSTOM_ID_STICKY_CFG_MODAL, CUSTOM_ID_STICKY_EXPIRY, CUSTOM_ID_STICKY_LOG,
    CUSTOM_ID_STICKY_OPTS, CUSTOM_ID_STICKY_ROLES,
};

const OPT_EXCLUDE_KICKS: &str = "opt:exclude-kicks";
const OPT_BOTS: &str = "opt:bots";
const OPT_NICKS: &str = "opt:nicks";

/// Picker cap (10) vs tracking cap (`store::MAX_ROLES`, 25).
const MAX_PICK_ROLES: usize = 10;

/// Per-guild modal-submit cooldown (bounds WAL fsync spam).
const SUBMIT_COOLDOWN_SECS: u64 = 5;

/// Guild-only gate message for `/sticky-roles`.
pub(crate) const STICKY_GUILD_ONLY_MSG: &str = "Use `/sticky-roles` inside a server.";

static SUBMIT_LAST: std::sync::OnceLock<super::CooldownMap> = std::sync::OnceLock::new();

pub(crate) fn submit_last() -> &'static super::CooldownMap {
    SUBMIT_LAST.get_or_init(|| super::CooldownMap::new(rustc_hash::FxHashMap::default()))
}

/// Expiry picker choices (default 1 month).
pub(crate) const EXPIRY_CHOICES: [(u32, &str); 5] = [
    (DURATION_1D_SECS, "1 day"),
    (DURATION_1W_SECS, "1 week"),
    (DURATION_1MO_SECS, "1 month"),
    (DURATION_6MO_SECS, "6 months"),
    (DURATION_1Y_SECS, "1 year"),
];

/// Log-channel picker types.
const LOG_CHANNEL_TYPES: [ChannelType; 5] = [
    ChannelType::GuildText,
    ChannelType::GuildAnnouncement,
    ChannelType::AnnouncementThread,
    ChannelType::PublicThread,
    ChannelType::PrivateThread,
];

/// Open the single `sticky:cfg` modal, pre-selected from the current config.
pub(crate) async fn handle_sticky_roles(interaction: &Interaction) {
    let Some(guild_id) = super::require_guild(interaction, STICKY_GUILD_ONLY_MSG).await else {
        return;
    };
    if super::gate_full(
        interaction,
        guild_id,
        "I need the Manage Roles permission (or Administrator) before sticky roles can be configured.",
    )
    .await
    .is_none()
    {
        return;
    }

    // Pre-selects fall back to defaults; opening never creates a store.
    let (role_ids, log_channel, no_kicks, bots, nicks, expiry) =
        registry::with_guild(guild_id, |s| {
            (
                s.config.role_ids(),
                s.config.log_channel,
                s.config.dont_save_if_ban_kicked,
                s.config.include_bots,
                s.config.save_nicknames,
                s.config.expiry_secs,
            )
        })
        .await
        .unwrap_or((Vec::new(), None, true, true, false, DEFAULT_EXPIRY_SECS));

    super::open_modal(
        interaction,
        "Sticky roles setup",
        CUSTOM_ID_STICKY_CFG_MODAL,
        modal_components(&role_ids, log_channel, no_kicks, bots, nicks, expiry),
        "Couldn't open sticky-roles setup. Please try again.",
    )
    .await;
}

/// Explainer + four Label-wrapped inputs, pre-selected from current config.
fn modal_components(
    roles: &[Id<RoleMarker>],
    log: Option<Id<ChannelMarker>>,
    no_kicks: bool,
    bots: bool,
    nicks: bool,
    expiry_secs: u32,
) -> Vec<Component> {
    let roles_menu = SelectMenuBuilder::new(CUSTOM_ID_STICKY_ROLES, SelectMenuType::Role)
        .max_values(MAX_PICK_ROLES as u8)
        .min_values(0)
        .required(false)
        .placeholder(format!("Select roles to track (up to {MAX_PICK_ROLES})"))
        .default_values(
            roles
                .iter()
                .copied()
                .map(SelectDefaultValue::Role)
                .collect(),
        )
        .build();
    let log_menu = SelectMenuBuilder::new(CUSTOM_ID_STICKY_LOG, SelectMenuType::Channel)
        .channel_types(LOG_CHANNEL_TYPES.to_vec())
        .max_values(1)
        .min_values(0)
        .required(false)
        .placeholder("Log channel (clear selection to disable)")
        .default_values(
            log.map(|ch| vec![SelectDefaultValue::Channel(ch)])
                .unwrap_or_default(),
        )
        .build();
    let opts = CheckboxGroupBuilder::new(CUSTOM_ID_STICKY_OPTS)
        .option(
            CheckboxGroupOptionBuilder::new(OPT_EXCLUDE_KICKS, "Don't save bans/kicks")
                .description("Skip saving when a member was banned or kicked")
                .default(no_kicks)
                .build(),
        )
        .option(
            CheckboxGroupOptionBuilder::new(OPT_BOTS, "Include bots")
                .default(bots)
                .build(),
        )
        .option(
            CheckboxGroupOptionBuilder::new(OPT_NICKS, "Save nicknames")
                .default(nicks)
                .build(),
        )
        .min_values(0)
        .max_values(3)
        .required(false)
        .build();
    let default_secs = if EXPIRY_CHOICES.iter().any(|(s, _)| *s == expiry_secs) {
        expiry_secs
    } else {
        DEFAULT_EXPIRY_SECS
    };
    let mut expiry = SelectMenuBuilder::new(CUSTOM_ID_STICKY_EXPIRY, SelectMenuType::Text)
        .min_values(1)
        .max_values(1)
        .required(true)
        .placeholder("How long saved roles last");
    for (secs, label) in EXPIRY_CHOICES {
        expiry = expiry.option(
            SelectMenuOptionBuilder::new(label, secs.to_string())
                .default(secs == default_secs)
                .build(),
        );
    }

    vec![
        Component::Label(
            LabelBuilder::new("Tracked roles", Component::SelectMenu(roles_menu))
                .description("Roles to restore to members upon rejoin")
                .build(),
        ),
        Component::Label(
            LabelBuilder::new("Log channel", Component::SelectMenu(log_menu))
                .description("The channel to log members who received a role")
                .build(),
        ),
        Component::Label(LabelBuilder::new("Options", Component::CheckboxGroup(opts)).build()),
        Component::Label(
            LabelBuilder::new("Keep saved data for", Component::SelectMenu(expiry.build()))
                .description("How long a leaver's roles are kept")
                .build(),
        ),
    ]
}

#[derive(Debug, Default)]
struct Submit {
    roles: Vec<Id<RoleMarker>>,
    log: Option<Id<ChannelMarker>>,
    no_kicks: bool,
    bots: bool,
    nicks: bool,
    expiry: Option<u32>,
}

fn parse_submit(data: &ModalInteractionData) -> Submit {
    fn visit(comp: &ModalInteractionComponent, out: &mut Submit) {
        match comp {
            ModalInteractionComponent::Label(l) => visit(&l.component, out),
            ModalInteractionComponent::ActionRow(r) => {
                for inner in &r.components {
                    visit(inner, out);
                }
            }
            ModalInteractionComponent::RoleSelect(s) if s.custom_id == CUSTOM_ID_STICKY_ROLES => {
                out.roles.extend_from_slice(&s.values);
            }
            ModalInteractionComponent::ChannelSelect(s) if s.custom_id == CUSTOM_ID_STICKY_LOG => {
                out.log = s.values.first().copied();
            }
            ModalInteractionComponent::CheckboxGroup(g) if g.custom_id == CUSTOM_ID_STICKY_OPTS => {
                for v in &g.values {
                    match v.as_str() {
                        OPT_EXCLUDE_KICKS => out.no_kicks = true,
                        OPT_BOTS => out.bots = true,
                        OPT_NICKS => out.nicks = true,
                        _ => {}
                    }
                }
            }
            ModalInteractionComponent::StringSelect(s)
                if s.custom_id == CUSTOM_ID_STICKY_EXPIRY =>
            {
                out.expiry = s.values.first().and_then(|v| v.parse().ok());
            }
            _ => {}
        }
    }

    let mut out = Submit::default();
    for comp in &data.components {
        visit(comp, &mut out);
    }
    out.roles.sort_unstable();
    out.roles.dedup();
    out.roles.truncate(MAX_PICK_ROLES);
    out
}

/// I-side vs you-side wording for the same denial.
enum Side {
    Bot,
    Invoker,
}

fn role_issue(side: Side, actor: &Actor, target: &Target, rid: Id<RoleMarker>) -> Option<String> {
    let (managed, no_grant, hierarchy) = match side {
        Side::Bot => (
            "it's managed by Discord, I can't assign it.",
            "I lack Manage Roles, so I can't track it.",
            "it's above my top role, I can't manage it.",
        ),
        Side::Invoker => (
            "it's managed by Discord, nobody can assign it.",
            "you lack Manage Roles, so you can't track it.",
            "it's above your top role, you can't manage it.",
        ),
    };
    let reason = match can_assign(actor, target) {
        Ok(()) => return None,
        Err(crate::hierarchy::Deny::Everyone) => "@everyone can't be tracked.",
        Err(crate::hierarchy::Deny::Managed) => managed,
        Err(crate::hierarchy::Deny::NoGrant) => no_grant,
        Err(crate::hierarchy::Deny::Hierarchy) => hierarchy,
    };
    Some(format!("<@&{}>: {reason}", rid.get()))
}

/// Bot + invoker gates per role; any issue aborts the whole submit.
async fn filter_roles(
    guild_id: Id<GuildMarker>,
    gate: &super::Gate,
    wanted: &[Id<RoleMarker>],
) -> (Vec<Id<RoleMarker>>, Vec<String>) {
    let invoker = Actor {
        top: gate.invoker_top,
        perms: gate.invoker_perms,
        is_owner: gate.is_owner,
    };
    let bot = Actor {
        top: gate.bot_top,
        perms: gate.bot_perms,
        is_owner: false,
    };

    let guard = crate::events::cache().read().await;
    let Some(g) = guard.guilds.get(&guild_id) else {
        return (Vec::new(), vec![super::STARTUP_MSG.to_owned()]);
    };
    let mut valid = Vec::with_capacity(wanted.len());
    let mut issues = Vec::new();
    for &rid in wanted {
        if is_everyone(rid, guild_id) {
            issues.push(format!("<@&{}>: @everyone can't be tracked.", rid.get()));
            continue;
        }
        let Some(role) = g.roles.get(&rid) else {
            issues.push(format!("<@&{}>: that role no longer exists.", rid.get()));
            continue;
        };
        let target = Target {
            is_everyone: false,
            pos: role.position,
            managed: role.managed,
        };
        if let Some(issue) = role_issue(Side::Bot, &bot, &target, rid) {
            issues.push(issue);
            continue;
        }
        if let Some(issue) = role_issue(Side::Invoker, &invoker, &target, rid) {
            issues.push(issue);
            continue;
        }
        valid.push(rid);
    }
    (valid, issues)
}

fn log_issues(
    invoker: &Actor,
    data: &ModalInteractionData,
    channel: Option<Id<ChannelMarker>>,
) -> Vec<String> {
    let Some(channel) = channel else {
        return Vec::new();
    };
    let unreadable = || {
        vec![format!(
            "<#{id}> I couldn't read that channel; re-pick it.",
            id = channel.get(),
        )]
    };
    let Some(ch) = data
        .resolved
        .as_ref()
        .and_then(|r| r.channels.get(&channel))
    else {
        return unreadable();
    };
    if !LOG_CHANNEL_TYPES.contains(&ch.kind) {
        return vec![format!(
            "<#{id}> isn't a text, announcement, or thread channel; pick one of those.",
            id = channel.get(),
        )];
    }
    let is_thread = matches!(
        ch.kind,
        ChannelType::AnnouncementThread | ChannelType::PublicThread | ChannelType::PrivateThread
    );
    // No guild-level fallback: without resolved per-channel bot perms, treat as unreadable.
    let Some(bot_perms) = ch.app_permissions else {
        return unreadable();
    };
    let mut issues = Vec::with_capacity(2);
    if !can_send_in(&bot_perms, is_thread) {
        issues.push(format!(
            "I can't send messages in <#{id}> — I need View Channel and Send Messages there.",
            id = channel.get(),
        ));
    }
    if !invoker.is_owner && !can_send_in(&ch.permissions, is_thread) {
        issues.push(format!(
            "You can't send messages in <#{id}> — you need View Channel and Send Messages there.",
            id = channel.get(),
        ));
    }
    issues
}

fn option_issues(gate: &super::Gate, no_kicks: bool, nicks: bool) -> Vec<String> {
    let mut issues = Vec::new();
    if no_kicks {
        let bot_ok = gate.bot_perms.contains(Permissions::VIEW_AUDIT_LOG)
            || gate.bot_perms.contains(Permissions::ADMINISTRATOR);
        let invoker_ok = gate.is_owner
            || gate.invoker_perms.contains(Permissions::VIEW_AUDIT_LOG)
            || gate.invoker_perms.contains(Permissions::ADMINISTRATOR);
        if !bot_ok {
            issues.push(
                "I need View Audit Log to skip bans and kicks; grant it to me or turn the option off."
                    .to_owned(),
            );
        }
        if !invoker_ok {
            issues.push(
                "You need View Audit Log to skip bans and kicks; get it granted or turn the option off."
                    .to_owned(),
            );
        }
    }
    if nicks {
        let bot_ok = gate.bot_perms.contains(Permissions::MANAGE_NICKNAMES)
            || gate.bot_perms.contains(Permissions::ADMINISTRATOR);
        if !bot_ok {
            issues.push(
                "I need Manage Nicknames to save nicknames; grant it to me or turn the option off."
                    .to_owned(),
            );
        }
    }
    issues
}

fn can_send_in(perms: &Permissions, is_thread: bool) -> bool {
    if perms.contains(Permissions::ADMINISTRATOR) {
        return true;
    }
    if !perms.contains(Permissions::VIEW_CHANNEL) {
        return false;
    }
    if perms.contains(Permissions::SEND_MESSAGES) {
        return true;
    }
    is_thread && perms.contains(Permissions::SEND_MESSAGES_IN_THREADS)
}

/// Pre-removal CSV backup: snapshot BEFORE the swap, only when removed roles
/// still hold members. Also reports whether the backup carries nicknames.
async fn removed_backup(
    guild_id: Id<GuildMarker>,
    valid: &[Id<RoleMarker>],
) -> Option<(String, Vec<(Id<RoleMarker>, usize)>, bool)> {
    let now = registry::now_secs();
    registry::with_guild(guild_id, |old| {
        let removed: Vec<Id<RoleMarker>> = old
            .config
            .role_ids()
            .into_iter()
            .filter(|r| !valid.contains(r))
            .collect();
        if removed.is_empty() {
            return None;
        }
        let counts = old.holder_counts_for(&removed, now);
        if !counts.iter().any(|(_, n)| *n > 0) {
            return None;
        }
        let mut mask = 0u32;
        for r in &removed {
            if let Some(i) = old.config.index_of(*r) {
                mask |= crate::store::bit(i);
            }
        }
        let nicked = old.config.save_nicknames
            && old
                .map
                .iter()
                .any(|(uid, e)| e.bits & mask != 0 && old.nicks.contains_key(uid));
        Some((export_guild(old), counts, nicked))
    })
    .await?
}

/// Validate then persist once. Expiry aborts alone; other issues batch into one message.
pub(crate) async fn handle_sticky_submit(interaction: &Interaction, data: &ModalInteractionData) {
    let slow_down =
        "Slow down — that setup was just saved. Wait a few seconds and try again.".to_owned();
    let Some(guild_id) = super::require_guild(interaction, STICKY_GUILD_ONLY_MSG).await else {
        return;
    };
    if !super::cooldown_peek_at(
        submit_last(),
        guild_id.get(),
        SUBMIT_COOLDOWN_SECS,
        std::time::Instant::now(),
    ) {
        super::respond_ephemeral(interaction, slow_down).await;
        return;
    }
    let Some(gate) = super::gate_full(
        interaction,
        guild_id,
        "I need the Manage Roles permission (or Administrator) to track roles.",
    )
    .await
    else {
        return;
    };

    let values = parse_submit(data);
    let Some(expiry_secs) = values.expiry else {
        super::respond_ephemeral(
            interaction,
            "Choose how long to keep saved roles.".to_owned(),
        )
        .await;
        return;
    };
    if !EXPIRY_CHOICES.iter().any(|(s, _)| *s == expiry_secs) {
        super::respond_ephemeral(
            interaction,
            "That length isn't one of the choices; please pick one of the listed options."
                .to_owned(),
        )
        .await;
        return;
    }

    let invoker = Actor {
        top: gate.invoker_top,
        perms: gate.invoker_perms,
        is_owner: gate.is_owner,
    };
    let (valid_roles, mut issues) = filter_roles(guild_id, &gate, &values.roles).await;
    issues.extend(log_issues(&invoker, data, values.log));
    issues.extend(option_issues(&gate, values.no_kicks, values.nicks));
    if !issues.is_empty() {
        super::respond_ephemeral(interaction, messages::validation_error_message(&issues)).await;
        return;
    }

    // Burn the budget only when heading to persist; validation aborts don't.
    if !super::cooldown_check_at(
        submit_last(),
        guild_id.get(),
        SUBMIT_COOLDOWN_SECS,
        std::time::Instant::now(),
    ) {
        super::respond_ephemeral(interaction, slow_down).await;
        return;
    }

    registry::ensure_guild(guild_id, GuildConfig::default()).await;
    let backup = removed_backup(guild_id, &valid_roles).await;
    // Nicknames dropped by turning the flag off ride the same followup.
    let dropped_nicks = if !values.nicks {
        registry::with_guild(guild_id, |s| {
            if s.config.save_nicknames {
                s.nicks.len()
            } else {
                0
            }
        })
        .await
        .unwrap_or(0)
    } else {
        0
    };
    // One followup at most, always with a dump: the role backup when roles
    // were removed, else a fresh pre-swap export when only nicks drop.
    let mut followup_text = String::new();
    let mut followup_dump: Option<String> = None;
    if let Some((csv, counts, nicked)) = backup {
        followup_text.push_str(
            &messages::removal_backup_message(&counts, nicked)
                .expect("backup implies holders, message must exist"),
        );
        followup_dump = Some(csv);
    } else if dropped_nicks > 0 {
        followup_dump = registry::with_guild(guild_id, |s| export_guild(s)).await;
    }
    if dropped_nicks > 0 {
        if !followup_text.is_empty() {
            followup_text.push('\n');
        }
        followup_text.push_str(&format!(
            "Save nicknames was turned off, so {dropped_nicks} stored {} removed and will no longer restore.",
            if dropped_nicks == 1 {
                "nickname was"
            } else {
                "nicknames were"
            }
        ));
    }
    let (cfg, _) = match super::persist_guild_config(
        guild_id,
        &valid_roles,
        values.log,
        values.no_kicks,
        values.bots,
        values.nicks,
        expiry_secs,
    )
    .await
    {
        Ok(ok) => ok,
        Err(msg) => {
            super::respond_ephemeral(interaction, msg.to_owned()).await;
            return;
        }
    };

    super::respond(
        interaction,
        messages::success_message(
            &valid_roles,
            values.log,
            cfg.dont_save_if_ban_kicked,
            cfg.include_bots,
            cfg.save_nicknames,
            cfg.expiry_secs,
        ),
        false,
    )
    .await;

    if let Some(csv) = followup_dump {
        super::send_ephemeral_backup_followup(
            interaction,
            &followup_text,
            format!("sticky-roles-{}-pre-removal.csv", guild_id.get()),
            csv.into_bytes(),
        )
        .await;
    }
}
