//! Idempotent DDL for the analysis tables/columns (`crates/bc-db/migrations/req_ws3_analysis.sql`).
//! WS1 wires that file as a numbered migration; this guard makes the service work either way.

use bc_db::rusqlite::Transaction;
use bc_db::{Db, Result};

const NEW_COLUMNS: &[(&str, &str)] = &[
    ("bpm_candidates", "TEXT"),
    ("grid_kind", "TEXT"),
    ("downbeat_offset_ms", "REAL"),
    ("lra", "REAL"),
    ("true_peak_dbtp", "REAL"),
    ("energy_v2", "REAL"),
    ("analyzer", "TEXT"),
];

// Same DDL as crates/bc-db/migrations/req_ws3_analysis.sql (tables part), all IF NOT EXISTS.
const TABLES: &str = include_str!("tables.sql");

fn columns(t: &Transaction<'_>, table: &str) -> Result<Vec<String>> {
    let mut st = t.prepare(&format!("PRAGMA table_info({table})"))?;
    let v = st.query_map([], |r| r.get::<_, String>(1))?.collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(v)
}

pub fn ensure(db: &Db) -> Result<()> {
    db.write(|t| {
        let have = columns(t, "analysis")?;
        for (name, ty) in NEW_COLUMNS {
            if !have.iter().any(|c| c == name) {
                t.execute_batch(&format!("ALTER TABLE analysis ADD COLUMN {name} {ty}"))?;
            }
        }
        t.execute_batch(TABLES)?;
        Ok(())
    })
}
