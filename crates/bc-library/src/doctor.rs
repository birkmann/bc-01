//! `bc doctor`: health checks of the library database.

use bc_db::Db;
use bc_libcore::{ApiResult, Scope};
use bc_types::library::TrackQuery;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct DoctorReport {
    pub checks: Vec<Check>,
    pub ok: bool,
}

fn push(r: &mut DoctorReport, name: &str, ok: bool, detail: impl Into<String>) {
    r.checks.push(Check { name: name.into(), ok, detail: detail.into() });
}

#[derive(Debug, Clone, Default)]
pub struct DoctorOptions {
    pub build_trigram: bool,
    pub rebuild_fts: bool,
    pub full_integrity: bool,
}

pub fn run(db: &Db, opts: &DoctorOptions) -> ApiResult<DoctorReport> {
    let mut r = DoctorReport::default();
    if opts.rebuild_fts {
        let n = db.write_with::<usize, bc_libcore::ApiError>(|t| Ok(bc_db::fts::rebuild_all(t)?))?;
        push(&mut r, "rebuild-fts", true, format!("{n} tracks reindexed"));
    }
    if opts.build_trigram {
        let n = db.write_with::<usize, bc_libcore::ApiError>(|t| Ok(bc_db::fts::rebuild_trigram(t)?))?;
        push(&mut r, "build-trigram", true, format!("{n} tracks indexed"));
    }
    db.read_with::<(), bc_libcore::ApiError>(|c| {
        let one = |sql: &str| -> i64 { c.query_row(sql, [], |r| r.get(0)).unwrap_or(-1) };
        let integrity: String = c
            .query_row(if opts.full_integrity { "PRAGMA integrity_check" } else { "PRAGMA quick_check" }, [], |r| r.get(0))
            .unwrap_or_else(|e| e.to_string());
        push(&mut r, "sqlite integrity", integrity == "ok", integrity);
        let v = one("PRAGMA user_version");
        push(&mut r, "schema version", v == bc_db::migrate::latest_version(), format!("v{v}"));
        let fk: i64 = c.prepare("PRAGMA foreign_key_check").map(|mut s| s.query_map([], |_| Ok(())).map(|m| m.count() as i64).unwrap_or(-1)).unwrap_or(-1);
        push(&mut r, "foreign keys", fk == 0, format!("{fk} violations"));
        let (tracks, fts) = (one("SELECT COUNT(*) FROM tracks"), one("SELECT COUNT(*) FROM search_index"));
        push(&mut r, "track FTS parity", tracks == fts, format!("{tracks} tracks, {fts} fts rows (run --rebuild-fts if they differ)"));
        let orphans = one("SELECT COUNT(*) FROM tracks t WHERE NOT EXISTS (SELECT 1 FROM files f WHERE f.track_id = t.id)");
        push(&mut r, "tracks without a file", orphans == 0, format!("{orphans}"));
        let missing = one("SELECT COUNT(*) FROM files WHERE missing_since IS NOT NULL");
        push(&mut r, "missing files", true, format!("{missing} (marked, never deleted)"));
        let avail = one("SELECT COUNT(*) FROM tracks WHERE available = 0");
        push(&mut r, "unavailable tracks", true, format!("{avail}"));
        let drift = one(
            "SELECT COUNT(*) FROM tracks t WHERE t.available <> EXISTS (SELECT 1 FROM files f WHERE f.track_id = t.id AND f.missing_since IS NULL)",
        );
        push(&mut r, "denormalised `available` in sync", drift == 0, format!("{drift} drifted"));
        let art = one("SELECT COUNT(*) FROM artwork WHERE sizes = 0");
        push(&mut r, "artwork awaiting WebP", true, format!("{art}"));
        let stat1 = one("SELECT COUNT(*) FROM sqlite_stat1");
        push(&mut r, "planner statistics", stat1 > 0, format!("{stat1} rows (ANALYZE after import)"));
        // The hot listing must not scan `tracks` without an index.
        let scope = Scope::resolve(c, None, None)?;
        let (sql, params) = crate::tracks::explain_sql(c, &TrackQuery { offset: Some(1000), ..Default::default() }, &scope)?;
        let mut st = c.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?;
        let plan: Vec<String> = st.query_map(bc_db::rusqlite::params_from_iter(params.iter()), |r| r.get::<_, String>(3))?.collect::<Result<_, _>>()?;
        let bad = plan.iter().any(|l| (l.starts_with("SCAN tracks") || l.starts_with("SCAN t")) && !l.contains("USING"));
        push(&mut r, "hot listing plan", !bad, plan.join(" | "));
        Ok(())
    })?;
    r.ok = r.checks.iter().all(|c| c.ok);
    Ok(r)
}
