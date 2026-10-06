//! Slash-command routing: `/sticky-roles` + `/data`, `sticky:*` components, modals.

pub mod data;
pub mod messages;
pub mod sticky;

use twilight_model::{
    application::{
        command::{Command, CommandType},
        interaction::{
            Interaction, InteractionContextType, InteractionData, InteractionType,
            application_command::{CommandData, CommandOptionValue},
            message_component::MessageComponentInteractionData,
            modal::ModalInteractionData,
        },
    },
    channel::message::{AllowedMentions, Component, MessageFlags},
    guild::Permissions,
    http::interaction::{InteractionResponse, InteractionResponseData, InteractionResponseType},
    id::{
        Id,
        marker::{ChannelMarker, GuildMarker, RoleMarker, UserMarker},
    },
    oauth::ApplicationIntegrationType,
};
use twilight_util::builder::command::{CommandBuilder, SubCommandBuilder};

use crate::store::{GuildConfig, persist};

// Custom IDs.
pub const CUSTOM_ID_STICKY_ROLES: &str = "sticky:roles";
pub const CUSTOM_ID_STICKY_LOG: &str = "sticky:log";
pub const CUSTOM_ID_STICKY_OPTS: &str = "sticky:opts";
pub const CUSTOM_ID_STICKY_EXPIRY: &str = "sticky:expiry";
pub const CUSTOM_ID_STICKY_CFG_MODAL: &str = "sticky:cfg";
pub const CUSTOM_ID_DATA_RESTORE_MODAL: &str = "data:restore";
pub const CUSTOM_ID_DATA_CSV_FILE: &str = "data:csvfile";

// Shared per-guild cooldown: bounded map, TTL sweep, oldest-evict.
pub(crate) const COOLDOWN_CAP: usize = 10_000;
pub(crate) const COOLDOWN_TTL_SECS: u64 = 3_600;
pub(crate) type CooldownMap = std::sync::Mutex<rustc_hash::FxHashMap<u64, std::time::Instant>>;

/// Cache-miss reply: guild data not yet cached (the interaction would
/// otherwise time out with no response).
pub(crate) const STARTUP_MSG: &str = "I'm still starting up; please try again in a few seconds.";

fn cooldown_inner(
    map: &CooldownMap,
    guild: u64,
    window_secs: u64,
    now: std::time::Instant,
    record: bool,
) -> bool {
    let Ok(mut guard) = map.lock() else {
        return true;
    };
    if let Some(last) = guard.get(&guild)
        && now.saturating_duration_since(*last).as_secs() < window_secs
    {
        return false;
    }
    if record {
        if guard.len() >= COOLDOWN_CAP {
            guard.retain(|_, last| {
                now.saturating_duration_since(*last).as_secs() < COOLDOWN_TTL_SECS
            });
            if guard.len() >= COOLDOWN_CAP
                && let Some(oldest) = guard.iter().min_by_key(|(_, last)| **last).map(|(k, _)| *k)
            {
                guard.remove(&oldest);
            }
        }
        guard.insert(guild, now);
    }
    true
}

/// True when the guild may act now (records). Fail-open on poisoned mutex.
pub(crate) fn cooldown_check_at(
    map: &CooldownMap,
    guild: u64,
    window_secs: u64,
    now: std::time::Instant,
) -> bool {
    cooldown_inner(map, guild, window_secs, now, true)
}

/// True when the guild may act now (peek: never records, never evicts). Fail-open.
pub(crate) fn cooldown_peek_at(
    map: &CooldownMap,
    guild: u64,
    window_secs: u64,
    now: std::time::Instant,
) -> bool {
    cooldown_inner(map, guild, window_secs, now, false)
}

/// Drop entries idle longer than `COOLDOWN_TTL_SECS`.
pub(crate) fn cooldown_sweep_at(map: &CooldownMap, now: std::time::Instant) -> usize {
    let Ok(mut guard) = map.lock() else {
        return 0;
    };
    let before = guard.len();
    guard.retain(|_, last| now.saturating_duration_since(*last).as_secs() < COOLDOWN_TTL_SECS);
    before - guard.len()
}

/// Drop one guild's entry (guild-delete path).
pub(crate) fn cooldown_evict(map: &CooldownMap, guild: u64) -> bool {
    map.lock()
        .map(|mut g| g.remove(&guild).is_some())
        .unwrap_or(false)
}

/// Hourly sweep over all per-guild cooldown maps. Returns total removed.
pub(crate) fn sweep_all_cooldowns(now: std::time::Instant) -> usize {
    cooldown_sweep_at(sticky::submit_last(), now)
        + cooldown_sweep_at(messages::log_last(), now)
        + cooldown_sweep_at(data::restore_last(), now)
}

/// Drop one guild from every cooldown map (guild-delete path).
pub(crate) fn evict_guild_cooldowns(guild_id: Id<GuildMarker>) {
    cooldown_evict(sticky::submit_last(), guild_id.get());
    cooldown_evict(messages::log_last(), guild_id.get());
    cooldown_evict(data::restore_last(), guild_id.get());
}

/// Global commands registered at startup (guild-only).
pub fn global_commands() -> Vec<Command> {
    // Visibility mirrors the handler gate (roles + guild management).
    let manage: Permissions = Permissions::MANAGE_ROLES | Permissions::MANAGE_GUILD;
    vec![
        CommandBuilder::new(
            "sticky-roles",
            "Configure the sticky roles configuration for this server",
            CommandType::ChatInput,
        )
        .default_member_permissions(manage)
        .contexts([InteractionContextType::Guild])
        .integration_types([ApplicationIntegrationType::GuildInstall])
        .build(),
        CommandBuilder::new(
            "data",
            "Back up or restore sticky-role data for this server",
            CommandType::ChatInput,
        )
        .default_member_permissions(manage)
        .contexts([InteractionContextType::Guild])
        .integration_types([ApplicationIntegrationType::GuildInstall])
        .option(SubCommandBuilder::new(
            "save",
            "Export this guild's sticky data as CSV",
        ))
        .option(SubCommandBuilder::new(
            "restore",
            "Replace this guild's sticky data from a CSV backup",
        ))
        .build(),
    ]
}

/// Route one interaction to its handler.
pub async fn handle_interaction(interaction: Interaction) {
    match interaction.kind {
        InteractionType::Ping => {
            send_response(
                &interaction,
                &InteractionResponse {
                    kind: InteractionResponseType::Pong,
                    data: None,
                },
            )
            .await;
        }
        InteractionType::ApplicationCommand => match &interaction.data {
            Some(InteractionData::ApplicationCommand(cmd)) => {
                handle_application_command(&interaction, cmd).await;
            }
            _ => respond_broken(&interaction).await,
        },
        InteractionType::MessageComponent => match &interaction.data {
            Some(InteractionData::MessageComponent(data)) => {
                handle_message_component(&interaction, data).await;
            }
            _ => respond_broken(&interaction).await,
        },
        InteractionType::ModalSubmit => match &interaction.data {
            Some(InteractionData::ModalSubmit(data)) => {
                handle_modal_submit(&interaction, data).await;
            }
            _ => respond_broken(&interaction).await,
        },
        // Unhandled by design: silent-drop (no response, no log).
        _ => {}
    }
}

async fn handle_application_command(interaction: &Interaction, cmd: &CommandData) {
    match cmd.name.as_str() {
        "sticky-roles" => sticky::handle_sticky_roles(interaction).await,
        "data" => {
            let sub = cmd.options.iter().find_map(|opt| {
                if matches!(opt.value, CommandOptionValue::SubCommand(_)) {
                    Some(opt.name.as_str())
                } else {
                    None
                }
            });
            match sub {
                Some("save") => data::handle_data_save(interaction).await,
                Some("restore") => data::handle_data_restore(interaction).await,
                _ => {
                    respond_ephemeral(
                        interaction,
                        "Unknown `/data` subcommand; try `/data save` or `/data restore`."
                            .to_owned(),
                    )
                    .await;
                }
            }
        }
        _ => {
            respond_ephemeral(
                interaction,
                "I don't recognize that command. Try `/sticky-roles` or `/data`.".to_owned(),
            )
            .await;
        }
    }
}

async fn handle_message_component(
    interaction: &Interaction,
    _data: &MessageComponentInteractionData,
) {
    // No live path sends components, so every `sticky:*` component is stale.
    respond_ephemeral(
        interaction,
        "That button is out of date. Run `/sticky-roles` again.".to_owned(),
    )
    .await;
}

/// Fallback for interactions with missing payload.
async fn respond_broken(interaction: &Interaction) {
    respond_ephemeral(
        interaction,
        "Something went wrong; please try again.".to_owned(),
    )
    .await;
}

async fn handle_modal_submit(interaction: &Interaction, data: &ModalInteractionData) {
    match data.custom_id.as_str() {
        CUSTOM_ID_STICKY_CFG_MODAL => {
            sticky::handle_sticky_submit(interaction, data).await;
        }
        CUSTOM_ID_DATA_RESTORE_MODAL => data::handle_restore_submit(interaction, data).await,
        _ => {
            respond_ephemeral(
                interaction,
                "That form is out of date. Please run the command again.".to_owned(),
            )
            .await;
        }
    }
}

// Permission helpers: invoker needs MANAGE_ROLES + MANAGE_GUILD (owner/ADMIN bypass).

/// Invoker's guild permissions (`member.permissions`; empty outside guilds).
pub(crate) fn invoker_permissions(interaction: &Interaction) -> Permissions {
    interaction
        .member
        .as_ref()
        .and_then(|m| m.permissions)
        .unwrap_or_else(Permissions::empty)
}

pub(crate) fn invoker_id(interaction: &Interaction) -> Option<Id<UserMarker>> {
    interaction.author_id()
}

/// Guild-only gate: returns the guild id, or sends `msg` ephemeral and returns `None`.
pub(crate) async fn require_guild(interaction: &Interaction, msg: &str) -> Option<Id<GuildMarker>> {
    if let Some(guild_id) = interaction.guild_id {
        Some(guild_id)
    } else {
        respond_ephemeral(interaction, msg.to_owned()).await;
        None
    }
}

/// One-shot permission snapshot: everything the gates need from a single cache read.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Gate {
    pub is_owner: bool,
    pub invoker_top: i16,
    pub invoker_perms: Permissions,
    pub bot_top: i16,
    pub bot_perms: Permissions,
}

impl Gate {
    fn has_grant(top: i16, perms: Permissions, is_owner: bool) -> bool {
        crate::hierarchy::has_manage_grant(&crate::hierarchy::Actor {
            top,
            perms,
            is_owner,
        })
    }

    pub(crate) fn invoker_granted(&self) -> bool {
        self.is_owner
            || self.invoker_perms.contains(Permissions::ADMINISTRATOR)
            || (self.invoker_perms.contains(Permissions::MANAGE_ROLES)
                && self.invoker_perms.contains(Permissions::MANAGE_GUILD))
    }

    pub(crate) fn bot_granted(&self) -> bool {
        Self::has_grant(self.bot_top, self.bot_perms, false)
    }
}

/// Pure snapshot: `None` on cache miss or unknown invoker (no response sent).
pub(crate) async fn read_gate(
    interaction: &Interaction,
    guild_id: Id<GuildMarker>,
) -> Option<Gate> {
    let invoker = invoker_id(interaction)?;
    let guard = crate::events::cache().read().await;
    let g = guard.guilds.get(&guild_id)?;
    Some(Gate {
        is_owner: invoker == g.owner_id,
        invoker_top: g.member_top(invoker),
        invoker_perms: invoker_permissions(interaction),
        bot_top: g.bot_top,
        bot_perms: interaction.app_permissions.unwrap_or(g.bot_perms),
    })
}

/// Invoker gate: ephemeral denial on failure, `false` on miss/deny.
pub(crate) async fn check_invoker(interaction: &Interaction, guild_id: Id<GuildMarker>) -> bool {
    let Some(gate) = read_gate(interaction, guild_id).await else {
        respond_ephemeral(interaction, STARTUP_MSG.to_owned()).await;
        return false;
    };
    if gate.invoker_granted() {
        true
    } else {
        respond_ephemeral(
            interaction,
            "You need the Manage Roles and Manage Guild permissions to use this.".to_owned(),
        )
        .await;
        false
    }
}

/// Combined invoker + bot gate. Sends exactly one denial.
pub(crate) async fn gate_full(
    interaction: &Interaction,
    guild_id: Id<GuildMarker>,
    bot_deny: &str,
) -> Option<Gate> {
    let Some(gate) = read_gate(interaction, guild_id).await else {
        respond_ephemeral(interaction, STARTUP_MSG.to_owned()).await;
        return None;
    };
    if !gate.invoker_granted() {
        respond_ephemeral(
            interaction,
            "You need the Manage Roles and Manage Guild permissions to use this.".to_owned(),
        )
        .await;
        return None;
    }
    if !gate.bot_granted() {
        respond_ephemeral(interaction, bot_deny.to_owned()).await;
        return None;
    }
    Some(gate)
}

// Responses. No-ping: every send sets allowed_mentions none.

/// Internal: send a pre-built interaction response (logs HTTP errors).
async fn send_response(interaction: &Interaction, response: &InteractionResponse) {
    if let Err(e) = crate::CTX
        .interaction()
        .create_response(interaction.id, &interaction.token, response)
        .await
    {
        messages::warn_http(
            &e,
            "failed to send interaction response; user got no reply, old data kept",
        );
    }
}

/// `ChannelMessageWithSource` reply (no-ping). `ephemeral` controls visibility.
pub(crate) async fn respond(interaction: &Interaction, content: String, ephemeral: bool) {
    let flags = ephemeral.then_some(MessageFlags::EPHEMERAL);
    send_response(
        interaction,
        &InteractionResponse {
            kind: InteractionResponseType::ChannelMessageWithSource,
            data: Some(InteractionResponseData {
                content: Some(content),
                flags,
                allowed_mentions: Some(AllowedMentions::default()),
                ..Default::default()
            }),
        },
    )
    .await;
}

/// Ephemeral `ChannelMessageWithSource` reply.
pub(crate) async fn respond_ephemeral(interaction: &Interaction, content: String) {
    respond(interaction, content, true).await;
}

/// Deferred ack (3s deadline); followup edit carries the body.
pub(crate) async fn send_deferred(interaction: &Interaction, ephemeral: bool) -> Result<(), ()> {
    let data = ephemeral.then(|| InteractionResponseData {
        flags: Some(MessageFlags::EPHEMERAL),
        ..Default::default()
    });
    let response = InteractionResponse {
        kind: InteractionResponseType::DeferredChannelMessageWithSource,
        data,
    };
    if let Err(e) = crate::CTX
        .interaction()
        .create_response(interaction.id, &interaction.token, &response)
        .await
    {
        messages::warn_http(
            &e,
            "failed to send deferred ack; user got no ack, old data kept",
        );
        return Err(());
    }
    Ok(())
}

/// Ephemeral followup with a pre-removal backup file (mentions render, never ping).
pub(crate) async fn send_ephemeral_backup_followup(
    interaction: &Interaction,
    content: &str,
    filename: String,
    csv: Vec<u8>,
) {
    let attachment = twilight_model::http::attachment::Attachment {
        description: Some(
            "Pre-removal full backup (restore only after re-enabling roles)".to_owned(),
        ),
        file: csv,
        filename,
        id: 1,
        duration_secs: None,
        is_spoiler: None,
        title: None,
        waveform: None,
    };
    if let Err(e) = crate::CTX
        .interaction()
        .create_followup(&interaction.token)
        .content(content)
        .attachments(&[attachment])
        .allowed_mentions(Some(&AllowedMentions::default()))
        .flags(MessageFlags::EPHEMERAL)
        .await
    {
        messages::warn_http(
            &e,
            "failed to send pre-removal backup followup; user got no backup file, old data kept",
        );
    }
}

/// Edit the deferred followup (ephemeral re-asserted when set).
pub(crate) async fn edit_followup(interaction: &Interaction, content: &str, ephemeral: bool) {
    let client = crate::CTX.interaction();
    let res = if ephemeral {
        client
            .update_response(&interaction.token)
            .allowed_mentions(Some(&AllowedMentions::default()))
            .content(Some(content))
            .flags(MessageFlags::EPHEMERAL)
            .await
    } else {
        client
            .update_response(&interaction.token)
            .allowed_mentions(Some(&AllowedMentions::default()))
            .content(Some(content))
            .await
    };
    if let Err(e) = res {
        messages::warn_http(
            &e,
            "failed to edit followup; user sees stale ack, old data kept",
        );
    }
}

/// Open a modal; ephemeral fallback when the open itself fails.
pub(crate) async fn open_modal(
    interaction: &Interaction,
    title: &str,
    custom_id: &str,
    components: Vec<Component>,
    fallback: &str,
) {
    let response = InteractionResponse {
        kind: InteractionResponseType::Modal,
        data: Some(InteractionResponseData {
            custom_id: Some(custom_id.to_owned()),
            title: Some(title.to_owned()),
            components: Some(components),
            ..Default::default()
        }),
    };
    if let Err(e) = crate::CTX
        .interaction()
        .create_response(interaction.id, &interaction.token, &response)
        .await
    {
        messages::warn_http(
            &e,
            "failed to open modal; user got no modal, settings unchanged",
        );
        respond_ephemeral(interaction, fallback.to_owned()).await;
    }
}

/// Persist one guild's full config. Err text is the user-facing message.
pub(crate) async fn persist_guild_config(
    guild_id: Id<GuildMarker>,
    roles: &[Id<RoleMarker>],
    log_channel: Option<Id<ChannelMarker>>,
    dont_save_if_ban_kicked: bool,
    include_bots: bool,
    save_nicknames: bool,
    expiry_secs: u32,
) -> Result<(GuildConfig, usize), &'static str> {
    let out = persist::append_config(
        guild_id,
        roles,
        log_channel,
        dont_save_if_ban_kicked,
        include_bots,
        save_nicknames,
        expiry_secs,
    )
    .await
    .map_err(|e| {
        tracing::warn!(
            error = ?e,
            "failed to persist guild config; settings unchanged, old data kept"
        );
        "couldn't save those settings; nothing was changed, please try again"
    })?;
    // Flag-off frees the transient cache sidecar (durable nicks retained).
    if !save_nicknames {
        crate::events::cache().write().await.clear_nicks(guild_id);
    }
    Ok(out)
}
