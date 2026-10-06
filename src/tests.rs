use twilight_model::id::{
    Id,
    marker::{GuildMarker, RoleMarker, UserMarker},
};

use crate::store::{GuildConfig, GuildStore, StickyEntry};

fn rid(id: u64) -> Id<RoleMarker> {
    Id::new(id)
}

fn uid(id: u64) -> Id<UserMarker> {
    Id::new(id)
}

fn gid(id: u64) -> Id<GuildMarker> {
    Id::new(id)
}

/// Serializes tests that touch globals (registry stores, event cache, DATA_DIR).
static IO_LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();

fn io_lock() -> std::sync::MutexGuard<'static, ()> {
    IO_LOCK
        .get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn member_user(id: u64, bot: bool) -> twilight_model::user::User {
    twilight_model::user::User {
        accent_color: None,
        avatar: None,
        avatar_decoration: None,
        avatar_decoration_data: None,
        banner: None,
        bot,
        discriminator: 0,
        email: None,
        flags: None,
        global_name: None,
        id: Id::new(id),
        locale: None,
        mfa_enabled: None,
        name: format!("user{id}"),
        premium_type: None,
        primary_guild: None,
        public_flags: None,
        system: None,
        verified: None,
    }
}

fn ban_entry(
    g: Id<GuildMarker>,
    u: Id<UserMarker>,
) -> twilight_model::gateway::payload::incoming::GuildAuditLogEntryCreate {
    use twilight_model::guild::audit_log::{AuditLogEntry, AuditLogEventType};
    use twilight_model::id::marker::GenericMarker;
    twilight_model::gateway::payload::incoming::GuildAuditLogEntryCreate(AuditLogEntry {
        action_type: AuditLogEventType::MemberBanAdd,
        changes: Vec::new(),
        guild_id: Some(g),
        id: Id::new(1),
        options: None,
        reason: None,
        target_id: Some(Id::<GenericMarker>::new(u.get())),
        user_id: None,
    })
}

#[test]
fn hierarchy_grant_and_position() {
    use crate::hierarchy::{Actor, Deny, Target, can_assign};
    use twilight_model::guild::Permissions;

    let bot = Actor {
        top: 5,
        perms: Permissions::MANAGE_ROLES,
        is_owner: false,
    };
    assert_eq!(
        can_assign(
            &bot,
            &Target {
                is_everyone: false,
                pos: 4,
                managed: false
            }
        ),
        Ok(())
    );
    assert_eq!(
        can_assign(
            &bot,
            &Target {
                is_everyone: false,
                pos: 5,
                managed: false
            }
        ),
        Err(Deny::Hierarchy)
    );
    assert_eq!(
        can_assign(
            &bot,
            &Target {
                is_everyone: false,
                pos: 1,
                managed: true
            }
        ),
        Err(Deny::Managed)
    );
    let grantless = Actor {
        top: 99,
        perms: Permissions::empty(),
        is_owner: false,
    };
    assert_eq!(
        can_assign(
            &grantless,
            &Target {
                is_everyone: false,
                pos: 1,
                managed: false
            }
        ),
        Err(Deny::NoGrant)
    );
    assert_eq!(
        can_assign(
            &bot,
            &Target {
                is_everyone: true,
                pos: 1,
                managed: false
            }
        ),
        Err(Deny::Everyone)
    );
    let owner = Actor {
        top: 0,
        perms: Permissions::empty(),
        is_owner: true,
    };
    assert_eq!(
        can_assign(
            &owner,
            &Target {
                is_everyone: false,
                pos: 99,
                managed: false
            }
        ),
        Ok(())
    );
}

#[test]
fn config_bits_round_trip() {
    let cfg = GuildConfig::from_ids_dedup(&[rid(11), rid(22), rid(33)]);
    assert_eq!(cfg.mask(), 0b111);
    let mut out = Vec::new();
    cfg.decode_into(0b101, &mut out);
    assert_eq!(out, vec![rid(11), rid(33)]);

    let mut skipped = cfg.clone();
    skipped.skipped_mask = 0b001;
    skipped.decode_into(0b011, &mut out);
    assert_eq!(out, vec![rid(22)]);
}

#[test]
fn snapshot_round_trip() {
    use crate::store::snapshot::{SnapshotEntry, decode_snapshot, encode_snapshot};

    assert!(decode_snapshot(b"nope").is_err());
    assert!(decode_snapshot(&[]).is_err());

    let mut store = GuildStore::new(gid(42), GuildConfig::from_ids_dedup(&[rid(11), rid(22)]));
    store.config.save_nicknames = true;
    store.map.insert(
        uid(7),
        StickyEntry {
            bits: 0b11,
            left_at: 2000,
        },
    );
    store.set_nick(uid(7), Some("Ash".into()));
    let bytes = encode_snapshot(&[SnapshotEntry {
        guild_id: gid(42),
        deleted_at: 0,
        store,
    }]);
    let decoded = decode_snapshot(&bytes).unwrap();
    assert_eq!(decoded.len(), 1);
    let (store, deleted_at) = &decoded[0];
    assert_eq!(*deleted_at, 0);
    assert_eq!(store.map[&uid(7)].bits, 0b11);
    assert_eq!(store.nick_for(uid(7)), Some("Ash"));
    let mut out = Vec::new();
    assert_eq!(
        store.restore_into(uid(7), 2500, &mut out),
        Some(Some("Ash"))
    );
    assert_eq!(out, vec![rid(11), rid(22)]);
}

#[test]
fn wal_replay_round_trip() {
    use crate::store::wal;
    use rustc_hash::FxHashMap;

    // Ordered multi-op replay: config, upsert, delete.
    let mut stores: FxHashMap<Id<GuildMarker>, GuildStore> = FxHashMap::default();
    let mut tombs: FxHashMap<Id<GuildMarker>, u64> = FxHashMap::default();

    let mut cfg_payload = Vec::new();
    let cfg = GuildConfig::from_ids_dedup(&[rid(11), rid(22)]);
    wal::encode_config_payload(&mut cfg_payload, gid(42), &cfg);
    let mut bytes = wal::encode_frame(wal::OP_CONFIG, &cfg_payload);

    let mut up = Vec::new();
    wal::encode_upsert_payload(
        &mut up,
        gid(42),
        uid(9),
        &StickyEntry {
            bits: 0b01,
            left_at: 1000,
        },
        Some("Bo"),
    );
    bytes.extend_from_slice(&wal::encode_frame(wal::OP_UPSERT, &up));

    let mut del = Vec::new();
    wal::encode_delete_payload(&mut del, gid(42), uid(9));
    bytes.extend_from_slice(&wal::encode_frame(wal::OP_DELETE, &del));

    let out = wal::replay_wal(&bytes, &mut stores, &mut tombs);
    assert!(out.corrupt_at.is_none());
    assert_eq!(out.stats.ops_applied, 3);
    assert!(!stores[&gid(42)].map.contains_key(&uid(9)));

    // Torn tail keeps the prefix.
    let mut torn = bytes.clone();
    torn.push(0xFF);
    let out = wal::replay_wal(&torn, &mut stores, &mut tombs);
    assert!(out.stats.torn_tail);

    // Corrupt frame truncates there.
    let mut corrupt = Vec::new();
    let mut up2 = Vec::new();
    wal::encode_upsert_payload(
        &mut up2,
        gid(42),
        uid(1),
        &StickyEntry {
            bits: 0b01,
            left_at: 1000,
        },
        None,
    );
    corrupt.extend_from_slice(&wal::encode_frame(wal::OP_UPSERT, &up2));
    corrupt.extend_from_slice(&[9, 9, 9]);
    let mut stores2 = FxHashMap::default();
    let mut tombs2 = FxHashMap::default();
    let out = wal::replay_wal(&corrupt, &mut stores2, &mut tombs2);
    assert!(out.corrupt_at.is_some() || out.stats.torn_tail);

    // Trailing payload bytes (newer field append) still apply.
    let mut up3 = Vec::new();
    wal::encode_upsert_payload(
        &mut up3,
        gid(42),
        uid(1),
        &StickyEntry {
            bits: 0b01,
            left_at: 1000,
        },
        None,
    );
    up3.extend_from_slice(&[0xAA, 0xBB]);
    let trailed = wal::encode_frame(wal::OP_UPSERT, &up3);
    let mut stores3: FxHashMap<Id<GuildMarker>, GuildStore> = FxHashMap::default();
    let mut tombs3: FxHashMap<Id<GuildMarker>, u64> = FxHashMap::default();
    let cfg3 = GuildConfig::from_ids_dedup(&[rid(11)]);
    stores3.insert(gid(42), GuildStore::new(gid(42), cfg3));
    let out = wal::replay_wal(&trailed, &mut stores3, &mut tombs3);
    assert!(out.corrupt_at.is_none());
    assert_eq!(out.stats.ops_applied, 1);
    assert!(stores3[&gid(42)].map.contains_key(&uid(1)));

    // Remaining ops: remap soft-skips, expire sweeps, tombstone/purge lifecycle.
    let mut more = Vec::new();
    let mut p = Vec::new();
    wal::encode_frame_into(
        &mut more,
        wal::OP_UPSERT,
        wal::encode_upsert_payload(
            &mut p,
            gid(42),
            uid(7),
            &StickyEntry {
                bits: 0b11,
                left_at: 1000,
            },
            None,
        ),
    );
    p.clear();
    wal::encode_frame_into(
        &mut more,
        wal::OP_REMAP,
        wal::encode_remap_payload(&mut p, gid(42), rid(11)),
    );
    p.clear();
    wal::encode_frame_into(
        &mut more,
        wal::OP_EXPIRE,
        wal::encode_expire_payload(&mut p, gid(42), 100_000, crate::store::DURATION_1D_SECS),
    );
    p.clear();
    wal::encode_frame_into(
        &mut more,
        wal::OP_TOMBSTONE,
        wal::encode_tombstone_payload(&mut p, gid(42), 5000),
    );
    p.clear();
    wal::encode_frame_into(
        &mut more,
        wal::OP_TOMBSTONE,
        wal::encode_tombstone_payload(&mut p, gid(42), 0),
    );
    p.clear();
    wal::encode_frame_into(
        &mut more,
        wal::OP_PURGE,
        wal::encode_purge_payload(&mut p, gid(42)),
    );
    let mut stores4: FxHashMap<Id<GuildMarker>, GuildStore> = FxHashMap::default();
    let mut tombs4: FxHashMap<Id<GuildMarker>, u64> = FxHashMap::default();
    let cfg4 = GuildConfig::from_ids_dedup(&[rid(11), rid(22)]);
    stores4.insert(gid(42), GuildStore::new(gid(42), cfg4));
    let out = wal::replay_wal(&more, &mut stores4, &mut tombs4);
    assert!(out.corrupt_at.is_none());
    assert_eq!(out.stats.ops_applied, 6);
    // Remapped 11 is skipped, so the upsert resolves to 22-only; then the
    // 1-day sweep (age 99_000) expires it; purge drops the store + marker.
    assert!(!stores4.contains_key(&gid(42)));
    assert!(!tombs4.contains_key(&gid(42)));
}

#[test]
fn csv_export_import_round_trip() {
    use crate::store::{csv, export_guild, import_replace};

    let mut store = GuildStore::new(gid(42), GuildConfig::from_ids_dedup(&[rid(11), rid(22)]));
    store.config.save_nicknames = true;
    store.map.insert(
        uid(7),
        StickyEntry {
            bits: 0b01,
            left_at: 2000,
        },
    );
    store.set_nick(uid(7), Some("Hi".into()));
    let text = export_guild(&store);
    assert!(text.starts_with(csv::CSV_HEADER));

    let mut fresh = GuildStore::new(gid(42), GuildConfig::from_ids_dedup(&[rid(11), rid(22)]));
    fresh.config.save_nicknames = true;
    let report = import_replace(&mut fresh, &text, gid(42), 3000, 0, |_| false).unwrap();
    assert_eq!(report.rows.len(), 1);
    assert_eq!(fresh.map[&uid(7)].bits, 0b01);
    assert_eq!(fresh.nick_for(uid(7)), Some("Hi"));

    let err = import_replace(&mut fresh, &text, gid(999), 3000, 0, |_| false).unwrap_err();
    assert_eq!(err.kind, csv::ImportRejectKind::StoreMismatch);
}

#[test]
fn cache_keeps_only_worthwhile_rows() {
    use crate::cache::Cache;
    use twilight_model::gateway::payload::incoming::MemberRemove;
    use twilight_model::guild::{Member, MemberFlags};
    use twilight_model::user::User;

    fn user(id: u64) -> User {
        User {
            accent_color: None,
            avatar: None,
            avatar_decoration: None,
            avatar_decoration_data: None,
            banner: None,
            bot: false,
            discriminator: 0,
            email: None,
            flags: None,
            global_name: None,
            id: Id::new(id),
            locale: None,
            mfa_enabled: None,
            name: format!("user{id}"),
            premium_type: None,
            primary_guild: None,
            public_flags: None,
            system: None,
            verified: None,
        }
    }

    let mut cache = Cache::default();
    assert!(Cache::needs_chunk(100, 5));
    assert!(!Cache::needs_chunk(5, 5));
    assert!(!Cache::needs_chunk(0, 0));
    assert!(Cache::needs_chunk(0, 1));
    cache.upsert_guild_create(gid(1), &[], &[], uid(9), None, 0, false);
    cache.handle_update(
        &twilight_model::gateway::payload::incoming::MemberUpdate {
            avatar: None,
            communication_disabled_until: None,
            guild_id: gid(1),
            deaf: None,
            flags: Some(MemberFlags::empty()),
            joined_at: None,
            mute: None,
            nick: None,
            pending: false,
            premium_since: None,
            roles: vec![rid(10)],
            user: user(2),
        },
        false,
        None,
    );
    assert!(cache.guilds[&gid(1)].members.contains_key(&uid(2)));

    // The bot itself is always cached, even role-less (backs bot_top/perms);
    // anyone else role-less is dropped.
    for (id, bot) in [(2u64, None), (7u64, Some(uid(7))), (8u64, None)] {
        cache.handle_update(
            &twilight_model::gateway::payload::incoming::MemberUpdate {
                avatar: None,
                communication_disabled_until: None,
                guild_id: gid(1),
                deaf: None,
                flags: Some(MemberFlags::empty()),
                joined_at: None,
                mute: None,
                nick: None,
                pending: false,
                premium_since: None,
                roles: vec![],
                user: user(id),
            },
            false,
            bot,
        );
    }
    assert!(!cache.guilds[&gid(1)].members.contains_key(&uid(2)));
    assert!(cache.guilds[&gid(1)].members.contains_key(&uid(7)));
    assert!(!cache.guilds[&gid(1)].members.contains_key(&uid(8)));

    let member = Member {
        avatar: None,
        avatar_decoration_data: None,
        banner: None,
        communication_disabled_until: None,
        deaf: false,
        flags: MemberFlags::empty(),
        joined_at: None,
        mute: false,
        nick: Some("Nick".to_owned()),
        pending: false,
        premium_since: None,
        roles: vec![],
        user: user(3),
    };
    cache.upsert_guild_create(gid(2), &[], &[member], uid(9), None, 1, true);
    assert_eq!(cache.guilds[&gid(2)].nick_for(uid(3)), Some("Nick"));
    assert!(cache.guilds[&gid(2)].members[&uid(3)].is_empty());

    assert!(cache.handle_remove(
        &MemberRemove {
            guild_id: gid(1),
            user: user(7),
        },
        None,
    ));
    assert!(!cache.guilds[&gid(1)].members.contains_key(&uid(7)));
    assert!(cache.guilds[&gid(1)].nicks.is_none());
}

#[test]
fn remove_role_soft_skips() {
    let mut store = GuildStore::new(gid(7), GuildConfig::from_ids_dedup(&[rid(11), rid(22)]));
    store.map.insert(
        uid(1),
        StickyEntry {
            bits: 0b11,
            left_at: 1000,
        },
    );
    assert_eq!(store.remove_role(rid(11)), 1);
    assert!(store.config.is_skipped_idx(0));
    assert_eq!(store.remove_role(rid(11)), 0);
    assert_eq!(store.remove_role(rid(999)), 0);
    let mut out = Vec::new();
    assert_eq!(store.restore_into(uid(1), 2000, &mut out), Some(None));
    assert_eq!(out, vec![rid(22)]);
}

#[test]
fn expiry_boundary() {
    use crate::store::DURATION_1D_SECS;
    assert!(!GuildStore::is_expired(
        1000,
        1000 + DURATION_1D_SECS,
        DURATION_1D_SECS
    ));
    assert!(GuildStore::is_expired(
        1000,
        1000 + DURATION_1D_SECS + 1,
        DURATION_1D_SECS
    ));
    assert!(!GuildStore::is_expired(0, 99999, DURATION_1D_SECS));
    assert!(!GuildStore::is_expired(1, u32::MAX, 0));
    assert!(!GuildStore::is_expired(2000, 1000, DURATION_1D_SECS));
}

#[test]
fn import_truncates_to_newest_at_cap() {
    use crate::store::{csv, import_replace};

    let mut store = GuildStore::new(gid(42), GuildConfig::from_ids_dedup(&[rid(11)]));
    let mut text = String::from("guild_id,user_id,roles,nickname,left_at\n");
    for n in 1..=(csv::MAX_RESTORE_ROWS as u64 + 5) {
        text.push_str(&format!("42,{n},11,,{n}\n"));
    }
    let report = import_replace(&mut store, &text, gid(42), u32::MAX, 0, |_| false).unwrap();
    assert_eq!(report.skipped_truncated, 5);
    assert_eq!(report.rows.len(), csv::MAX_RESTORE_ROWS);
    assert_eq!(store.map.len(), csv::MAX_RESTORE_ROWS);
    assert!(!store.map.contains_key(&uid(1)));
    assert!(
        store
            .map
            .contains_key(&uid((csv::MAX_RESTORE_ROWS as u64) + 5))
    );
}

#[test]
fn audit_consume_is_single_shot() {
    use crate::events::audit;
    use twilight_model::guild::audit_log::AuditLogEventType;

    let g = gid(9001);
    let u = uid(1337);
    audit::insert(g, u, AuditLogEventType::MemberBanAdd);
    assert!(audit::was_banned_or_kicked(g, u, false));
    assert!(!audit::was_banned_or_kicked(g, u, false));
    assert!(!audit::was_banned_or_kicked(g, u, true));
    audit::insert(g, u, AuditLogEventType::MemberKick);
    assert!(audit::was_banned_or_kicked(g, u, false));
    assert!(!audit::was_banned_or_kicked(g, u, false));
    audit::note_bot_save(g, u);
    assert!(audit::take_bot_save(g, u));
    assert!(!audit::take_bot_save(g, u));
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn config_transition_remaps_and_skips() {
    use crate::store::{persist, registry};

    let _io = io_lock();
    let dir = std::env::temp_dir().join(format!("sticky-cfgtest-{}", std::process::id()));
    let _ = tokio::fs::remove_dir_all(&dir).await;
    unsafe { std::env::set_var("DATA_DIR", &dir) };
    crate::store::persist::invalidate_data_dir_for_test();
    let g = gid(0xC0FFEE);
    registry::remove_guild(g).await;

    registry::ensure_guild(g, GuildConfig::default()).await;
    persist::append_config(g, &[rid(11), rid(22)], None, true, true, false, 0)
        .await
        .unwrap();
    persist::append_upsert(
        g,
        uid(1),
        &StickyEntry {
            bits: 0b01,
            left_at: 1000,
        },
        None,
    )
    .await
    .unwrap();

    let (cfg, purged) = persist::append_config(g, &[rid(22)], None, true, true, false, 0)
        .await
        .unwrap();
    assert_eq!(purged, 0);
    assert_eq!(cfg.role_ids(), vec![rid(22)]);
    assert!(cfg.is_skipped_idx(1));
    let store = registry::with_guild(g, |s| s.clone()).await.unwrap();
    assert_eq!(store.map[&uid(1)].bits, 0b10);

    let (cfg, purged) = persist::append_config(g, &[], None, true, true, false, 0)
        .await
        .unwrap();
    assert_eq!(cfg.role_ids(), Vec::<Id<RoleMarker>>::new());
    let store = registry::with_guild(g, |s| s.clone()).await.unwrap();
    assert!(store.map.is_empty());
    assert_eq!(purged, 1);

    assert!(persist::is_grace_lapsed(100, 100 + persist::GRACE_SECS));
    assert!(!persist::is_grace_lapsed(
        100,
        100 + persist::GRACE_SECS - 1
    ));
    assert!(!persist::is_grace_lapsed(0, u64::MAX));

    // Nick-only rows survive dict transitions while nicks are on, and go with
    // the flag flip.
    let h = gid(0xC0FFE1);
    registry::remove_guild(h).await;
    registry::ensure_guild(h, GuildConfig::default()).await;
    persist::append_config(h, &[rid(55)], None, true, true, true, 0)
        .await
        .unwrap();
    persist::append_upsert(
        h,
        uid(2),
        &StickyEntry {
            bits: 0,
            left_at: 2000,
        },
        Some("Solo"),
    )
    .await
    .unwrap();
    let solo = registry::with_guild(h, |s| s.clone()).await.unwrap();
    assert_eq!(solo.map[&uid(2)].bits, 0);
    assert_eq!(solo.nick_for(uid(2)), Some("Solo"));
    persist::append_config(h, &[rid(66)], None, true, true, true, 0)
        .await
        .unwrap();
    let solo = registry::with_guild(h, |s| s.clone()).await.unwrap();
    assert_eq!(solo.map[&uid(2)].bits, 0);
    assert_eq!(solo.nick_for(uid(2)), Some("Solo"));
    persist::append_config(h, &[rid(66)], None, true, true, false, 0)
        .await
        .unwrap();
    let solo = registry::with_guild(h, |s| s.clone()).await.unwrap();
    assert!(solo.map.is_empty());

    // Dict caps at MAX_ROLES, actives first.
    let many: Vec<Id<RoleMarker>> = (1..=30u64).map(rid).collect();
    let (cfg, _) = persist::append_config(h, &many, None, true, true, false, 0)
        .await
        .unwrap();
    assert_eq!(cfg.roles.len(), crate::store::MAX_ROLES);
    assert_eq!(cfg.role_ids().len(), crate::store::MAX_ROLES);

    // Live RAM must equal a fresh replay of this test's WAL (no fork).
    let bytes = std::fs::read(persist::wal_path()).unwrap();
    let mut replayed: rustc_hash::FxHashMap<Id<GuildMarker>, GuildStore> =
        rustc_hash::FxHashMap::default();
    let mut tombs: rustc_hash::FxHashMap<Id<GuildMarker>, u64> = rustc_hash::FxHashMap::default();
    let out = crate::store::wal::replay_wal(&bytes, &mut replayed, &mut tombs);
    assert!(out.corrupt_at.is_none());
    for gid in [g, h] {
        let live = registry::with_guild(gid, |s| s.clone()).await.unwrap();
        assert_eq!(replayed.get(&gid), Some(&live), "replay fork on {gid:?}");
    }

    registry::remove_guild(g).await;
    registry::remove_guild(h).await;
    let _ = tokio::fs::remove_dir_all(&dir).await;
    unsafe { std::env::remove_var("DATA_DIR") };
}

#[test]
fn invoker_gate_needs_roles_and_guild() {
    use crate::commands::Gate;
    use twilight_model::guild::Permissions;

    let base = Gate {
        is_owner: false,
        invoker_top: 10,
        invoker_perms: Permissions::empty(),
        bot_top: 10,
        bot_perms: Permissions::MANAGE_ROLES,
    };
    assert!(!Gate { ..base }.invoker_granted());
    assert!(
        !Gate {
            invoker_perms: Permissions::MANAGE_ROLES,
            ..base
        }
        .invoker_granted()
    );
    assert!(
        !Gate {
            invoker_perms: Permissions::MANAGE_GUILD,
            ..base
        }
        .invoker_granted()
    );
    assert!(
        Gate {
            invoker_perms: Permissions::MANAGE_ROLES | Permissions::MANAGE_GUILD,
            ..base
        }
        .invoker_granted()
    );
    assert!(
        Gate {
            invoker_perms: Permissions::ADMINISTRATOR,
            ..base
        }
        .invoker_granted()
    );
    assert!(
        Gate {
            is_owner: true,
            ..base
        }
        .invoker_granted()
    );
    assert!(Gate { ..base }.bot_granted());
    assert!(
        !Gate {
            bot_perms: Permissions::empty(),
            ..base
        }
        .bot_granted()
    );
}

#[test]
fn nick_truncate_matches_discord_limit() {
    use crate::store::{GuildStore, MAX_NICK_BYTES, MAX_NICK_CHARS};

    assert_eq!(GuildStore::truncate_nick("Ash"), "Ash");
    let long_ascii = "a".repeat(40);
    assert_eq!(
        GuildStore::truncate_nick(&long_ascii).chars().count(),
        MAX_NICK_CHARS
    );
    let long_emoji = "é".repeat(40);
    let cut = GuildStore::truncate_nick(&long_emoji);
    assert_eq!(cut.chars().count(), MAX_NICK_CHARS);
    assert!(cut.len() <= MAX_NICK_BYTES);
    let wide = "🎉".repeat(40);
    let cut = GuildStore::truncate_nick(&wide);
    assert!(cut.len() <= MAX_NICK_BYTES);
    assert!(cut.chars().count() <= MAX_NICK_CHARS);
    let mut sided = GuildStore::new(gid(1), GuildConfig::default());
    sided.set_nick(uid(1), Some("".into()));
    assert!(sided.nicks.is_empty());
    sided.set_nick(uid(1), Some("Bo".into()));
    assert_eq!(sided.nick_for(uid(1)), Some("Bo"));
    sided.set_nick(uid(1), None);
    assert!(sided.nicks.is_empty());
}

/// One loop over member leave (+ one rejoin, + late audits): roles, nicks,
/// bots, kick checks. Nick-only leavers keep a zero-bit row; role-less
/// nick-less leavers purge. A late audit purges human saves but not bot ones.
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn leave_save_matrix() {
    use crate::cache::{GuildCache, RoleInfo};
    use crate::events::{audit, cache};
    use crate::store::{GuildConfig, StickyEntry, registry};
    use twilight_model::gateway::payload::incoming::{MemberAdd, MemberRemove};
    use twilight_model::guild::audit_log::AuditLogEventType;
    use twilight_model::guild::{Member, MemberFlags, Permissions};

    struct Case {
        name: &'static str,
        roles: &'static [u64],
        nick: Option<&'static str>,
        prev: Option<(u32, Option<&'static str>)>,
        save_nicks: bool,
        bot: bool,
        include_bots: bool,
        dont_save: bool,
        audit_kick: bool,
        join_after: bool,
        late_audit: bool,
        want_bits: Option<u32>,
        want_nick: Option<&'static str>,
        final_bits: Option<u32>,
        final_nick: Option<&'static str>,
    }

    let cases = [
        Case {
            name: "roles+nick",
            roles: &[11],
            nick: Some("N"),
            prev: None,
            save_nicks: true,
            bot: false,
            include_bots: true,
            dont_save: false,
            audit_kick: false,
            join_after: false,
            late_audit: false,
            want_bits: Some(0b01),
            want_nick: Some("N"),
            final_bits: Some(0b01),
            final_nick: Some("N"),
        },
        Case {
            name: "roles, no nick",
            roles: &[11],
            nick: None,
            prev: None,
            save_nicks: true,
            bot: false,
            include_bots: true,
            dont_save: false,
            audit_kick: false,
            join_after: false,
            late_audit: false,
            want_bits: Some(0b01),
            want_nick: None,
            final_bits: Some(0b01),
            final_nick: None,
        },
        Case {
            name: "nick only",
            roles: &[],
            nick: Some("N"),
            prev: None,
            save_nicks: true,
            bot: false,
            include_bots: true,
            dont_save: false,
            audit_kick: false,
            join_after: false,
            late_audit: false,
            want_bits: Some(0),
            want_nick: Some("N"),
            final_bits: Some(0),
            final_nick: Some("N"),
        },
        Case {
            name: "nick only overwrites stale row",
            roles: &[],
            nick: Some("N"),
            prev: Some((0b01, Some("Old"))),
            save_nicks: true,
            bot: false,
            include_bots: true,
            dont_save: false,
            audit_kick: false,
            join_after: false,
            late_audit: false,
            want_bits: Some(0),
            want_nick: Some("N"),
            final_bits: Some(0),
            final_nick: Some("N"),
        },
        Case {
            name: "untracked roles + nick",
            roles: &[99],
            nick: Some("N"),
            prev: None,
            save_nicks: true,
            bot: false,
            include_bots: true,
            dont_save: false,
            audit_kick: false,
            join_after: false,
            late_audit: false,
            want_bits: Some(0),
            want_nick: Some("N"),
            final_bits: Some(0),
            final_nick: Some("N"),
        },
        Case {
            name: "empty leave purges stale row",
            roles: &[],
            nick: None,
            prev: Some((0b01, None)),
            save_nicks: true,
            bot: false,
            include_bots: true,
            dont_save: false,
            audit_kick: false,
            join_after: false,
            late_audit: false,
            want_bits: None,
            want_nick: None,
            final_bits: None,
            final_nick: None,
        },
        Case {
            name: "nicks off drops nick, keeps roles",
            roles: &[11],
            nick: Some("N"),
            prev: None,
            save_nicks: false,
            bot: false,
            include_bots: true,
            dont_save: false,
            audit_kick: false,
            join_after: false,
            late_audit: false,
            want_bits: Some(0b01),
            want_nick: None,
            final_bits: Some(0b01),
            final_nick: None,
        },
        Case {
            name: "nick changed, roles unchanged",
            roles: &[11],
            nick: Some("New"),
            prev: Some((0b01, Some("Old"))),
            save_nicks: true,
            bot: false,
            include_bots: true,
            dont_save: false,
            audit_kick: false,
            join_after: false,
            late_audit: false,
            want_bits: Some(0b01),
            want_nick: Some("New"),
            final_bits: Some(0b01),
            final_nick: Some("New"),
        },
        Case {
            name: "bot saved when included",
            roles: &[11],
            nick: None,
            prev: None,
            save_nicks: true,
            bot: true,
            include_bots: true,
            dont_save: false,
            audit_kick: false,
            join_after: false,
            late_audit: false,
            want_bits: Some(0b01),
            want_nick: None,
            final_bits: Some(0b01),
            final_nick: None,
        },
        Case {
            name: "kicked bot saved despite kick check",
            roles: &[11],
            nick: None,
            prev: None,
            save_nicks: true,
            bot: true,
            include_bots: true,
            dont_save: true,
            audit_kick: true,
            join_after: false,
            late_audit: false,
            want_bits: Some(0b01),
            want_nick: None,
            final_bits: Some(0b01),
            final_nick: None,
        },
        Case {
            name: "excluded bot never saved nor restored",
            roles: &[11],
            nick: None,
            prev: Some((0b01, None)),
            save_nicks: true,
            bot: true,
            include_bots: false,
            dont_save: false,
            audit_kick: false,
            join_after: true,
            late_audit: false,
            want_bits: None,
            want_nick: None,
            final_bits: None,
            final_nick: None,
        },
        Case {
            name: "late audit purges human save",
            roles: &[11],
            nick: None,
            prev: None,
            save_nicks: true,
            bot: false,
            include_bots: true,
            dont_save: true,
            audit_kick: false,
            join_after: false,
            late_audit: true,
            want_bits: Some(0b01),
            want_nick: None,
            final_bits: None,
            final_nick: None,
        },
        Case {
            name: "late audit keeps bot save",
            roles: &[11],
            nick: None,
            prev: None,
            save_nicks: true,
            bot: true,
            include_bots: true,
            dont_save: true,
            audit_kick: false,
            join_after: false,
            late_audit: true,
            want_bits: Some(0b01),
            want_nick: None,
            final_bits: Some(0b01),
            final_nick: None,
        },
    ];

    let _io = io_lock();
    let dir = std::env::temp_dir().join(format!("sticky-leavemtx-{}", std::process::id()));
    let _ = tokio::fs::remove_dir_all(&dir).await;
    unsafe { std::env::set_var("DATA_DIR", &dir) };
    crate::store::persist::invalidate_data_dir_for_test();

    for (i, c) in cases.iter().enumerate() {
        let g = gid(0xC000 + i as u64);
        let u = uid(0xD000 + i as u64);
        registry::remove_guild(g).await;

        let mut cfg = GuildConfig::from_ids_dedup(&[rid(11), rid(22)]);
        cfg.save_nicknames = c.save_nicks;
        cfg.include_bots = c.include_bots;
        cfg.dont_save_if_ban_kicked = c.dont_save;
        cfg.expiry_secs = 0;
        registry::ensure_guild(g, cfg).await;
        if let Some((bits, nick)) = c.prev {
            let mut guard = registry::stores().write().await;
            let store = guard.get_mut(&g).expect("store exists");
            store.map.insert(
                u,
                StickyEntry {
                    bits,
                    left_at: 1000,
                },
            );
            if let Some(n) = nick {
                store.set_nick(u, Some(n.into()));
            }
        }
        {
            let mut guard = cache().write().await;
            let mut roles = rustc_hash::FxHashMap::default();
            roles.insert(
                rid(11),
                RoleInfo {
                    position: 3,
                    managed: false,
                    permissions: Permissions::empty(),
                },
            );
            let mut guild = GuildCache {
                roles,
                members: [(
                    u,
                    c.roles
                        .iter()
                        .map(|&r| rid(r))
                        .collect::<Vec<_>>()
                        .into_boxed_slice(),
                )]
                .into_iter()
                .collect(),
                nicks: None,
                owner_id: uid(9),
                bot_top: 0,
                bot_perms: if c.audit_kick {
                    Permissions::VIEW_AUDIT_LOG
                } else {
                    Permissions::empty()
                },
            };
            if c.save_nicks
                && let Some(n) = c.nick
            {
                guild.nicks = Some(Box::new([(u, n.into())].into_iter().collect()));
            }
            guard.guilds.insert(g, guild);
        }
        if c.audit_kick {
            audit::insert(g, u, AuditLogEventType::MemberKick);
        }

        crate::events::member::on_member_remove(&MemberRemove {
            guild_id: g,
            user: member_user(u.get(), c.bot),
        })
        .await;

        let (bits, nick) = registry::with_guild(g, |s| {
            (
                s.map.get(&u).map(|e| e.bits),
                s.nick_for(u).map(str::to_owned),
            )
        })
        .await
        .unwrap_or((None, None));
        assert_eq!(bits, c.want_bits, "case '{}': bits", c.name);
        assert_eq!(nick.as_deref(), c.want_nick, "case '{}': nick", c.name);

        if c.late_audit {
            crate::events::audit::on_audit_entry(&ban_entry(g, u)).await;
            let (bits, nick) = registry::with_guild(g, |s| {
                (
                    s.map.get(&u).map(|e| e.bits),
                    s.nick_for(u).map(str::to_owned),
                )
            })
            .await
            .unwrap_or((None, None));
            assert_eq!(bits, c.final_bits, "case '{}': final bits", c.name);
            assert_eq!(
                nick.as_deref(),
                c.final_nick,
                "case '{}': final nick",
                c.name
            );
        }

        // Excluded-bot rejoin short-circuits before any restore (CTX is
        // uninitialized in tests, so reaching it would panic).
        if c.join_after {
            let member = Member {
                avatar: None,
                avatar_decoration_data: None,
                banner: None,
                communication_disabled_until: None,
                deaf: false,
                flags: MemberFlags::empty(),
                joined_at: None,
                mute: false,
                nick: None,
                pending: false,
                premium_since: None,
                roles: c.roles.iter().map(|&r| rid(r)).collect(),
                user: member_user(u.get(), c.bot),
            };
            crate::events::member::on_member_add(&MemberAdd {
                guild_id: g,
                member,
            })
            .await;
            assert!(
                !registry::has_entry(g, u).await,
                "case '{}': rejoin must not restore",
                c.name
            );
        }

        cache().write().await.remove_guild(g);
        registry::remove_guild(g).await;
    }

    let _ = tokio::fs::remove_dir_all(&dir).await;
    unsafe { std::env::remove_var("DATA_DIR") };
}

/// Kick → tombstoned with data kept → rejoin within grace keeps everything →
/// past grace the sweep purges and evicts.
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn kick_rejoin_keeps_data_within_grace() {
    use crate::store::{GuildConfig, StickyEntry, persist, registry};

    let _io = io_lock();
    let dir = std::env::temp_dir().join(format!("sticky-grace-{}", std::process::id()));
    let _ = tokio::fs::remove_dir_all(&dir).await;
    unsafe { std::env::set_var("DATA_DIR", &dir) };
    crate::store::persist::invalidate_data_dir_for_test();
    let g = gid(0x61ACE);
    let u = uid(0x6ACE1);
    registry::remove_guild(g).await;

    registry::ensure_guild(g, GuildConfig::default()).await;
    persist::append_config(g, &[rid(11)], None, true, true, false, 0)
        .await
        .unwrap();
    persist::append_upsert(
        g,
        u,
        &StickyEntry {
            bits: 0b1,
            left_at: 1000,
        },
        None,
    )
    .await
    .unwrap();

    // Kick: marker set, store and rows kept for the grace window.
    crate::events::guild::on_guild_delete(g).await;
    assert!(persist::is_tombstoned(g), "kick must tombstone");
    let kept = registry::with_guild(g, |s| s.clone()).await.unwrap();
    assert!(kept.map.contains_key(&u));
    assert_eq!(kept.config.role_ids(), vec![rid(11)]);

    // Rejoin within grace: cancel keeps everything.
    let outcome = persist::cancel_tombstone(g, persist::now_u64()).await;
    assert_eq!(outcome, persist::CancelOutcome::CancelledFresh);
    assert!(!persist::is_tombstoned(g));
    let kept = registry::with_guild(g, |s| s.clone()).await.unwrap();
    assert!(kept.map.contains_key(&u));

    // Past grace: sweep purges marker + store.
    persist::tombstone_guild(g, persist::now_u64()).await;
    assert!(persist::is_tombstoned(g));
    let purged = persist::sweep_tombstones(persist::now_u64() + persist::GRACE_SECS + 10).await;
    assert!(purged.contains(&g));
    assert!(!persist::is_tombstoned(g));
    assert!(registry::with_guild(g, |_| ()).await.is_none());

    registry::remove_guild(g).await;
    let _ = tokio::fs::remove_dir_all(&dir).await;
    unsafe { std::env::remove_var("DATA_DIR") };
}

#[test]
fn csv_parse_buckets_invalid_expired_and_empty() {
    use crate::store::csv::parse_import;

    let now = 10_000u32;
    let text = format!(
        "guild_id,user_id,roles,nickname,left_at\n\
         42,1,11,Ok,100\n\
         42,2,xx,Bad,100\n\
         42,3,11,Zero,0\n\
         42,4,11,Future,{}\n\
         42,5,99,Untracked,100\n\
         42,6,,Empty,100\n",
        now + crate::store::csv::FUTURE_SKEW_SECS + 1,
    );
    let report = parse_import(&text, gid(42), now).unwrap();
    assert_eq!(report.skipped_invalid, 2);
    assert_eq!(report.skipped_expired, 1);
    assert_eq!(report.skipped_truncated, 0);
    let ids: Vec<u64> = report.rows.iter().map(|r| r.user_id.get()).collect();
    assert_eq!(ids, vec![1, 5, 6]);
}

#[test]
fn import_replace_applies_live_expiry() {
    use crate::store::{DURATION_1D_SECS, import_replace};

    let now = 500_000u32;
    let mut store = GuildStore::new(gid(42), GuildConfig::from_ids_dedup(&[rid(11)]));
    store.config.expiry_secs = DURATION_1D_SECS;
    let text = format!(
        "guild_id,user_id,roles,nickname,left_at\n\
         42,7,11,Fresh,{}\n\
         42,8,11,Old,{}\n",
        now - 100,
        now - DURATION_1D_SECS - 1,
    );
    let report =
        import_replace(&mut store, &text, gid(42), now, DURATION_1D_SECS, |_| false).unwrap();
    assert_eq!(report.skipped_expired, 1);
    assert_eq!(report.rows.len(), 1);
    assert!(store.map.contains_key(&uid(7)));
    assert!(!store.map.contains_key(&uid(8)));
}

#[test]
fn cooldown_peek_burn_and_sweep() {
    use crate::commands::{CooldownMap, cooldown_check_at, cooldown_peek_at, cooldown_sweep_at};
    use std::time::{Duration, Instant};

    let map = CooldownMap::new(rustc_hash::FxHashMap::default());
    let t0 = Instant::now();
    assert!(cooldown_peek_at(&map, 7, 60, t0));
    assert_eq!(map.lock().unwrap().len(), 0);
    assert!(cooldown_check_at(&map, 7, 60, t0));
    assert!(!cooldown_check_at(&map, 7, 60, t0));
    assert!(!cooldown_peek_at(&map, 7, 60, t0));
    let later = t0 + Duration::from_secs(61);
    assert!(cooldown_peek_at(&map, 7, 60, later));
    assert!(cooldown_check_at(&map, 7, 60, later));
    map.lock()
        .unwrap()
        .insert(9, t0 - Duration::from_secs(3700));
    assert_eq!(cooldown_sweep_at(&map, later), 1);
}

#[test]
fn restore_denied_for_above_top_not_admin() {
    use crate::commands::{Gate, data::restore_denied};
    use twilight_model::guild::Permissions;

    let positions: rustc_hash::FxHashMap<Id<RoleMarker>, i16> =
        [(rid(11), 5), (rid(22), 9)].into_iter().collect();
    let base = Gate {
        is_owner: false,
        invoker_top: 6,
        invoker_perms: Permissions::MANAGE_ROLES | Permissions::MANAGE_GUILD,
        bot_top: 10,
        bot_perms: Permissions::MANAGE_ROLES,
    };
    assert!(restore_denied(&[rid(11), rid(22)], &positions, &base));
    assert!(!restore_denied(&[rid(11)], &positions, &base));
    assert!(!restore_denied(&[rid(99)], &positions, &base));
    assert!(!restore_denied(&[], &positions, &base));
    assert!(!restore_denied(
        &[rid(22)],
        &positions,
        &Gate {
            is_owner: true,
            ..base
        }
    ));
    // Admins never bypass position through the bot: an admin whose top is
    // below the role cannot grant it natively, so the import is denied too.
    assert!(restore_denied(
        &[rid(22)],
        &positions,
        &Gate {
            invoker_perms: Permissions::ADMINISTRATOR,
            ..base
        }
    ));
    assert!(!restore_denied(
        &[rid(22)],
        &positions,
        &Gate {
            invoker_top: 10,
            invoker_perms: Permissions::ADMINISTRATOR,
            ..base
        }
    ));
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn tombstone_edge_cases() {
    use crate::store::{GuildConfig, persist, registry};

    let _io = io_lock();
    let dir = std::env::temp_dir().join(format!("sticky-tombedge-{}", std::process::id()));
    let _ = tokio::fs::remove_dir_all(&dir).await;
    unsafe { std::env::set_var("DATA_DIR", &dir) };
    crate::store::persist::invalidate_data_dir_for_test();
    let g = gid(0x70E0);
    registry::remove_guild(g).await;

    // Dataless guild: immediate drop, no marker, no WAL.
    registry::ensure_guild(g, GuildConfig::default()).await;
    assert!(!persist::tombstone_guild(g, persist::now_u64()).await);
    assert!(!persist::is_tombstoned(g));

    // Idempotent re-tombstone while data present.
    let mut cfg = GuildConfig::from_ids_dedup(&[rid(11)]);
    cfg.expiry_secs = 0;
    registry::ensure_guild(g, cfg).await;
    persist::append_upsert(
        g,
        uid(1),
        &StickyEntry {
            bits: 0b1,
            left_at: 1000,
        },
        None,
    )
    .await
    .unwrap();
    assert!(persist::tombstone_guild(g, persist::now_u64()).await);
    assert!(persist::tombstone_guild(g, persist::now_u64()).await);
    assert!(persist::is_tombstoned(g));

    // Lapsed marker cancels as expired purge, store gone.
    let now = persist::now_u64();
    let outcome = persist::cancel_tombstone(g, now + persist::GRACE_SECS + 1).await;
    assert_eq!(outcome, persist::CancelOutcome::ExpiredPurged);
    assert!(!persist::is_tombstoned(g));
    assert!(registry::with_guild(g, |_| ()).await.is_none());

    registry::remove_guild(g).await;
    let _ = tokio::fs::remove_dir_all(&dir).await;
    unsafe { std::env::remove_var("DATA_DIR") };
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn append_expire_sweeps_once_then_noop() {
    use crate::store::{DURATION_1D_SECS, GuildConfig, StickyEntry, persist, registry};

    let _io = io_lock();
    let dir = std::env::temp_dir().join(format!("sticky-expire-{}", std::process::id()));
    let _ = tokio::fs::remove_dir_all(&dir).await;
    unsafe { std::env::set_var("DATA_DIR", &dir) };
    crate::store::persist::invalidate_data_dir_for_test();
    let g = gid(0xE121);
    registry::remove_guild(g).await;

    let mut cfg = GuildConfig::from_ids_dedup(&[rid(11)]);
    cfg.expiry_secs = DURATION_1D_SECS;
    registry::ensure_guild(g, cfg).await;
    let now = 500_000u32;
    {
        let mut guard = registry::stores().write().await;
        let store = guard.get_mut(&g).expect("store exists");
        store.map.insert(
            uid(1),
            StickyEntry {
                bits: 0b1,
                left_at: now - DURATION_1D_SECS - 1,
            },
        );
        store.map.insert(
            uid(2),
            StickyEntry {
                bits: 0b1,
                left_at: now - 100,
            },
        );
    }
    assert_eq!(persist::append_expire(g, now).await.unwrap(), 1);
    assert!(!registry::has_entry(g, uid(1)).await);
    assert!(registry::has_entry(g, uid(2)).await);
    assert_eq!(persist::append_expire(g, now).await.unwrap(), 0);

    registry::remove_guild(g).await;
    let _ = tokio::fs::remove_dir_all(&dir).await;
    unsafe { std::env::remove_var("DATA_DIR") };
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn import_replace_noop_writes_no_frames() {
    use crate::store::{GuildConfig, persist, registry};

    let _io = io_lock();
    let dir = std::env::temp_dir().join(format!("sticky-impnoop-{}", std::process::id()));
    let _ = tokio::fs::remove_dir_all(&dir).await;
    unsafe { std::env::set_var("DATA_DIR", &dir) };
    crate::store::persist::invalidate_data_dir_for_test();
    let g = gid(0x1AB0);
    registry::remove_guild(g).await;

    let mut cfg = GuildConfig::from_ids_dedup(&[rid(11)]);
    cfg.save_nicknames = true;
    registry::ensure_guild(g, cfg).await;
    let text = "guild_id,user_id,roles,nickname,left_at\n".to_owned()
        + &format!("{},7,11,Hi,2000\n", g.get());
    let is_member = |_: Id<UserMarker>| false;
    persist::append_import_replace(g, &text, 0, is_member)
        .await
        .unwrap();
    let len = std::fs::metadata(persist::wal_path()).map(|m| m.len()).ok();
    persist::append_import_replace(g, &text, 0, is_member)
        .await
        .unwrap();
    assert_eq!(
        std::fs::metadata(persist::wal_path()).map(|m| m.len()).ok(),
        len
    );

    registry::remove_guild(g).await;
    let _ = tokio::fs::remove_dir_all(&dir).await;
    unsafe { std::env::remove_var("DATA_DIR") };
}

#[test]
fn removal_backup_message_mentions_nicks_only_when_present() {
    use crate::commands::messages::removal_backup_message;

    let counts = vec![(rid(11), 3)];
    let msg = removal_backup_message(&counts, true).expect("holders imply message");
    assert!(msg.contains("saved nicknames"), "{msg}");
    let msg = removal_backup_message(&counts, false).expect("holders imply message");
    assert!(!msg.contains("nickname"), "{msg}");
    assert!(removal_backup_message(&[], false).is_none());
}

/// Nick-only row + flag off at rejoin: the stored nick routes through the
/// skip path (logged) and the row is cleaned, not silently kept.
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn join_nick_only_flag_off_logs_skip_and_cleans() {
    use crate::cache::{GuildCache, RoleInfo};
    use crate::events::{cache, member::on_member_add};
    use crate::store::{GuildConfig, StickyEntry, registry};
    use rustc_hash::FxHashMap;
    use twilight_model::gateway::payload::incoming::MemberAdd;
    use twilight_model::guild::{Member, MemberFlags, Permissions};

    let _io = io_lock();
    let dir = std::env::temp_dir().join(format!("sticky-joinnick-{}", std::process::id()));
    let _ = tokio::fs::remove_dir_all(&dir).await;
    unsafe { std::env::set_var("DATA_DIR", &dir) };
    crate::store::persist::invalidate_data_dir_for_test();
    let g = gid(0x9A0);
    let u = uid(0x9A1);
    registry::remove_guild(g).await;

    let mut cfg = GuildConfig::from_ids_dedup(&[rid(11)]);
    cfg.save_nicknames = false;
    cfg.expiry_secs = 0;
    registry::ensure_guild(g, cfg).await;
    {
        let mut guard = registry::stores().write().await;
        let store = guard.get_mut(&g).expect("store exists");
        store.map.insert(
            u,
            StickyEntry {
                bits: 0,
                left_at: 1000,
            },
        );
        store.set_nick(u, Some("Bo".into()));
    }
    {
        let mut guard = cache().write().await;
        let mut roles = FxHashMap::default();
        roles.insert(
            rid(11),
            RoleInfo {
                position: 3,
                managed: false,
                permissions: Permissions::empty(),
            },
        );
        guard.guilds.insert(
            g,
            GuildCache {
                roles,
                members: FxHashMap::default(),
                nicks: None,
                owner_id: uid(9),
                bot_top: 10,
                bot_perms: Permissions::MANAGE_ROLES,
            },
        );
    }

    on_member_add(&MemberAdd {
        guild_id: g,
        member: Member {
            avatar: None,
            avatar_decoration_data: None,
            banner: None,
            communication_disabled_until: None,
            deaf: false,
            flags: MemberFlags::empty(),
            joined_at: None,
            mute: false,
            nick: None,
            pending: false,
            premium_since: None,
            roles: vec![],
            user: member_user(u.get(), false),
        },
    })
    .await;

    assert!(
        !registry::has_entry(g, u).await,
        "flag-off nick-only row must be cleaned after the skip"
    );

    cache().write().await.remove_guild(g);
    registry::remove_guild(g).await;
    let _ = tokio::fs::remove_dir_all(&dir).await;
    unsafe { std::env::remove_var("DATA_DIR") };
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn config_flip_off_purges_nicks_keep_roles() {
    use crate::store::{GuildConfig, StickyEntry, persist, registry};

    let _io = io_lock();
    let dir = std::env::temp_dir().join(format!("sticky-nickflip-{}", std::process::id()));
    let _ = tokio::fs::remove_dir_all(&dir).await;
    unsafe { std::env::set_var("DATA_DIR", &dir) };
    crate::store::persist::invalidate_data_dir_for_test();
    let g = gid(0x9C40);
    registry::remove_guild(g).await;

    let mut cfg = GuildConfig::from_ids_dedup(&[rid(11)]);
    cfg.save_nicknames = true;
    registry::ensure_guild(g, cfg).await;
    persist::append_upsert(
        g,
        uid(1),
        &StickyEntry {
            bits: 0b1,
            left_at: 1000,
        },
        Some("Bo"),
    )
    .await
    .unwrap();
    persist::append_upsert(
        g,
        uid(2),
        &StickyEntry {
            bits: 0,
            left_at: 1000,
        },
        Some("Solo"),
    )
    .await
    .unwrap();

    let (cfg, _) = persist::append_config(g, &[rid(11)], None, true, true, false, 0)
        .await
        .unwrap();
    assert!(!cfg.save_nicknames);
    let store = registry::with_guild(g, |s| s.clone()).await.unwrap();
    assert_eq!(store.map[&uid(1)].bits, 0b1);
    assert!(store.nick_for(uid(1)).is_none());
    assert!(!store.map.contains_key(&uid(2)));
    assert!(store.nicks.is_empty());

    registry::remove_guild(g).await;
    let _ = tokio::fs::remove_dir_all(&dir).await;
    unsafe { std::env::remove_var("DATA_DIR") };
}

#[test]
fn import_replace_reports_stripped_nicks() {
    use crate::store::import_replace;

    let mut store = GuildStore::new(gid(42), GuildConfig::from_ids_dedup(&[rid(11)]));
    assert!(!store.config.save_nicknames);
    let text = "guild_id,user_id,roles,nickname,left_at\n\
                42,7,11,Hi,2000\n\
                42,8,,Solo,2001\n";
    let report = import_replace(&mut store, text, gid(42), 3000, 0, |_| false).unwrap();
    assert_eq!(report.skipped_nicknames, 2);
    assert_eq!(store.map[&uid(7)].bits, 0b1);
    assert!(store.nick_for(uid(7)).is_none());
    assert!(!store.map.contains_key(&uid(8)));
}

#[test]
fn pending_chunk_backoff_respects_server_limit() {
    use crate::store::registry::chunk_backoff_ms;

    assert_eq!(chunk_backoff_ms(0), 0);
    assert!(chunk_backoff_ms(1) >= 30_000);
    assert!(chunk_backoff_ms(2) >= chunk_backoff_ms(1));
    assert!(chunk_backoff_ms(u32::MAX) <= 300_000);
}
