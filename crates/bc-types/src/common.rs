use serde::{Deserialize, Serialize};

pub type TrackId = i64;
pub type ReleaseId = i64;
pub type ArtistId = i64;
pub type LabelId = i64;
pub type TagId = i64;
pub type FanId = i64;
pub type PlaylistId = i64;
pub type SetId = i64;
/// Jobs use uuid strings (legacy schema).
pub type JobId = String;

/// Offset page (random access) — total is the full filtered count.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub total: i64,
    pub offset: i64,
    pub limit: i64,
}

/// RFC 9457 problem+json body, shared by every API error.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Problem {
    #[serde(rename = "type")]
    pub kind: String,
    pub title: String,
    pub status: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl Problem {
    pub fn new(status: u16, title: impl Into<String>) -> Self {
        Self { kind: "about:blank".into(), title: title.into(), status, detail: None }
    }
    pub fn detail(mut self, d: impl Into<String>) -> Self {
        self.detail = Some(d.into());
        self
    }
}

/// `202 Accepted` body for anything long-running (PLAN §3.4: everything long is a job).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Accepted {
    pub job_id: JobId,
}

/// Library scope (PLAN §9k of web/PLAN.md): mine | all, optional fan shelf.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ScopeMode {
    #[default]
    Mine,
    All,
}
