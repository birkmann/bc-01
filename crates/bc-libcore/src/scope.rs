//! Which part of the library a listing shows (port of `services/library/scope.py`).
//!
//! Records downloaded from another person's wishlist are real rows but not *my* library:
//! `releases.source_fan_id` marks them and, by default, listings show only releases with no
//! source fan. A second independent exclusion hides Bandcamp teaser clips (`is_snippet`,
//! `snippet_only`). Both collapse to "no restriction" when nothing in the library could
//! match, so a library with neither pays nothing.
//!
//! Predicates are SQL fragments over a caller-chosen table alias; ids are inlined (i64).

use bc_db::rusqlite::{Connection, OptionalExtension};
use bc_types::ScopeMode;

use crate::error::ApiResult;

pub const UNIFIED_KEY: &str = "library.unified";
pub const HIDE_SNIPPETS_KEY: &str = "library.hide_snippets";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    All,
    Mine,
    Fan(i64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Scope {
    pub mode: Mode,
    /// Leave Bandcamp teaser clips out (tracks, and releases holding nothing else).
    pub no_snippets: bool,
}

pub const ALL: Scope = Scope { mode: Mode::All, no_snippets: false };

fn setting(c: &Connection, key: &str) -> ApiResult<Option<String>> {
    Ok(c.query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| r.get::<_, String>(0)).optional()?)
}

pub fn unified(c: &Connection) -> ApiResult<bool> {
    Ok(setting(c, UNIFIED_KEY)?.as_deref() == Some("1"))
}

pub fn snippets_hidden(c: &Connection) -> ApiResult<bool> {
    Ok(setting(c, HIDE_SNIPPETS_KEY)?.as_deref() == Some("1"))
}

pub fn any_foreign(c: &Connection) -> ApiResult<bool> {
    Ok(c.query_row("SELECT 1 FROM releases WHERE source_fan_id IS NOT NULL LIMIT 1", [], |r| r.get::<_, i64>(0))
        .optional()?
        .is_some())
}

pub fn any_snippets(c: &Connection) -> ApiResult<bool> {
    Ok(c.query_row("SELECT 1 FROM tracks WHERE is_snippet = 1 LIMIT 1", [], |r| r.get::<_, i64>(0))
        .optional()?
        .is_some()
        || c.query_row("SELECT 1 FROM releases WHERE snippet_only = 1 LIMIT 1", [], |r| r.get::<_, i64>(0))
            .optional()?
            .is_some())
}

impl Scope {
    pub fn filtered(&self) -> bool {
        self.mode != Mode::All || self.no_snippets
    }

    /// The scope a request asked for, or the configured default. `fan_id` pins one fan's shelf;
    /// `scope` is an explicit `mine`/`all`; absent both, the saved `unified` switch decides.
    pub fn resolve(c: &Connection, scope: Option<ScopeMode>, fan_id: Option<i64>) -> ApiResult<Scope> {
        let mut mode = if let Some(f) = fan_id {
            Mode::Fan(f)
        } else {
            match scope {
                Some(ScopeMode::All) => Mode::All,
                Some(ScopeMode::Mine) => Mode::Mine,
                None => {
                    if unified(c)? {
                        Mode::All
                    } else {
                        Mode::Mine
                    }
                }
            }
        };
        if mode == Mode::Mine && !any_foreign(c)? {
            mode = Mode::All;
        }
        let no_snippets = snippets_hidden(c)? && any_snippets(c)?;
        Ok(Scope { mode, no_snippets })
    }

    fn fan_pred(&self, r: &str) -> Option<String> {
        match self.mode {
            Mode::All => None,
            Mode::Mine => Some(format!("{r}.source_fan_id IS NULL")),
            Mode::Fan(id) => Some(format!("{r}.source_fan_id = {id}")),
        }
    }

    /// Predicate over a releases alias `r`, or `None` for no restriction.
    pub fn release_pred(&self, r: &str) -> Option<String> {
        let mut v = Vec::new();
        if let Some(p) = self.fan_pred(r) {
            v.push(p);
        }
        if self.no_snippets {
            v.push(format!("{r}.snippet_only = 0"));
        }
        join_and(v)
    }

    /// Predicate over a tracks alias `t`. The snippet half is a plain column test (a snippet can
    /// sit on an otherwise real record); the shelf half reaches into releases.
    pub fn track_pred(&self, t: &str) -> Option<String> {
        let mut v = Vec::new();
        if self.no_snippets {
            v.push(format!("{t}.is_snippet = 0"));
        }
        if let Some(p) = self.fan_pred("rs") {
            v.push(format!("EXISTS (SELECT 1 FROM releases rs WHERE rs.id = {t}.release_id AND {p})"));
        }
        join_and(v)
    }

    /// Predicate over an artists alias `a`: any in-scope release or track is theirs; and when
    /// hiding snippets, not an artist whose whole output is teaser clips.
    pub fn artist_pred(&self, a: &str) -> Option<String> {
        let mut v = Vec::new();
        if let Some(p) = self.fan_pred("rs") {
            v.push(format!(
                "(EXISTS (SELECT 1 FROM releases rs WHERE rs.artist_id = {a}.id AND {p}) \
                 OR EXISTS (SELECT 1 FROM tracks ts JOIN releases rs ON rs.id = ts.release_id WHERE ts.artist_id = {a}.id AND {p}))"
            ));
        }
        if self.no_snippets {
            // The artists the filter *removes*: touched by clips, with nothing else.
            v.push(format!(
                "NOT ((EXISTS (SELECT 1 FROM tracks tx WHERE tx.artist_id = {a}.id AND tx.is_snippet = 1) \
                       OR EXISTS (SELECT 1 FROM releases rx WHERE rx.artist_id = {a}.id AND rx.snippet_only = 1)) \
                      AND NOT EXISTS (SELECT 1 FROM tracks ty WHERE ty.artist_id = {a}.id AND ty.is_snippet = 0) \
                      AND NOT EXISTS (SELECT 1 FROM releases ry WHERE ry.artist_id = {a}.id AND ry.snippet_only = 0))"
            ));
        }
        join_and(v)
    }

    /// Predicate over a labels alias `l`.
    pub fn label_pred(&self, l: &str) -> Option<String> {
        let mut v = Vec::new();
        if let Some(p) = self.fan_pred("rs") {
            v.push(format!("EXISTS (SELECT 1 FROM releases rs WHERE rs.label_id = {l}.id AND {p})"));
        }
        if self.no_snippets {
            v.push(format!(
                "NOT (EXISTS (SELECT 1 FROM releases rx WHERE rx.label_id = {l}.id AND rx.snippet_only = 1) \
                      AND NOT EXISTS (SELECT 1 FROM releases ry WHERE ry.label_id = {l}.id AND ry.snippet_only = 0))"
            ));
        }
        join_and(v)
    }

    /// `AND <pred>` suffix, or empty.
    pub fn and_release(&self, r: &str) -> String {
        self.release_pred(r).map(|p| format!(" AND {p}")).unwrap_or_default()
    }
    pub fn and_track(&self, t: &str) -> String {
        self.track_pred(t).map(|p| format!(" AND {p}")).unwrap_or_default()
    }
    pub fn and_artist(&self, a: &str) -> String {
        self.artist_pred(a).map(|p| format!(" AND {p}")).unwrap_or_default()
    }
    pub fn and_label(&self, l: &str) -> String {
        self.label_pred(l).map(|p| format!(" AND {p}")).unwrap_or_default()
    }
}

fn join_and(v: Vec<String>) -> Option<String> {
    if v.is_empty() { None } else { Some(v.join(" AND ")) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bc_db::Db;

    fn seed(db: &Db) {
        db.write(|t| {
            t.execute_batch(
                "INSERT INTO artists(id,name,name_key,created_at) VALUES (1,'A','a','x'),(2,'B','b','x'),(3,'Clips','clips','x');
                 INSERT INTO fans(id,username,url,is_self,created_at) VALUES (1,'f','https://bandcamp.com/f',0,'x');
                 INSERT INTO releases(id,title,title_key,artist_id,kind,added_at,source_fan_id,snippet_only) VALUES
                    (1,'Mine','mine',1,'album','x',NULL,0),(2,'Theirs','theirs',2,'album','x',1,0),(3,'Teasers','teasers',3,'album','x',NULL,1);
                 INSERT INTO tracks(id,release_id,artist_id,title,title_key,loved,play_count,skip_count,added_at,is_snippet) VALUES
                    (1,1,1,'t1','t1',0,0,0,'x',0),(2,2,2,'t2','t2',0,0,0,'x',0),(3,3,3,'t3 [snippet]','t3',0,0,0,'x',1);",
            )?;
            Ok(())
        })
        .unwrap();
    }

    fn ids(db: &Db, sql: &str) -> Vec<i64> {
        let sql = sql.to_string();
        db.read(move |c| {
            let mut st = c.prepare(&sql)?;
            Ok(st.query_map([], |r| r.get(0))?.collect::<Result<Vec<i64>, _>>()?)
        })
        .unwrap()
    }

    #[test]
    fn mine_hides_fan_shelf_and_collapses_when_nothing_foreign() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("t.db")).unwrap();
        let s = db.read(|c| Ok(Scope::resolve(c, Some(ScopeMode::Mine), None).unwrap())).unwrap();
        assert_eq!(s.mode, Mode::All, "nothing foreign yet: collapses to all");
        seed(&db);
        let s = db.read(|c| Ok(Scope::resolve(c, None, None).unwrap())).unwrap();
        assert_eq!(s.mode, Mode::Mine);
        assert_eq!(ids(&db, &format!("SELECT t.id FROM tracks t WHERE 1=1{}", s.and_track("t"))), vec![1, 3]);
        assert_eq!(ids(&db, &format!("SELECT a.id FROM artists a WHERE 1=1{}", s.and_artist("a"))), vec![1, 3]);
        let all = db.read(|c| Ok(Scope::resolve(c, Some(ScopeMode::All), None).unwrap())).unwrap();
        assert!(!all.filtered());
        let fan = db.read(|c| Ok(Scope::resolve(c, None, Some(1)).unwrap())).unwrap();
        assert_eq!(ids(&db, &format!("SELECT r.id FROM releases r WHERE 1=1{}", fan.and_release("r"))), vec![2]);
    }

    #[test]
    fn snippet_exclusion_is_independent_and_asymmetric() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("t.db")).unwrap();
        seed(&db);
        db.write(|t| {
            t.execute("INSERT INTO settings(key,value,updated_at) VALUES ('library.hide_snippets','1','x')", [])?;
            // an artist with nothing at all must survive the snippet filter
            t.execute("INSERT INTO artists(id,name,name_key,created_at) VALUES (4,'Empty','empty','x')", [])?;
            Ok(())
        })
        .unwrap();
        let s = db.read(|c| Ok(Scope::resolve(c, Some(ScopeMode::All), None).unwrap())).unwrap();
        assert!(s.no_snippets && s.mode == Mode::All);
        assert_eq!(ids(&db, &format!("SELECT t.id FROM tracks t WHERE 1=1{}", s.and_track("t"))), vec![1, 2]);
        assert_eq!(ids(&db, &format!("SELECT r.id FROM releases r WHERE 1=1{}", s.and_release("r"))), vec![1, 2]);
        // artist 3 (all clips) removed, artist 4 (nothing) kept
        assert_eq!(ids(&db, &format!("SELECT a.id FROM artists a WHERE 1=1{}", s.and_artist("a"))), vec![1, 2, 4]);
    }
}
