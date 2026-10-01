//! Playlist DTOs. DJ-set DTOs live in `crate::sets` (workstream 3) and are served
//! by workstream 1's `/sets` CRUD routes.

use serde::{Deserialize, Serialize};

use super::TrackQuery;
use crate::TrackId;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct PlaylistOut {
    pub id: i64,
    pub name: String,
    pub description: Option<String>,
    /// `manual` | `smart`
    pub kind: String,
    #[serde(default)]
    pub track_count: i64,
    #[serde(default)]
    pub duration_ms: i64,
    pub art_url: Option<String>,
    pub created_at: Option<String>,
    /// Smart playlists: the saved filter (absent for manual ones).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rules: Option<TrackQuery>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct PlaylistCreate {
    pub name: String,
    pub description: Option<String>,
    /// `manual` (default) or `smart`.
    pub kind: Option<String>,
    /// Smart playlists: the saved filter spec, evaluated live.
    pub rules: Option<TrackQuery>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct PlaylistPatch {
    pub name: Option<String>,
    pub description: Option<String>,
    pub rules: Option<TrackQuery>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct PlaylistAddTracks {
    #[serde(default)]
    pub track_ids: Vec<TrackId>,
    pub at_index: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct PlaylistAdded {
    pub added: i64,
    /// Items in the list afterwards.
    #[serde(default)]
    pub total: i64,
}

/// `POST /playlists/{id}/tracks/{item_id}/move` answer: the item's new fractional position.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct PlaylistMoved {
    pub position: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct PlaylistMove {
    /// Index in the list WITHOUT the dragged item.
    pub to_index: i64,
}

/// `POST /playlists/from-tracks`: keep a view of the library (the `/tracks` filters) as a playlist.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct PlaylistFromTracks {
    pub name: Option<String>,
    #[serde(flatten)]
    pub filter: TrackQuery,
}

/// `GET /playlists/{id}/export?format=m3u8|csv|zip` (and `/sets/{id}/export`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct PlaylistExportQuery {
    pub format: crate::library::ExportFormat,
}
