//! UI-state DTOs shared by server and UI (workstream 5): column prefs, selection,
//! paging and the server-side ui_state keys.
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub const KEY_THEMES: &str = "themes";
pub const KEY_COLUMNS: &str = "columns";
pub const KEY_PLAYER_OUTPUT: &str = "player.output";
pub const KEY_SETTINGS_UI: &str = "ui.prefs";

/// Per-table column preferences (order, visibility, widths).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ColumnPrefs {
    #[serde(default)]
    pub order: Vec<String>,
    #[serde(default)]
    pub hidden: Vec<String>,
    #[serde(default)]
    pub widths: BTreeMap<String, f64>,
}

/// Selection as a tagged union (no 100k-element sets): `none`, an explicit id
/// set, or "everything matching the filter except these".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Selection {
    #[default]
    None,
    Include { ids: Vec<i64> },
    All { filter: serde_json::Value, excluded: Vec<i64> },
}

/// Misc UI preferences synced across devices.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UiPrefs {
    #[serde(default)]
    pub waveform_style: String, // "rgb" | "bands" | "mono"
    /// Player-bar waveform: "bars" (default) | "rgb" (spectral).
    #[serde(default = "default_player_wave")]
    pub player_wave_style: String,
    #[serde(default)]
    pub album_art_accent: bool,
    #[serde(default)]
    pub reduce_motion: bool,
    #[serde(default)]
    pub row_waveforms: bool,
    /// Tint tag chips with a stable per-tag hue (sidebar tags, tag cloud).
    #[serde(default)]
    pub tag_colors: bool,
    /// Library-stats widget in the sidebar footer: "off" (default) | "s" | "m" | "l".
    #[serde(default = "default_sidebar_stats")]
    pub sidebar_stats: String,
}
fn default_player_wave() -> String {
    "bars".into()
}
fn default_sidebar_stats() -> String {
    "off".into()
}
impl Default for UiPrefs {
    fn default() -> Self {
        Self { waveform_style: "rgb".into(), player_wave_style: default_player_wave(), album_art_accent: false, reduce_motion: false, row_waveforms: false, tag_colors: false, sidebar_stats: default_sidebar_stats() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn old_prefs_json_defaults_player_style() {
        let p: UiPrefs = serde_json::from_str(r#"{"waveform_style":"mono"}"#).unwrap();
        assert_eq!(p.player_wave_style, "bars");
        assert_eq!(p.waveform_style, "mono");
        assert_eq!(p.sidebar_stats, "off");
    }
}
