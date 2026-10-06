use twilight_model::id::{
    Id,
    marker::{GuildMarker, RoleMarker, UserMarker},
};

use crate::store::GuildStore;

pub const CSV_HEADER: &str = "guild_id,user_id,roles,nickname,left_at";
pub const MAX_RESTORE_ROWS: usize = 25_000;
pub const FUTURE_SKEW_SECS: u32 = 300;
pub(crate) const MAX_CSV_BYTES: u64 = 2_000_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StickyRow {
    pub guild_id: Id<GuildMarker>,
    pub user_id: Id<UserMarker>,
    pub roles: Vec<Id<RoleMarker>>,
    pub nickname: Option<Box<str>>,
    pub left_at: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportReport {
    pub rows: Vec<StickyRow>,
    pub skipped_invalid: usize,
    pub skipped_expired: usize,
    pub skipped_truncated: usize,
    pub skipped_current_members: usize,
    pub skipped_untracked_roles: usize,
    pub skipped_nicknames: usize,
}

#[derive(thiserror::Error, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportRejectKind {
    #[error("missing-header")]
    MissingHeader,
    #[error("bad-header")]
    BadHeader,
    #[error("guild-mismatch")]
    GuildMismatch,
    #[error("store-guild-mismatch")]
    StoreMismatch,
    #[error("body-too-large")]
    BodyTooLarge,
}

impl ImportRejectKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MissingHeader => "missing-header",
            Self::BadHeader => "bad-header",
            Self::GuildMismatch => "guild-mismatch",
            Self::StoreMismatch => "store-guild-mismatch",
            Self::BodyTooLarge => "body-too-large",
        }
    }
}

#[derive(thiserror::Error, Debug, Clone, Copy, PartialEq, Eq)]
#[error("{kind} (bad_rows={bad_rows})")]
pub struct ImportReject {
    #[source]
    pub kind: ImportRejectKind,
    pub bad_rows: usize,
}

impl ImportReject {
    pub fn new(kind: ImportRejectKind, bad_rows: usize) -> Self {
        Self { kind, bad_rows }
    }

    pub fn kind_str(self) -> &'static str {
        self.kind.as_str()
    }
}

pub(crate) fn export_csv(rows: &[StickyRow]) -> String {
    let mut w = csv::WriterBuilder::new()
        .has_headers(false)
        .terminator(csv::Terminator::Any(b'\n'))
        .from_writer(Vec::new());
    w.write_record(CSV_HEADER.split(','))
        .expect("csv header write infallible");
    for row in rows {
        let roles = row
            .roles
            .iter()
            .map(|r| r.get().to_string())
            .collect::<Vec<_>>()
            .join(";");
        w.write_record([
            row.guild_id.get().to_string(),
            row.user_id.get().to_string(),
            roles,
            row.nickname.as_deref().unwrap_or("").to_owned(),
            row.left_at.to_string(),
        ])
        .expect("csv row write infallible");
    }
    String::from_utf8(w.into_inner().expect("csv flush infallible")).expect("csv output utf-8")
}

pub(crate) fn parse_import(
    csv_text: &str,
    expected_guild: Id<GuildMarker>,
    now_secs: u32,
) -> Result<ImportReport, ImportReject> {
    if csv_text.len() as u64 > MAX_CSV_BYTES {
        return Err(ImportReject::new(ImportRejectKind::BodyTooLarge, 0));
    }
    let text = csv_text.strip_prefix('\u{FEFF}').unwrap_or(csv_text);
    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(true)
        .flexible(true)
        .from_reader(text.as_bytes());
    let headers = rdr
        .headers()
        .map_err(|_| ImportReject::new(ImportRejectKind::MissingHeader, 0))?
        .clone();
    let expect = ["guild_id", "user_id", "roles", "nickname", "left_at"];
    if headers.len() != 5 || headers.iter().zip(expect).any(|(a, b)| a != b) {
        return Err(ImportReject::new(ImportRejectKind::BadHeader, 0));
    }
    let mut report = ImportReport::default();
    for res in rdr.records() {
        let rec = match res {
            Ok(r) => r,
            Err(_) => {
                report.skipped_invalid += 1;
                continue;
            }
        };
        if rec.len() != 5 {
            report.skipped_invalid += 1;
            continue;
        }
        let guild_raw: u64 = match rec[0].parse() {
            Ok(n) => n,
            Err(_) => {
                report.skipped_invalid += 1;
                continue;
            }
        };
        let Some(guild_id) = Id::new_checked(guild_raw) else {
            report.skipped_invalid += 1;
            continue;
        };
        if guild_id != expected_guild {
            let bad_rows = report.skipped_invalid + report.skipped_expired + 1;
            return Err(ImportReject::new(ImportRejectKind::GuildMismatch, bad_rows));
        }
        let user_raw: u64 = match rec[1].parse() {
            Ok(n) => n,
            Err(_) => {
                report.skipped_invalid += 1;
                continue;
            }
        };
        let Some(user_id) = Id::new_checked(user_raw) else {
            report.skipped_invalid += 1;
            continue;
        };
        let mut roles: Vec<Id<RoleMarker>> = Vec::new();
        let mut bad_role = false;
        if !rec[2].is_empty() {
            for part in rec[2].split(';') {
                match part.parse::<u64>() {
                    // Snowflake 0 is never valid; silently dropping it would
                    // turn "11;0;22" into [11,22] instead of rejecting the row.
                    Ok(0) => {
                        bad_role = true;
                        break;
                    }
                    Ok(n) => {
                        if let Some(id) = Id::new_checked(n) {
                            roles.push(id);
                        }
                    }
                    Err(_) => {
                        bad_role = true;
                        break;
                    }
                }
            }
        }
        if bad_role {
            report.skipped_invalid += 1;
            continue;
        }
        let nickname = if rec[3].is_empty() {
            None
        } else {
            let t = GuildStore::truncate_nick(&rec[3]);
            if t.is_empty() { None } else { Some(t.into()) }
        };
        let left_at: u32 = match rec[4].parse() {
            Ok(n) => n,
            Err(_) => {
                report.skipped_invalid += 1;
                continue;
            }
        };
        if left_at == 0 {
            report.skipped_expired += 1;
            continue;
        }
        if (left_at as u64) > (now_secs as u64) + (FUTURE_SKEW_SECS as u64) {
            report.skipped_invalid += 1;
            continue;
        }
        report.rows.push(StickyRow {
            guild_id,
            user_id,
            roles,
            nickname,
            left_at,
        });
    }
    report.rows.sort_by_key(|r| (r.left_at, r.user_id.get()));
    // Dedupe users, keeping the newest row (max left_at). import_replace
    // inserts in sorted order so last-wins, but its truncation binary_search
    // assumes unique user_ids.
    if report.rows.len() > 1 {
        let mut seen = std::collections::HashSet::with_capacity(report.rows.len());
        let mut uniq: Vec<StickyRow> = Vec::with_capacity(report.rows.len());
        for row in report.rows.drain(..).rev() {
            if seen.insert(row.user_id) {
                uniq.push(row);
            } else {
                report.skipped_invalid += 1;
            }
        }
        uniq.reverse();
        // Restore (left_at, user_id) order after the rev/dedupe round-trip.
        uniq.sort_by_key(|r| (r.left_at, r.user_id.get()));
        report.rows = uniq;
    }
    Ok(report)
}
