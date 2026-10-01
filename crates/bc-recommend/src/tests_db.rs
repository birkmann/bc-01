//! `EXPLAIN QUERY PLAN` guards for the hot SQL shapes (PLAN 9l) plus a few DB-level checks.
//!
//! The plans are asserted on a seeded temp library: none of the pool branches may `SCAN` the
//! tracks table (`SCAN t` / `SCAN tracks`). The one deliberate exception is documented below
//! (a nextup pool with no analysis constraint walks `tracks` in id order and stops at its LIMIT).

use bc_types::sets::PoolSource;
use bc_types::suggest::{Harmonic, SuggestRequest, TagMode, Tempo};

use crate::scope::{Scope, ScopeExt};
use crate::sqlutil::in_list;
use crate::testutil::{Lib, T};
use crate::{nextup, pool, sets, similar, similar_to, taste};

fn plan(lib: &Lib, sql: &str) -> Vec<String> {
    let sql = format!("EXPLAIN QUERY PLAN {sql}");
    lib.db
        .read(move |c| {
            let mut st = c.prepare(&sql)?;
            let rows = st.query_map([], |r| r.get::<_, String>(3))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .unwrap()
}

fn scans_tracks(plan: &[String]) -> bool {
    plan.iter().any(|l| {
        let l = l.trim();
        l == "SCAN t" || l.starts_with("SCAN t ") || l == "SCAN tracks" || l.starts_with("SCAN tracks ")
    })
}

fn assert_no_scan(lib: &Lib, label: &str, sql: &str) {
    let p = plan(lib, sql);
    assert!(!scans_tracks(&p), "{label} scans tracks:\n{}\n{sql}", p.join("\n"));
}

/// A little library with every kind of row the branches touch.
fn seeded() -> (Lib, i64) {
    let mut lib = Lib::new();
    let seed = lib.add(T::new("seed").bpm(128.0).key("8A").energy(0.5).tags(&["techno", "dub techno"]).artist("Vril").label("Ostgut"));
    for i in 0..40 {
        let mut t = T::new(&format!("t{i}")).bpm(120.0 + i as f64 % 20.0).key(&format!("{}A", 1 + i % 12)).tags(&["techno"]).artist(&format!("A{}", i % 7));
        if i % 3 == 0 {
            t = t.tags(&["techno", "dub techno"]).label("Ostgut");
        }
        if i % 5 == 0 {
            t = t.loved();
        }
        lib.add(t);
    }
    (lib, seed)
}

#[test]
fn similar_pool_branches_never_scan_tracks() {
    let (lib, seed_id) = seeded();
    let rows = lib.db.read(|c| Ok(nextup::load_seed_row(c, seed_id).unwrap().unwrap())).unwrap();
    let seed = similar::Seed {
        bpm: rows.bpm,
        camelot: rows.camelot.clone(),
        energy: rows.energy,
        tags: rows.tags.clone(),
        artist_id: rows.artist_id,
        release_id: rows.release_id,
        label_id: rows.label_id,
    };
    let tag_ids: Vec<i64> = lib.db.read(|c| Ok(crate::pooling::tag_rows(c, rows.tags.iter()).unwrap().iter().map(|r| r.0).collect())).unwrap();
    for scope in [Scope::all(), Scope::all().without_snippets()] {
        let stmts = similar::pool_statements(&seed, &Default::default(), &scope, &[seed_id], &tag_ids, 3);
        assert_eq!(stmts.len(), 5, "tags + artist x2 + label + loved");
        for (label, sql) in stmts {
            assert_no_scan(&lib, &format!("similar/{label}"), &sql);
            assert!(!sql.contains(" OR "), "{label} must not OR across tables");
            assert!(!sql.contains("IN (SELECT track_id FROM files"), "{label} must use the correlated EXISTS");
            assert!(sql.contains("EXISTS (SELECT 1 FROM files f WHERE f.track_id = t.id"));
        }
    }
}

#[test]
fn the_artist_lookup_is_two_statements() {
    let seed = similar::Seed { artist_id: Some(5), ..Default::default() };
    let stmts = similar::pool_statements(&seed, &Default::default(), &Scope::all(), &[], &[], 0);
    let artist: Vec<_> = stmts.iter().filter(|(l, _)| l.starts_with("artist")).collect();
    assert_eq!(artist.len(), 2);
    assert!(artist[0].1.contains("t.artist_id = 5") && !artist[0].1.contains("sr.artist_id"));
    assert!(artist[1].1.contains("sr.artist_id = 5") && !artist[1].1.contains("t.artist_id = "));
}

#[test]
fn the_mine_scope_keeps_the_branches_off_the_scan_too() {
    let (lib, seed_id) = seeded();
    lib.exec("INSERT INTO fans(id, username, url, is_self, created_at) VALUES (1, 'x', 'u', 0, '2026-01-01')", []);
    lib.exec("UPDATE releases SET source_fan_id = 1 WHERE id = 3", []);
    let seed = similar::Seed { artist_id: Some(1), label_id: Some(1), ..Default::default() };
    let _ = seed_id;
    for scope in [Scope::mine(), Scope::fan(1)] {
        for (label, sql) in similar::pool_statements(&seed, &Default::default(), &scope, &[], &[1, 2], 0) {
            assert_no_scan(&lib, &format!("similar/{label}/{scope:?}"), &sql);
        }
    }
}

#[test]
fn nextup_pool_never_scans_tracks_when_it_filters() {
    let (lib, seed_id) = seeded();
    let row = lib.db.read(|c| Ok(nextup::load_seed_row(c, seed_id).unwrap().unwrap())).unwrap();
    let seed = nextup::Seed {
        bpm: row.bpm,
        camelot: row.camelot.clone(),
        energy: row.energy,
        tags: row.tags.clone(),
        artist_id: row.artist_id,
        release_id: row.release_id,
    };
    let scope = Scope::all();
    let mut variants: Vec<(&str, SuggestRequest)> = vec![];
    variants.push(("strict", SuggestRequest::default()));
    let mut loose = SuggestRequest::default();
    loose.direction.harmonic = Harmonic::Loose;
    loose.direction.tempo = Tempo::Raise;
    variants.push(("loose+raise", loose));
    let mut stick = SuggestRequest::default();
    stick.direction.tag_mode = TagMode::Stick;
    variants.push(("stick", stick));
    let mut sw = SuggestRequest::default();
    sw.direction.tag_mode = TagMode::Switch;
    sw.direction.tags = vec!["dub techno".into()];
    variants.push(("switch", sw));
    let mut allow = SuggestRequest::default();
    allow.direction.allow_tags = vec!["dub techno".into()];
    allow.direction.deny_tags = vec!["house".into()];
    variants.push(("allow/deny", allow));
    let pl = SuggestRequest { playlist_id: Some(1), ..Default::default() };
    variants.push(("playlist", pl));
    for (label, req) in &variants {
        let sql = nextup::pool_sql_for_tests(&seed, req, &scope, None);
        assert_no_scan(&lib, &format!("nextup/{label}"), &sql);
        let restricted = nextup::pool_sql_for_tests(&seed, req, &scope, Some("SELECT id FROM tracks WHERE loved = 1"));
        assert_no_scan(&lib, &format!("nextup/{label}/restricted"), &restricted);
    }
}

#[test]
fn nextup_pool_without_a_harmonic_constraint_is_an_early_exit_id_walk() {
    // No camelot, no tempo: the pool is the first N present tracks by id. That is the legacy
    // shape; `SCAN t` here ends at the LIMIT (200..500 rows), it never visits the table.
    let (lib, _) = seeded();
    let mut req = SuggestRequest::default();
    req.direction.harmonic = Harmonic::Off;
    let sql = nextup::pool_sql_for_tests(&nextup::Seed::default(), &req, &Scope::all(), None);
    assert!(sql.contains("ORDER BY t.id LIMIT 200"));
    let p = plan(&lib, &sql);
    assert!(!p.iter().any(|l| l.contains("USE TEMP B-TREE")), "no sort needed:\n{}", p.join("\n"));
}

#[test]
fn pool_sources_never_scan_tracks() {
    let (lib, _) = seeded();
    let sources: Vec<PoolSource> = serde_json::from_str(
        r#"[{"kind":"tag","tag":"dub techno"},{"kind":"loved"},{"kind":"playlist","playlist_id":1},
            {"kind":"label","label_id":1},{"kind":"artist","artist_id":1},{"kind":"tracks","track_ids":[1,2,3]}]"#,
    )
    .unwrap();
    for scope in [Scope::all(), Scope::all().without_snippets()] {
        // tracks/loved arms are index lookups (ix_tracks_loved) or PK probes.
        for (label, sql) in sets::pool_statements(&sources, &scope) {
            // The page statement orders the whole union by added_at, which sorts the pool
            // (bounded by the sources) but must not scan the tracks table to find it.
            assert_no_scan(&lib, &label, &sql);
        }
    }
    // The whole pool is a single UNION, and no arm ORs across tables.
    let sql = pool::pool_track_ids(&sources);
    assert!(sql.contains(" UNION ") && !sql.contains(" OR "));
}

#[test]
fn taste_pool_never_ors_across_tables_or_scans() {
    let (lib, _) = seeded();
    lib.love(1);
    let profile = lib
        .db
        .read(|c| {
            let rows = taste::loved_rows(c).unwrap();
            let ids: Vec<i64> = rows.iter().map(|r| r.0).collect();
            let (tags, counts) = crate::pooling::tags_by_track(c, &ids).unwrap();
            let total = crate::pooling::total_tracks(c).unwrap();
            let idf: std::collections::HashMap<String, f64> = counts.iter().map(|(k, n)| (k.clone(), nextup::idf(*n, total))).collect();
            let loved: Vec<taste::LovedRow> = rows
                .iter()
                .map(|(tid, ta, ra, label, bpm, e)| taste::LovedRow {
                    track_id: *tid,
                    tags: tags.get(tid).cloned().unwrap_or_default(),
                    artist_id: ta.or(*ra),
                    label_id: *label,
                    bpm: *bpm,
                    energy: *e,
                })
                .collect();
            Ok(taste::build_profile(&loved, &idf))
        })
        .unwrap();
    let tag_ids = lib.db.read({ let p = profile.clone(); move |c| Ok(taste::pool_tag_ids(c, &p).unwrap()) }).unwrap();
    let stmts = taste::pool_statements(&profile, &Scope::all(), 0, &[], &tag_ids);
    assert!(stmts.len() >= 3);
    for (i, sql) in stmts.iter().enumerate() {
        assert!(!sql.contains(" OR "), "{sql}");
        assert_no_scan(&lib, &format!("taste/{i}"), sql);
    }
}

#[test]
fn similar_to_draw_never_scans() {
    let (lib, seed_id) = seeded();
    let ids = [seed_id, seed_id + 1, seed_id + 2];
    let profile = lib.db.read(move |c| Ok(similar_to::profile_of(c, &ids).unwrap())).unwrap();
    let tag_ids: Vec<i64> = vec![1, 2];
    for (label, sql) in similar_to::draw_statements(&profile, &tag_ids, &Scope::all(), 10, 0) {
        assert_no_scan(&lib, &format!("similar_to/{label}"), &sql);
        assert!(!sql.contains(" OR "));
    }
}

#[test]
fn in_lists_inline_ids() {
    assert_eq!(in_list(&[1, 2, 3]), "1,2,3");
    assert_eq!(in_list(&[]), "NULL");
}

#[test]
fn briefs_hydrate_the_display_row() {
    let (lib, seed_id) = seeded();
    let got = lib.db.read(move |c| Ok(crate::hydrate::briefs(c, &[seed_id]).unwrap())).unwrap();
    let t = &got[&seed_id];
    assert_eq!(t.title, "seed");
    assert_eq!(t.artist.as_ref().unwrap().name, "Vril");
    assert_eq!(t.release.as_ref().unwrap().title, "seed EP");
    assert_eq!(t.bpm, Some(128.0));
    assert_eq!(t.camelot.as_deref(), Some("8A"));
    let mut tags = t.tags.clone();
    tags.sort();
    assert_eq!(tags, ["dub techno", "techno"]);
    assert_eq!(t.stream_url, format!("/api/stream/{seed_id}"));
}

#[test]
fn scope_resolve_follows_the_saved_switches() {
    use crate::scope::{ScopeParams, resolve};
    use bc_types::ScopeMode;
    let (lib, _) = seeded();
    let r = |lib: &Lib, scope: Option<ScopeMode>, fan: Option<i64>| {
        lib.db.read(move |c| Ok(resolve(c, &ScopeParams { scope, source_fan_id: fan }).unwrap())).unwrap()
    };
    // Nothing foreign, no snippets: every explicit mode collapses to ALL.
    assert_eq!(r(&lib, None, None), Scope::all());
    assert_eq!(r(&lib, Some(ScopeMode::Mine), None), Scope::all());
    lib.exec("INSERT INTO fans(id, username, url, is_self, created_at) VALUES (1, 'x', 'u', 0, '2026-01-01')", []);
    lib.exec("UPDATE releases SET source_fan_id = 1 WHERE id = 2", []);
    assert_eq!(r(&lib, None, None), Scope::mine());
    assert_eq!(r(&lib, Some(ScopeMode::All), None), Scope::all());
    assert_eq!(r(&lib, None, Some(1)), Scope::fan(1));
    lib.exec("INSERT INTO settings(key, value, updated_at) VALUES ('library.unified', '1', '2026-01-01')", []);
    assert_eq!(r(&lib, None, None), Scope::all());
    lib.exec("INSERT INTO settings(key, value, updated_at) VALUES ('library.hide_snippets', '1', '2026-01-01')", []);
    assert!(!r(&lib, None, None).no_snippets);
    lib.exec("UPDATE tracks SET is_snippet = 1 WHERE id = 3", []);
    assert!(r(&lib, None, None).no_snippets);
}
