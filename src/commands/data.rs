//! `/data save` + `/data restore` (CSV backup/restore).

use thiserror::Error;
use twilight_model::{
    application::interaction::{
        Interaction,
        modal::{ModalInteractionComponent, ModalInteractionData},
    },
    channel::{
        Attachment as ResolvedAttachment,
        message::{AllowedMentions, Component, MessageFlags},
    },
    http::{
        attachment::Attachment,
        interaction::{InteractionResponse, InteractionResponseData, InteractionResponseType},
    },
    id::{
        Id,
        marker::{AttachmentMarker, GuildMarker, RoleMarker},
    },
};
use twilight_util::builder::message::{FileUploadBuilder, LabelBuilder, TextDisplayBuilder};

use std::sync::{Arc, OnceLock};
use tokio::sync::Semaphore;

use crate::store::{DEFAULT_EXPIRY_SECS, csv, export_guild, persist, registry};

use super::{CUSTOM_ID_DATA_CSV_FILE, CUSTOM_ID_DATA_RESTORE_MODAL};

/// Max concurrent restores (saturated = busy followup).
pub(crate) const RESTORE_MAX_CONCURRENT: usize = 6;

/// Per-guild restore-submit cooldown: bounds CSV download + WAL REPLACE spam.
const RESTORE_COOLDOWN_SECS: u64 = 10;

/// Guild-only gate message for `/data restore`.
pub(crate) const RESTORE_GUILD_ONLY_MSG: &str = "Use `/data restore` inside a server.";
/// Bot-grant gate message for `/data restore`.
pub(crate) const RESTORE_NEEDS_MANAGE_MSG: &str =
    "I need the Manage Roles permission (or Administrator) to restore sticky data.";

static RESTORE_LAST: OnceLock<super::CooldownMap> = OnceLock::new();

pub(crate) fn restore_last() -> &'static super::CooldownMap {
    RESTORE_LAST.get_or_init(|| super::CooldownMap::new(rustc_hash::FxHashMap::default()))
}

static RESTORE_SEMAPHORE: OnceLock<Arc<Semaphore>> = OnceLock::new();

fn restore_semaphore() -> Arc<Semaphore> {
    RESTORE_SEMAPHORE
        .get_or_init(|| Arc::new(Semaphore::new(RESTORE_MAX_CONCURRENT)))
        .clone()
}

type CdnConnector =
    hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>;
type CdnClient =
    hyper_util::client::legacy::Client<CdnConnector, http_body_util::Full<hyper::body::Bytes>>;

static CDN_CLIENT: OnceLock<CdnClient> = OnceLock::new();

/// Shared CDN client (platform roots, https-only).
fn cdn_client() -> CdnClient {
    CDN_CLIENT
        .get_or_init(|| {
            let mut http = hyper_util::client::legacy::connect::HttpConnector::new();
            http.enforce_http(false);
            let tls = {
                use hyper_rustls::ConfigBuilderExt;
                rustls::ClientConfig::builder()
                    .try_with_platform_verifier()
                    .expect("no usable cipher suites in crypto provider")
                    .with_no_client_auth()
            };
            let https = hyper_rustls::HttpsConnectorBuilder::new()
                .with_tls_config(tls)
                .https_only()
                .enable_http1()
                .enable_http2()
                .wrap_connector(http);
            hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
                .build(https)
        })
        .clone()
}

/// Export this guild's sticky map to CSV (always a file; never inline).
pub(crate) async fn handle_data_save(interaction: &Interaction) {
    let Some(guild_id) =
        super::require_guild(interaction, "Use `/data save` inside a server.").await
    else {
        return;
    };
    if !super::check_invoker(interaction, guild_id).await {
        return;
    }

    let (csv, entries) = {
        let guard = registry::stores().read().await;
        match guard.get(&guild_id) {
            Some(store) => (export_guild(store), store.map.len()),
            None => (csv::export_csv(&[]), 0),
        }
    };

    // Always file so the content cap can never truncate a backup.
    let entry_word = if entries == 1 { "entry" } else { "entries" };
    let data = InteractionResponseData {
        content: Some(format!(
            "Sticky backup for this server: {entries} {entry_word} attached."
        )),
        attachments: Some(vec![Attachment {
            description: Some("Sticky-roles CSV backup".to_owned()),
            file: csv.into_bytes(),
            filename: format!("sticky-roles-{}.csv", guild_id.get()),
            id: 0,
            duration_secs: None,
            is_spoiler: None,
            title: None,
            waveform: None,
        }]),
        flags: Some(MessageFlags::EPHEMERAL),
        allowed_mentions: Some(AllowedMentions::default()),
        ..Default::default()
    };
    let response = InteractionResponse {
        kind: InteractionResponseType::ChannelMessageWithSource,
        data: Some(data),
    };
    if let Err(e) = crate::CTX
        .interaction()
        .create_response(interaction.id, &interaction.token, &response)
        .await
    {
        super::messages::warn_http(
            &e,
            "failed to send /data save response; user got no file, old data kept",
        );
    }
}

/// Re-exported store cap (canonical: [`crate::store::csv::MAX_CSV_BYTES`]).
pub(crate) use crate::store::csv::MAX_CSV_BYTES;

/// Allowed CSV content types (case-insensitive, charset suffix stripped).
pub(crate) const ALLOWED_CSV_CONTENT_TYPES: &[&str] = &[
    "text/csv",
    "application/csv",
    "text/plain",
    "text/x-csv",
    "application/vnd.ms-excel",
];

/// Schema block shown above the restore upload.
pub(crate) const RESTORE_SCHEMA_TEXT: &str = "Upload a CSV backup exported by `/data save`. Overwrites current saved data with new data.\n```csv\nguild_id,user_id,roles,nickname,left_at\n111111111111111111,222222222222222222,444444444444444444;555555555555555555,Ash,1700000000\n111111111111111111,333333333333333333,,\"Quoted, \"\"Nick\"\"\",1700000001\n```";

/// Open restore modal: TextDisplay schema + csvfile FileUpload.
pub(crate) async fn handle_data_restore(interaction: &Interaction) {
    let Some(guild_id) = super::require_guild(interaction, RESTORE_GUILD_ONLY_MSG).await else {
        return;
    };
    if super::gate_full(interaction, guild_id, RESTORE_NEEDS_MANAGE_MSG)
        .await
        .is_none()
    {
        return;
    };

    let schema = TextDisplayBuilder::new(RESTORE_SCHEMA_TEXT).build();
    let upload = FileUploadBuilder::new(CUSTOM_ID_DATA_CSV_FILE)
        .required(true)
        .min_values(1)
        .max_values(1)
        .file_types([".csv"])
        .build();
    let label = LabelBuilder::new("CSV backup file", Component::FileUpload(upload))
        .description("Upload a CSV backup from `/data save`.")
        .build();
    super::open_modal(
        interaction,
        "Restore sticky data",
        CUSTOM_ID_DATA_RESTORE_MODAL,
        vec![Component::TextDisplay(schema), Component::Label(label)],
        "Couldn't open data restore. Please try again.",
    )
    .await;
}

async fn gate_restore<'a>(
    interaction: &Interaction,
    data: &'a ModalInteractionData,
) -> Option<(Id<GuildMarker>, &'a str)> {
    let guild_id = super::require_guild(interaction, RESTORE_GUILD_ONLY_MSG).await?;
    // Full gate so a bot-impotent import cannot wipe data it could never re-grant.
    super::gate_full(interaction, guild_id, RESTORE_NEEDS_MANAGE_MSG).await?;
    // Peek early so hot guilds get "Slow down" without burning the budget.
    if !super::cooldown_peek_at(
        restore_last(),
        guild_id.get(),
        RESTORE_COOLDOWN_SECS,
        std::time::Instant::now(),
    ) {
        super::respond_ephemeral(
            interaction,
            "Slow down — a restore just ran here. Wait a few seconds and try again.".to_owned(),
        )
        .await;
        return None;
    }
    let Some(attachment_id) = extract_csv_attachment_id(data) else {
        super::respond_ephemeral(
            interaction,
            "No CSV file was attached; please upload one `.csv` backup and try again.".to_owned(),
        )
        .await;
        return None;
    };
    let Some(attachment) = data
        .resolved
        .as_ref()
        .and_then(|r| r.attachments.get(&attachment_id))
    else {
        super::respond_ephemeral(
            interaction,
            "I couldn't read the uploaded file; please try again.".to_owned(),
        )
        .await;
        return None;
    };
    if let Err(reject) = validate_csv_attachment(attachment) {
        super::respond_ephemeral(interaction, reject.to_string()).await;
        return None;
    }
    // Recorded only after a successful deferred ack, so early failures never burn it.
    Some((guild_id, &attachment.url))
}

/// True when the restore would grant roles at/above the invoker's top.
/// Only configured roles can be granted; owner bypasses; unknown roles fail open.
pub(crate) fn restore_denied(
    active: &[Id<RoleMarker>],
    positions: &rustc_hash::FxHashMap<Id<RoleMarker>, i16>,
    gate: &super::Gate,
) -> bool {
    if gate.is_owner || active.is_empty() {
        return false;
    }
    let Some(max) = active.iter().filter_map(|r| positions.get(r)).max() else {
        return false;
    };
    gate.invoker_top <= *max
}

/// Download + size checks. Err text is the followup message.
async fn fetch_csv(url: &str) -> Result<Vec<u8>, String> {
    let bytes = download_csv_bytes(url).await.map_err(|fail| {
        tracing::warn!(
            reason = %fail.kind,
            status = fail.status.unwrap_or(0),
            bytes = fail.bytes.unwrap_or(0),
            "restore download failed; old data kept, bad_rows=0"
        );
        "I couldn't download the attached file; please upload it again.".to_owned()
    })?;
    if bytes.is_empty() {
        return Err(CsvReject::Empty.to_string());
    }
    if bytes.len() as u64 > MAX_CSV_BYTES {
        return Err(CsvReject::TooBig.to_string());
    }
    Ok(bytes)
}

/// Live restore snapshot (single cache read). `None` fail-closes (no import).
async fn snapshot_restore_live(
    guild_id: Id<GuildMarker>,
    active: &[Id<RoleMarker>],
) -> Option<(
    rustc_hash::FxHashSet<u64>,
    rustc_hash::FxHashMap<Id<RoleMarker>, i16>,
)> {
    let guard = crate::events::cache().read().await;
    let g = guard.guilds.get(&guild_id)?;
    let mut member_set = rustc_hash::FxHashSet::default();
    member_set.extend(g.members.keys().map(|u| u.get()));
    let mut positions = rustc_hash::FxHashMap::default();
    for rid in active {
        if let Some(r) = g.roles.get(rid) {
            positions.insert(*rid, r.position);
        }
    }
    Some((member_set, positions))
}

fn reject_message(kind: &str) -> &'static str {
    if matches!(kind, "guild-mismatch" | "store-guild-mismatch") {
        "That backup is from a different server; nothing was changed. Upload a backup from this server."
    } else if matches!(kind, "missing-header" | "bad-header") {
        "That file doesn't look like a backup from `/data save`; nothing was changed. The first line should be `guild_id,user_id,roles,nickname,left_at`."
    } else {
        "I couldn't restore that file; nothing was changed. Upload a fresh backup from `/data save`."
    }
}

/// Validate, re-gate, and import. Returns the followup text for every path.
async fn apply_restore(
    interaction: &Interaction,
    guild_id: Id<GuildMarker>,
    bytes: Vec<u8>,
) -> String {
    let text = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(_) => {
            return "That file isn't readable as text. Please upload a CSV backup from `/data save`."
                .to_owned();
        }
    };
    if text.trim().is_empty() {
        return "The uploaded CSV is empty.".to_owned();
    }
    let expiry_secs = registry::with_guild(guild_id, |s| s.config.expiry_secs)
        .await
        .unwrap_or(DEFAULT_EXPIRY_SECS);
    let active: Vec<Id<RoleMarker>> = registry::with_guild(guild_id, |s| s.config.role_ids())
        .await
        .unwrap_or_default();
    let Some((member_set, positions)) = snapshot_restore_live(guild_id, &active).await else {
        return super::STARTUP_MSG.to_owned();
    };
    // Live re-gate: privs revoked during download must not wipe via import.
    let Some(gate) = super::read_gate(interaction, guild_id).await else {
        return super::STARTUP_MSG.to_owned();
    };
    if !gate.invoker_granted() {
        return "You need the Manage Roles and Manage Guild permissions to use this.".to_owned();
    }
    if !gate.bot_granted() {
        return RESTORE_NEEDS_MANAGE_MSG.to_owned();
    }
    if restore_denied(&active, &positions, &gate) {
        tracing::warn!(
            reason = "restore-hierarchy-deny",
            "restore position rejected; old data kept"
        );
        return "Couldn't restore that backup — it contains roles above your top role, so you can't restore it."
            .to_owned();
    }
    // No `ensure_guild` before import: a rejected restore must leave no store behind.
    match persist::append_import_replace(guild_id, &text, expiry_secs, |uid| {
        member_set.contains(&uid.get())
    })
    .await
    {
        Err(e) => {
            tracing::warn!(
                reason = e.kind_str(),
                bad_rows = e.bad_rows(),
                "restore rejected; old data kept, skipped-count=bad_rows"
            );
            reject_message(e.kind_str()).to_owned()
        }
        Ok(report) => {
            let remembered = registry::with_guild(guild_id, |s| s.map.len())
                .await
                .unwrap_or(0);
            super::messages::format_restore_complete(
                remembered,
                report.skipped_invalid,
                report.skipped_expired,
                report.skipped_truncated,
                report.skipped_current_members,
                report.skipped_untracked_roles,
                report.skipped_nicknames,
            )
        }
    }
}

/// Validate upload, download, then `import_replace` (deferred ack first).
pub(crate) async fn handle_restore_submit(interaction: &Interaction, data: &ModalInteractionData) {
    let Some((guild_id, url)) = gate_restore(interaction, data).await else {
        return;
    };
    if super::send_deferred(interaction, true).await.is_err() {
        return;
    }
    let _permit = match restore_semaphore().try_acquire_owned() {
        Ok(p) => p,
        Err(_) => {
            super::edit_followup(
                interaction,
                "Too many restores are running right now; please try again shortly.",
                true,
            )
            .await;
            return;
        }
    };
    let bytes = match fetch_csv(url).await {
        Ok(bytes) => bytes,
        Err(msg) => {
            super::edit_followup(interaction, &msg, true).await;
            return;
        }
    };
    // Burn the budget only for work actually done: failed downloads and busy
    // lanes cost nothing, matching sticky's never-burn-on-validation rule.
    if !super::cooldown_check_at(
        restore_last(),
        guild_id.get(),
        RESTORE_COOLDOWN_SECS,
        std::time::Instant::now(),
    ) {
        super::edit_followup(
            interaction,
            "Slow down — a restore just ran here. Wait a few seconds and try again.",
            true,
        )
        .await;
        return;
    }
    let msg = apply_restore(interaction, guild_id, bytes).await;
    super::edit_followup(interaction, &msg, true).await;
}

/// CDN hosts allowed for CSV fetch (checked before any socket; no redirects).
pub(crate) const ALLOWED_CDN_HOSTS: &[&str] = &["cdn.discordapp.com", "media.discordapp.net"];

/// Total fetch budget for the hyper GET + body stream (per attempt).
const CDN_FETCH_TIMEOUT_SECS: u64 = 10;

/// Delay before the single idempotent CDN retry (transient failures only).
const CDN_RETRY_DELAY_MS: u64 = 250;

/// Max URL length (8 KiB).
const MAX_URL_LEN: usize = 8 * 1024;

/// Machine-kind for a failed CSV download (server logs carry kind only).
#[derive(Error, Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DownloadKind {
    #[error("bad-url")]
    BadUrl,
    #[error("request-failed")]
    RequestFailed,
    #[error("redirect")]
    Redirect,
    #[error("http-status")]
    HttpStatus,
    #[error("bad-encoding")]
    BadEncoding,
    #[error("bad-content-length")]
    BadContentLength,
    #[error("body-too-large")]
    BodyTooLarge,
    #[error("read-failed")]
    ReadFailed,
    #[error("timeout")]
    Timeout,
}

/// Download failure: kind + counts only (never URLs or body bytes).
#[derive(Error, Debug, Clone, Copy, PartialEq, Eq)]
#[error("{kind}")]
pub(crate) struct DownloadFail {
    #[source]
    pub kind: DownloadKind,
    pub status: Option<u16>,
    pub bytes: Option<u64>,
}

impl DownloadFail {
    fn new(kind: DownloadKind) -> Self {
        Self {
            kind,
            status: None,
            bytes: None,
        }
    }

    fn with_status(kind: DownloadKind, status: u16) -> Self {
        Self {
            kind,
            status: Some(status),
            bytes: None,
        }
    }

    fn with_bytes(kind: DownloadKind, bytes: u64) -> Self {
        Self {
            kind,
            status: None,
            bytes: Some(bytes),
        }
    }
}

/// Retry once on transient failures (Timeout, ReadFailed, 5xx). Old data kept either way.
fn should_retry_cdn_download(fail: &DownloadFail) -> bool {
    match fail.kind {
        DownloadKind::Timeout | DownloadKind::ReadFailed => true,
        DownloadKind::HttpStatus => fail.status.is_some_and(|s| (500..=599).contains(&s)),
        _ => false,
    }
}

/// Header-check phase of the CDN GET. Returns the body reserve on success.
fn check_cdn_response(
    status: http::StatusCode,
    headers: &http::HeaderMap,
) -> Result<usize, DownloadFail> {
    if status.is_redirection() {
        return Err(DownloadFail::with_status(
            DownloadKind::Redirect,
            status.as_u16(),
        ));
    }
    if status != http::StatusCode::OK {
        return Err(DownloadFail::with_status(
            DownloadKind::HttpStatus,
            status.as_u16(),
        ));
    }
    if let Some(enc) = headers.get(http::header::CONTENT_ENCODING) {
        let v = enc.to_str().unwrap_or("").trim().to_ascii_lowercase();
        if !(v.is_empty() || v == "identity") {
            return Err(DownloadFail::new(DownloadKind::BadEncoding));
        }
    }
    let content_len: Option<u64> = match headers.get(http::header::CONTENT_LENGTH) {
        None => None,
        Some(v) => Some(
            v.to_str()
                .unwrap_or("")
                .trim()
                .parse()
                .map_err(|_| DownloadFail::new(DownloadKind::BadContentLength))?,
        ),
    };
    if let Some(len) = content_len
        && len > MAX_CSV_BYTES
    {
        return Err(DownloadFail::with_bytes(DownloadKind::BodyTooLarge, len));
    }
    Ok(content_len
        .map(|len| len.min(MAX_CSV_BYTES) as usize)
        .unwrap_or(8 * 1024))
}

/// Single bounded CDN GET attempt (caller wraps with timeout/retry budget).
async fn fetch_cdn_once(uri: http::Uri) -> Result<Vec<u8>, DownloadFail> {
    let req = http::Request::builder()
        .method(http::Method::GET)
        .uri(uri)
        .header(http::header::USER_AGENT, "sticky-roles data-restore")
        .header(http::header::ACCEPT, "text/csv,*/*;q=0.5")
        .body(http_body_util::Full::new(hyper::body::Bytes::new()))
        .map_err(|_| DownloadFail::new(DownloadKind::RequestFailed))?;
    let resp = cdn_client()
        .request(req)
        .await
        .map_err(|_| DownloadFail::new(DownloadKind::RequestFailed))?;

    let (parts, body) = resp.into_parts();
    let reserve = check_cdn_response(parts.status, &parts.headers)?;

    use http_body_util::BodyExt;
    let mut body = body;
    let mut out: Vec<u8> = Vec::with_capacity(reserve);
    while let Some(frame) = body
        .frame()
        .await
        .transpose()
        .map_err(|_| DownloadFail::new(DownloadKind::ReadFailed))?
    {
        if let Some(data) = frame.data_ref() {
            if out.len() as u64 + data.len() as u64 > MAX_CSV_BYTES {
                return Err(DownloadFail::with_bytes(
                    DownloadKind::BodyTooLarge,
                    (out.len() + data.len()) as u64,
                ));
            }
            out.extend_from_slice(data);
        }
    }
    Ok(out)
}

/// Bounded CDN GET with one retry on transient failures.
async fn download_csv_bytes(url: &str) -> Result<Vec<u8>, DownloadFail> {
    // Map the validator to a bare kind so server logs never carry URL parts.
    validate_cdn_url(url).map_err(|_| DownloadFail::new(DownloadKind::BadUrl))?;

    for attempt in 0..=1u32 {
        let uri: http::Uri = url
            .parse()
            .map_err(|_| DownloadFail::new(DownloadKind::BadUrl))?;
        let res = tokio::time::timeout(
            std::time::Duration::from_secs(CDN_FETCH_TIMEOUT_SECS),
            fetch_cdn_once(uri),
        )
        .await
        .map_err(|_| DownloadFail::new(DownloadKind::Timeout))?;
        match res {
            Ok(bytes) => return Ok(bytes),
            Err(fail) if attempt == 0 && should_retry_cdn_download(&fail) => {
                tokio::time::sleep(std::time::Duration::from_millis(CDN_RETRY_DELAY_MS)).await;
            }
            Err(fail) => return Err(fail),
        }
    }
    // Unreachable (loop always returns).
    Err(DownloadFail::new(DownloadKind::RequestFailed))
}

/// Typed reason for a rejected CDN URL.
#[derive(Error, Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CdnUrlReason {
    #[error("attachment URL is too long")]
    TooLong,
    #[error("attachment URL must use https")]
    MustUseHttps,
    #[error("attachment URL is missing its host")]
    MissingHost,
    #[error("attachment URL must not embed credentials")]
    EmbedsCredentials,
    #[error("attachment URL uses an unexpected port")]
    UnexpectedPort,
    #[error("unexpected CDN host")]
    UnexpectedHost,
    #[error("attachment URL has a bad path")]
    BadPath,
}

/// Borrow-only CDN allowlist check, zero allocs.
fn validate_cdn_url(url: &str) -> Result<(), CdnUrlReason> {
    if url.len() > MAX_URL_LEN {
        return Err(CdnUrlReason::TooLong);
    }
    let rest = url
        .strip_prefix("https://")
        .ok_or(CdnUrlReason::MustUseHttps)?;
    if rest.is_empty() {
        return Err(CdnUrlReason::MissingHost);
    }
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    if authority.is_empty() {
        return Err(CdnUrlReason::MissingHost);
    }
    if authority.contains('@') {
        return Err(CdnUrlReason::EmbedsCredentials);
    }
    let (host, port) = match authority.rfind(':') {
        Some(i) if authority[i + 1..].chars().all(|c| c.is_ascii_digit()) => {
            (&authority[..i], Some(&authority[i + 1..]))
        }
        _ => (authority, None),
    };
    if host.is_empty() {
        return Err(CdnUrlReason::MissingHost);
    }
    if let Some(port) = port
        && port != "443"
    {
        return Err(CdnUrlReason::UnexpectedPort);
    }
    if !ALLOWED_CDN_HOSTS
        .iter()
        .any(|h| h.eq_ignore_ascii_case(host))
    {
        return Err(CdnUrlReason::UnexpectedHost);
    }
    if !path.starts_with('/') {
        return Err(CdnUrlReason::BadPath);
    }
    if path
        .bytes()
        .any(|b| b == b' ' || b == b'\t' || b == b'\r' || b == b'\n')
    {
        return Err(CdnUrlReason::BadPath);
    }
    Ok(())
}

/// Upload attachment id from the csvfile FileUpload.
fn extract_csv_attachment_id(data: &ModalInteractionData) -> Option<Id<AttachmentMarker>> {
    fn visit(comp: &ModalInteractionComponent) -> Option<Id<AttachmentMarker>> {
        match comp {
            ModalInteractionComponent::FileUpload(fu) => {
                if fu.custom_id == CUSTOM_ID_DATA_CSV_FILE {
                    fu.values.first().copied()
                } else {
                    None
                }
            }
            ModalInteractionComponent::Label(label) => visit(&label.component),
            ModalInteractionComponent::ActionRow(row) => row.components.iter().find_map(visit),
            _ => None,
        }
    }
    data.components.iter().find_map(visit)
}

/// Typed reason for a rejected CSV attachment (user text is the [`Display`] impl).
#[derive(Error, Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CsvReject {
    #[error("That file doesn't look like a CSV backup; please upload a `.csv` file.")]
    NotCsv,
    #[error("That file is empty.")]
    Empty,
    #[error("That file is too big; backups must be under 2 MB.")]
    TooBig,
}

/// Metadata gate: .csv extension, allowlisted type, 2 MiB max.
fn validate_csv_attachment(attachment: &ResolvedAttachment) -> Result<(), CsvReject> {
    if !attachment.filename.to_ascii_lowercase().ends_with(".csv") {
        return Err(CsvReject::NotCsv);
    }
    let content_type = attachment
        .content_type
        .as_deref()
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim();
    if !ALLOWED_CSV_CONTENT_TYPES
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(content_type))
    {
        return Err(CsvReject::NotCsv);
    }
    if attachment.size == 0 {
        return Err(CsvReject::Empty);
    }
    if attachment.size > MAX_CSV_BYTES {
        return Err(CsvReject::TooBig);
    }
    Ok(())
}
