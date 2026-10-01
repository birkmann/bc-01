//! Library scope: WS1's canonical `bc_libcore::scope::Scope`, plus the query parameters every
//! scoped route accepts and a few helpers for the SQL builders of this crate.

use bc_db::rusqlite::Connection;
pub use bc_libcore::scope::{Mode, Scope};
use bc_types::ScopeMode;
use serde::Deserialize;

use crate::error::Result;

/// `?scope=mine|all&source_fan_id=7`
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ScopeParams {
    pub scope: Option<ScopeMode>,
    pub source_fan_id: Option<i64>,
}

/// Resolve the scope of a request (the saved switches decide when nothing is asked).
pub fn resolve(conn: &Connection, p: &ScopeParams) -> Result<Scope> {
    Ok(Scope::resolve(conn, p.scope, p.source_fan_id)?)
}

/// Constructors and the always-present predicate over the tracks alias `t`.
pub trait ScopeExt {
    fn all() -> Scope;
    fn mine() -> Scope;
    fn fan(id: i64) -> Scope;
    fn without_snippets(self) -> Scope;
    /// Predicate over `t`; `1=1` when unfiltered (so it can always follow `AND`).
    fn tp(&self) -> String;
}

impl ScopeExt for Scope {
    fn all() -> Scope {
        Scope { mode: Mode::All, no_snippets: false }
    }
    fn mine() -> Scope {
        Scope { mode: Mode::Mine, no_snippets: false }
    }
    fn fan(id: i64) -> Scope {
        Scope { mode: Mode::Fan(id), no_snippets: false }
    }
    fn without_snippets(mut self) -> Scope {
        self.no_snippets = true;
        self
    }
    fn tp(&self) -> String {
        self.track_pred("t").unwrap_or_else(|| "1=1".into())
    }
}
