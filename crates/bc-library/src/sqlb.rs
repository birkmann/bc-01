//! A tiny dynamic-SQL helper: numbered parameters, AND-joined conditions.

use bc_db::rusqlite::types::Value;

#[derive(Debug, Default, Clone)]
pub struct Sql {
    pub conds: Vec<String>,
    pub params: Vec<Value>,
}

impl Sql {
    pub fn new() -> Self {
        Self::default()
    }

    /// Bind a value, returning its `?N` placeholder text.
    pub fn bind(&mut self, v: impl Into<Value>) -> String {
        self.params.push(v.into());
        format!("?{}", self.params.len())
    }

    pub fn cond(&mut self, c: impl Into<String>) {
        self.conds.push(c.into());
    }

    pub fn where_clause(&self) -> String {
        if self.conds.is_empty() { "1=1".into() } else { self.conds.join(" AND ") }
    }

    /// Cache key: SQL text + params.
    pub fn key(&self) -> String {
        format!("{}|{:?}", self.where_clause(), self.params)
    }
}
