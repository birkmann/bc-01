//! Ports: what the player needs from the other workstreams, as traits, plus a
//! `DbPorts` shim that implements them straight against the legacy-shaped
//! library DB until the real services are published.
//!
//! * `TODO(ws1)`: `LibraryPort` -> WS1 library API (track resolve, play history, playlists, listings).
//! * `TODO(ws2)`: `BandcampPort` -> WS2 stream resolution + fan/explore walks.
//! * `TODO(ws3)`: `RecommendPort` -> `bc_recommend` next-up, `bc-music` mix points (peaks).

use bc_db::Db;
use bc_types::player::{FanCursor, ItemOrigin, LabelMode, Pool, QueueItem};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Debug, thiserror::Error)]
pub enum PortError {
    #[error("not found")]
    NotFound,
    #[error("not available: {0}")]
    Unavailable(String),
    #[error("{0}")]
    Other(String),
}

impl From<bc_db::DbError> for PortError {
    fn from(e: bc_db::DbError) -> Self {
        PortError::Other(e.to_string())
    }
}

pub type PortResult<T> = Result<T, PortError>;

/// The analysis facts the player uses (a slice of the `analysis` row).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TrackFacts {
    pub bpm: Option<f64>,
    pub bpm_confidence: Option<f64>,
    pub beat_offset_ms: Option<f64>,
    pub loudness_lufs: Option<f64>,
    pub true_peak_dbtp: Option<f64>,
    pub duration_ms: Option<i64>,
    /// v2 analysis: absolute beat origin and downbeat beat, when present.
    pub grid_origin_s: Option<f64>,
    pub downbeat_beat: Option<u32>,
    /// Analysed mix-in / mix-out cues (ms), when present.
    pub mix_in_ms: Option<i64>,
    pub mix_out_ms: Option<i64>,
}

/// The "what could come next" request: WS3's `SuggestRequest`, as the planner's crate sends it.
pub use bc_types::suggest::SuggestRequest as NextUpRequest;

/// One page of a fan's wishlist walk.
#[derive(Debug, Clone, Default)]
pub struct FanPage {
    pub items: Vec<FanItem>,
    pub exhausted: bool,
}

#[derive(Debug, Clone)]
pub struct FanItem {
    pub item_id: i64,
    pub url: String,
    pub release_id: Option<i64>,
}

/// One slot of a DJ set as the live player plays it (cues, tempo and the planned blend INTO it).
#[derive(Debug, Clone, PartialEq)]
pub struct SetSlot {
    pub item: QueueItem,
    pub cue_in_ms: Option<i64>,
    pub cue_out_ms: Option<i64>,
    pub tempo_adjust_pct: f64,
    pub key_lock: bool,
    /// Planned length of the blend into this slot, ms (from its `transition_beats` at its effective tempo).
    pub blend_in_ms: Option<i64>,
}

pub trait LibraryPort: Send + Sync {
    /// The planned slots of a DJ set (`QueueSource::Set`), read through bc-plist.
    fn set_plan(&self, _set_id: i64) -> PortResult<Vec<SetSlot>> {
        Ok(vec![])
    }
    fn track_item(&self, track_id: i64) -> PortResult<Option<QueueItem>>;
    fn file_path(&self, track_id: i64) -> PortResult<Option<PathBuf>>;
    fn facts(&self, track_id: i64) -> PortResult<Option<TrackFacts>>;
    /// Waveform peaks (int8 min/max pairs) for mix-point refinement. TODO(ws3).
    fn peaks(&self, _track_id: i64, _points: usize) -> PortResult<Option<Vec<(i8, i8)>>> {
        Ok(None)
    }
    fn record_play(&self, track_id: i64, ms_played: i64, completed: bool, skipped: bool) -> PortResult<()>;
    fn playlist_items(&self, playlist_id: i64) -> PortResult<Vec<QueueItem>>;
    fn random_tracks(&self, pool: Option<&Pool>, limit: usize) -> PortResult<Vec<QueueItem>>;
    fn next_release(&self, after: i64, listing: &serde_json::Value) -> PortResult<Option<i64>>;
    fn release_tracks(&self, release_id: i64) -> PortResult<Vec<QueueItem>>;
    fn next_label(&self, after: i64, listing: &serde_json::Value) -> PortResult<Option<i64>>;
    fn random_label(&self, listing: &serde_json::Value, exclude: i64) -> PortResult<Option<i64>>;
    fn label_tracks(&self, label_id: i64, mode: LabelMode) -> PortResult<Vec<QueueItem>>;
    fn shuffle_labels(&self, listing: &serde_json::Value, limit: usize) -> PortResult<Vec<QueueItem>>;
}

pub trait BandcampPort: Send + Sync {
    /// A playable URL for a Bandcamp item (stream resolution). TODO(ws2).
    fn resolve_stream(&self, item: &QueueItem) -> PortResult<String>;
    fn fan_next(&self, cursor: &FanCursor, after: Option<i64>, limit: usize) -> PortResult<FanPage>;
    /// Playable tracks of a Bandcamp release page.
    fn release_tracks(&self, url: &str) -> PortResult<Vec<QueueItem>>;
}

pub trait RecommendPort: Send + Sync {
    fn next_up(&self, req: &NextUpRequest) -> PortResult<Vec<QueueItem>>;
}

/// Small key/value persistence (the `settings` table until `ui_state` lands).
pub trait StatePort: Send + Sync {
    fn get(&self, key: &str) -> Option<String>;
    fn set(&self, key: &str, value: &str);
}

#[derive(Clone)]
pub struct Ports {
    pub library: Arc<dyn LibraryPort>,
    pub bandcamp: Arc<dyn BandcampPort>,
    pub recommend: Arc<dyn RecommendPort>,
    pub state: Arc<dyn StatePort>,
    /// Base URL for server-relative stream URLs (`/api/stream/..`).
    pub base_url: String,
}

// ---------------------------------------------------------------------------
// Shim over the legacy-shaped DB
// ---------------------------------------------------------------------------

/// `TODO(ws1/ws2/ws3)`: implements every port straight against the legacy schema.
#[derive(Clone)]
pub struct DbPorts {
    pub db: Db,
}

impl DbPorts {
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    pub fn into_ports(self, base_url: &str) -> Ports {
        let a = Arc::new(self);
        Ports { library: a.clone(), bandcamp: a.clone(), recommend: a.clone(), state: a, base_url: base_url.into() }
    }
}

const ITEM_SELECT: &str = "SELECT t.id, t.title, t.artist_id, COALESCE(a.name, ra.name), t.release_id, r.title, r.label_id, \
    t.track_no, t.duration_ms, t.loved, an.bpm, an.camelot, an.energy, r.cover_path, t.is_snippet \
    FROM tracks t \
    LEFT JOIN artists a ON a.id = t.artist_id \
    LEFT JOIN releases r ON r.id = t.release_id \
    LEFT JOIN artists ra ON ra.id = r.artist_id \
    LEFT JOIN analysis an ON an.track_id = t.id";

fn row_item(r: &bc_db::rusqlite::Row<'_>) -> bc_db::rusqlite::Result<QueueItem> {
    let release_id: Option<i64> = r.get(4)?;
    let cover: Option<String> = r.get(13)?;
    Ok(QueueItem {
        uid: 0,
        track_id: r.get(0)?,
        title: r.get(1)?,
        artist_id: r.get(2)?,
        artist: r.get(3)?,
        release_id,
        album: r.get(5)?,
        label_id: r.get(6)?,
        track_no: r.get(7)?,
        duration_ms: r.get(8)?,
        loved: r.get::<_, i64>(9)? != 0,
        bpm: r.get(10)?,
        camelot: r.get(11)?,
        energy: r.get(12)?,
        art_url: match (release_id, cover) {
            (Some(id), Some(_)) => Some(format!("/api/art/release/{id}?size=thumb")),
            _ => None,
        },
        is_snippet: r.get::<_, i64>(14)? != 0,
        origin: ItemOrigin::Library,
        ..Default::default()
    })
}

fn attach_tags(c: &bc_db::rusqlite::Connection, items: &mut [QueueItem]) -> bc_db::rusqlite::Result<()> {
    let mut st = c.prepare_cached(
        "SELECT g.name FROM track_tags tt JOIN tags g ON g.id = tt.tag_id WHERE tt.track_id = ?1 ORDER BY tt.weight DESC",
    )?;
    for it in items {
        it.tags = st.query_map([it.track_id], |r| r.get::<_, String>(0))?.collect::<Result<_, _>>()?;
    }
    Ok(())
}

impl LibraryPort for DbPorts {
    fn track_item(&self, track_id: i64) -> PortResult<Option<QueueItem>> {
        Ok(self.db.read(|c| {
            use bc_db::rusqlite::OptionalExtension;
            let mut it = c
                .query_row(&format!("{ITEM_SELECT} WHERE t.id = ?1"), [track_id], row_item)
                .optional()?;
            if let Some(i) = it.as_mut() {
                attach_tags(c, std::slice::from_mut(i))?;
            }
            Ok(it)
        })?)
    }

    fn file_path(&self, track_id: i64) -> PortResult<Option<PathBuf>> {
        // WS1's resolver: first live file, validated to lie under an enabled library root
        Ok(self
            .db
            .read(|c| Ok(bc_libcore::resolve_track_file(c, track_id).ok().map(|f| f.path)))?)
    }

    fn facts(&self, track_id: i64) -> PortResult<Option<TrackFacts>> {
        Ok(self.db.read(|c| {
            use bc_db::rusqlite::OptionalExtension;
            let mut f = c
                .query_row(
                    "SELECT an.bpm, an.bpm_confidence, an.beat_offset_ms, an.loudness_lufs, an.true_peak_db, t.duration_ms \
                     FROM tracks t LEFT JOIN analysis an ON an.track_id = t.id WHERE t.id = ?1",
                    [track_id],
                    |r| {
                        Ok(TrackFacts {
                            bpm: r.get(0)?,
                            bpm_confidence: r.get(1)?,
                            beat_offset_ms: r.get(2)?,
                            loudness_lufs: r.get(3)?,
                            true_peak_dbtp: r.get(4)?,
                            duration_ms: r.get(5)?,
                            ..Default::default()
                        })
                    },
                )
                .optional()?;
            if let Some(f) = f.as_mut() {
                // v2 analysis (WS3): stored beat grid and auto cues. Tables may not exist yet; ignore.
                if let Ok(Some(json)) = c.query_row("SELECT grid FROM beat_grids WHERE track_id = ?1", [track_id], |r| r.get::<_, String>(0)).optional()
                    && let Ok(g) = serde_json::from_str::<bc_types::analysis::BeatGrid>(&json)
                        && let Some(seg) = g.segments.first() {
                            f.grid_origin_s = Some(seg.origin_ms / 1000.0);
                            f.bpm = f.bpm.or(Some(seg.bpm));
                            f.downbeat_beat = g.downbeat_phase.map(|d| d as u32);
                            f.bpm_confidence = Some(g.confidence);
                            f.bpm = Some(seg.bpm);
                        }
                if let Ok(mut st) = c.prepare("SELECT kind, pos_ms FROM cue_points WHERE track_id = ?1 AND kind IN ('mix_in','mix_out')")
                    && let Ok(rows) = st.query_map([track_id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?))) {
                        for (k, pos) in rows.flatten() {
                            match k.as_str() {
                                "mix_in" => f.mix_in_ms = Some(pos as i64),
                                _ => f.mix_out_ms = Some(pos as i64),
                            }
                        }
                    }
            }
            Ok(f)
        })?)
    }

    fn record_play(&self, track_id: i64, ms_played: i64, completed: bool, skipped: bool) -> PortResult<()> {
        bc_library::history::record_play_db(&self.db, track_id, ms_played, completed, skipped)
            .map_err(|e| PortError::Other(e.to_string()))
    }

    fn set_plan(&self, set_id: i64) -> PortResult<Vec<SetSlot>> {
        // WS1/bc-plist owns the set's shape (order, cues, tempo/key-lock adjustments, planned blends)
        let detail = self.db.read(|c| bc_plist::sets::set_detail(c, set_id).map_err(|e| bc_db::DbError::Other(e.to_string())));
        let detail = detail.map_err(|e| PortError::Other(e.to_string()))?;
        let mut out = Vec::with_capacity(detail.items.len());
        for it in detail.items {
            let Some(tid) = it.track_id else { continue };
            if it.missing {
                continue;
            }
            let Some(item) = self.track_item(tid)? else { continue };
            let eff = it.effective_bpm.or(it.bpm.map(|b| b * (1.0 + it.tempo_adjust_pct / 100.0)));
            let blend_in_ms = match (it.transition_beats, eff) {
                (Some(b), Some(bpm)) if b > 0 && bpm > 0.0 => Some((b as f64 * 60_000.0 / bpm) as i64),
                _ => None,
            };
            out.push(SetSlot {
                item,
                cue_in_ms: it.cue_in_ms,
                cue_out_ms: it.cue_out_ms,
                tempo_adjust_pct: it.tempo_adjust_pct,
                key_lock: it.key_lock,
                blend_in_ms,
            });
        }
        Ok(out)
    }

    fn playlist_items(&self, playlist_id: i64) -> PortResult<Vec<QueueItem>> {
        Ok(self.db.read(|c| {
            let sql = format!(
                "{ITEM_SELECT} JOIN playlist_items pi ON pi.track_id = t.id WHERE pi.playlist_id = ?1 ORDER BY pi.position, pi.id"
            );
            let mut st = c.prepare(&sql)?;
            let mut items: Vec<QueueItem> = st.query_map([playlist_id], row_item)?.collect::<Result<_, _>>()?;
            attach_tags(c, &mut items)?;
            Ok(items)
        })?)
    }

    fn random_tracks(&self, pool: Option<&Pool>, limit: usize) -> PortResult<Vec<QueueItem>> {
        if let Some(Pool::Playlist { id, .. }) = pool {
            let mut items = self.playlist_items(*id)?;
            shuffle(&mut items);
            items.truncate(limit);
            return Ok(items);
        }
        let loved = matches!(pool, Some(Pool::Loved));
        Ok(self.db.read(|c| {
            // playable library tracks only (a file on disk), random
            let sql = format!(
                "{ITEM_SELECT} WHERE t.is_snippet = 0 {} AND EXISTS (SELECT 1 FROM files f WHERE f.track_id = t.id AND f.missing_since IS NULL) \
                 ORDER BY ((t.id * 2654435761 + ?1) % 4294967296) LIMIT ?2",
                if loved { "AND t.loved = 1" } else { "" }
            );
            let seed = (std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(7)
                % 1_000_000_007) as i64;
            let mut st = c.prepare(&sql)?;
            let mut items: Vec<QueueItem> = st.query_map(bc_db::rusqlite::params![seed, limit as i64], row_item)?.collect::<Result<_, _>>()?;
            attach_tags(c, &mut items)?;
            Ok(items)
        })?)
    }

    fn next_release(&self, after: i64, _listing: &serde_json::Value) -> PortResult<Option<i64>> {
        // TODO(ws1): honour the listing filter/order; the shim walks release ids.
        Ok(self.db.read(|c| {
            use bc_db::rusqlite::OptionalExtension;
            Ok(c.query_row("SELECT id FROM releases WHERE id > ?1 ORDER BY id LIMIT 1", [after], |r| r.get(0)).optional()?)
        })?)
    }

    fn release_tracks(&self, release_id: i64) -> PortResult<Vec<QueueItem>> {
        Ok(self.db.read(|c| {
            let sql = format!("{ITEM_SELECT} WHERE t.release_id = ?1 AND t.is_snippet = 0 ORDER BY t.disc_no, t.track_no, t.id");
            let mut st = c.prepare(&sql)?;
            let mut items: Vec<QueueItem> = st.query_map([release_id], row_item)?.collect::<Result<_, _>>()?;
            items.retain(|_| true);
            attach_tags(c, &mut items)?;
            Ok(items)
        })?)
    }

    fn next_label(&self, after: i64, _listing: &serde_json::Value) -> PortResult<Option<i64>> {
        Ok(self.db.read(|c| {
            use bc_db::rusqlite::OptionalExtension;
            Ok(c.query_row("SELECT id FROM labels WHERE id > ?1 ORDER BY id LIMIT 1", [after], |r| r.get(0)).optional()?)
        })?)
    }

    fn random_label(&self, _listing: &serde_json::Value, exclude: i64) -> PortResult<Option<i64>> {
        Ok(self.db.read(|c| {
            use bc_db::rusqlite::OptionalExtension;
            Ok(c.query_row(
                "SELECT id FROM labels WHERE id != ?1 ORDER BY random() LIMIT 1",
                [exclude],
                |r| r.get(0),
            )
            .optional()?)
        })?)
    }

    fn label_tracks(&self, label_id: i64, mode: LabelMode) -> PortResult<Vec<QueueItem>> {
        Ok(self.db.read(|c| {
            let order = if mode == LabelMode::Shuffle { "random()" } else { "r.title, t.disc_no, t.track_no" };
            let sql = format!("{ITEM_SELECT} WHERE r.label_id = ?1 AND t.is_snippet = 0 ORDER BY {order} LIMIT 500");
            let mut st = c.prepare(&sql)?;
            let mut items: Vec<QueueItem> = st.query_map([label_id], row_item)?.collect::<Result<_, _>>()?;
            attach_tags(c, &mut items)?;
            Ok(items)
        })?)
    }

    fn shuffle_labels(&self, _listing: &serde_json::Value, limit: usize) -> PortResult<Vec<QueueItem>> {
        Ok(self.db.read(|c| {
            let sql = format!("{ITEM_SELECT} WHERE r.label_id IS NOT NULL AND t.is_snippet = 0 ORDER BY random() LIMIT ?1");
            let mut st = c.prepare(&sql)?;
            let mut items: Vec<QueueItem> = st.query_map([limit as i64], row_item)?.collect::<Result<_, _>>()?;
            attach_tags(c, &mut items)?;
            Ok(items)
        })?)
    }
}

impl BandcampPort for DbPorts {
    fn resolve_stream(&self, item: &QueueItem) -> PortResult<String> {
        // TODO(ws2): resolve through the Bandcamp client; the shim trusts `stream_url`.
        item.stream_url.clone().ok_or_else(|| PortError::Unavailable("no stream url".into()))
    }
    fn fan_next(&self, _c: &FanCursor, _after: Option<i64>, _limit: usize) -> PortResult<FanPage> {
        Err(PortError::Unavailable("fan walks need the Bandcamp service (ws2)".into()))
    }
    fn release_tracks(&self, _url: &str) -> PortResult<Vec<QueueItem>> {
        Err(PortError::Unavailable("explore releases need the Bandcamp service (ws2)".into()))
    }
}

impl StatePort for DbPorts {
    /// WS1's `ui_state` store; a value an earlier build left in `settings` is still read (and
    /// moves over on the next write).
    fn get(&self, key: &str) -> Option<String> {
        let k = key.to_string();
        self.db
            .read(move |c| Ok(match bc_db::ui_state::get(c, &k)? {
                Some(v) => Some(v),
                None => bc_db::settings::get(c, &k)?,
            }))
            .ok()
            .flatten()
    }
    fn set(&self, key: &str, value: &str) {
        let (k, v) = (key.to_string(), value.to_string());
        if let Err(e) = self.db.write(move |t| bc_db::ui_state::set(t, &k, &v)) {
            tracing::warn!("persisting player state failed: {e}");
        }
    }
}

/// An ad-hoc library of files (the `play` example, tests): track `i + 1` is `files[i]`.
pub struct FilePorts {
    pub files: Vec<PathBuf>,
    /// Tempo for every file (and a beat grid at offset 0) when given.
    pub bpm: Option<f64>,
    pub duration_ms: Vec<Option<i64>>,
}

impl FilePorts {
    pub fn new(files: Vec<PathBuf>) -> Self {
        let n = files.len();
        Self { files, bpm: None, duration_ms: vec![None; n] }
    }
    pub fn into_ports(self) -> Ports {
        let a = Arc::new(self);
        Ports { library: a.clone(), bandcamp: a.clone(), recommend: a.clone(), state: Arc::new(MemState::default()), base_url: String::new() }
    }
    pub fn item(&self, id: i64) -> Option<QueueItem> {
        let p = self.files.get((id - 1) as usize)?;
        Some(QueueItem {
            track_id: id,
            title: p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default(),
            duration_ms: self.duration_ms.get((id - 1) as usize).copied().flatten(),
            bpm: self.bpm,
            origin: ItemOrigin::Library,
            ..Default::default()
        })
    }
}

/// An in-memory [`StatePort`].
#[derive(Default)]
pub struct MemState(parking_lot::Mutex<std::collections::HashMap<String, String>>);

impl StatePort for MemState {
    fn get(&self, key: &str) -> Option<String> {
        self.0.lock().get(key).cloned()
    }
    fn set(&self, key: &str, value: &str) {
        self.0.lock().insert(key.to_string(), value.to_string());
    }
}

impl LibraryPort for FilePorts {
    fn track_item(&self, id: i64) -> PortResult<Option<QueueItem>> {
        Ok(self.item(id))
    }
    fn file_path(&self, id: i64) -> PortResult<Option<PathBuf>> {
        Ok(self.files.get((id - 1) as usize).cloned())
    }
    fn facts(&self, id: i64) -> PortResult<Option<TrackFacts>> {
        Ok(self.item(id).map(|i| TrackFacts {
            bpm: self.bpm,
            bpm_confidence: self.bpm.map(|_| 0.9),
            beat_offset_ms: self.bpm.map(|_| 0.0),
            duration_ms: i.duration_ms,
            ..Default::default()
        }))
    }
    fn record_play(&self, _: i64, _: i64, _: bool, _: bool) -> PortResult<()> {
        Ok(())
    }
    fn set_plan(&self, _: i64) -> PortResult<Vec<SetSlot>> {
        // a canned plan for tests/examples: enter at 2 s, leave 3 s before the end, 4 s blends, the
        // second slot sped up 4 % with key lock
        Ok((1..=self.files.len() as i64)
            .filter_map(|i| self.item(i))
            .enumerate()
            .map(|(k, item)| {
                let out = item.duration_ms.map(|d| d - 3000);
                SetSlot { item, cue_in_ms: Some(2000), cue_out_ms: out, tempo_adjust_pct: if k == 1 { 4.0 } else { 0.0 }, key_lock: true, blend_in_ms: (k > 0).then_some(4000) }
            })
            .collect())
    }
    fn playlist_items(&self, _: i64) -> PortResult<Vec<QueueItem>> {
        Ok((1..=self.files.len() as i64).filter_map(|i| self.item(i)).collect())
    }
    fn random_tracks(&self, _: Option<&Pool>, limit: usize) -> PortResult<Vec<QueueItem>> {
        let mut v: Vec<QueueItem> = (1..=self.files.len() as i64).filter_map(|i| self.item(i)).collect();
        Rng::seeded().shuffle(&mut v);
        v.truncate(limit);
        Ok(v)
    }
    fn next_release(&self, _: i64, _: &serde_json::Value) -> PortResult<Option<i64>> {
        Ok(None)
    }
    fn release_tracks(&self, _: i64) -> PortResult<Vec<QueueItem>> {
        Ok(vec![])
    }
    fn next_label(&self, _: i64, _: &serde_json::Value) -> PortResult<Option<i64>> {
        Ok(None)
    }
    fn random_label(&self, _: &serde_json::Value, _: i64) -> PortResult<Option<i64>> {
        Ok(None)
    }
    fn label_tracks(&self, _: i64, _: LabelMode) -> PortResult<Vec<QueueItem>> {
        Ok(vec![])
    }
    fn shuffle_labels(&self, _: &serde_json::Value, _: usize) -> PortResult<Vec<QueueItem>> {
        Ok(vec![])
    }
}

impl BandcampPort for FilePorts {
    fn resolve_stream(&self, _: &QueueItem) -> PortResult<String> {
        Err(PortError::Unavailable("files only".into()))
    }
    fn fan_next(&self, _: &FanCursor, _: Option<i64>, _: usize) -> PortResult<FanPage> {
        Err(PortError::Unavailable("files only".into()))
    }
    fn release_tracks(&self, _: &str) -> PortResult<Vec<QueueItem>> {
        Err(PortError::Unavailable("files only".into()))
    }
}

impl RecommendPort for FilePorts {
    /// Anything not excluded, in file order: enough to keep a file-only set going.
    fn next_up(&self, req: &NextUpRequest) -> PortResult<Vec<QueueItem>> {
        let ex: std::collections::HashSet<i64> = req.exclude_track_ids.iter().copied().collect();
        Ok((1..=self.files.len() as i64)
            .filter(|i| !ex.contains(i))
            .filter_map(|i| self.item(i))
            .take(req.limit.max(1) as usize)
            .collect())
    }
}

fn shuffle<T>(v: &mut [T]) {
    let mut rng = Rng::seeded();
    for i in (1..v.len()).rev() {
        let j = rng.below(i + 1);
        v.swap(i, j);
    }
}

/// Tiny xorshift RNG (no `rand` dependency); seeded from the clock.
pub struct Rng(u64);

impl Rng {
    pub fn seeded() -> Self {
        let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(1);
        Rng(n | 1)
    }
    pub fn with_seed(s: u64) -> Self {
        Rng(s | 1)
    }
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }
    pub fn below(&mut self, n: usize) -> usize {
        if n <= 1 { 0 } else { (self.next_u64() % n as u64) as usize }
    }
    pub fn shuffle<T>(&mut self, v: &mut [T]) {
        for i in (1..v.len()).rev() {
            let j = self.below(i + 1);
            v.swap(i, j);
        }
    }
}

// ---------------------------------------------------------------------------
// next-up: WS3's in-process recommender
// ---------------------------------------------------------------------------

fn brief_to_item(t: bc_types::library::TrackOut) -> QueueItem {
    QueueItem {
        uid: 0,
        track_id: t.id,
        title: t.title,
        artist_id: t.artist.as_ref().map(|a| a.id),
        artist: t.artist.map(|a| a.name),
        release_id: t.release.as_ref().map(|r| r.id),
        album: t.release.map(|r| r.title),
        track_no: t.track_no.map(|n| n as i32),
        duration_ms: t.duration_ms,
        bpm: t.bpm,
        camelot: t.camelot,
        energy: t.energy,
        tags: t.tags,
        loved: t.loved,
        art_url: t.art_url,
        origin: ItemOrigin::Library,
        is_snippet: t.is_snippet,
        ..Default::default()
    }
}

impl RecommendPort for DbPorts {
    /// `bc_recommend::nextup::suggest`, in process, over the user's own library.
    fn next_up(&self, req: &NextUpRequest) -> PortResult<Vec<QueueItem>> {
        let scope = { use bc_recommend::scope::ScopeExt; bc_recommend::scope::Scope::mine().without_snippets() };
        let resp = bc_recommend::nextup::suggest(&self.db, &scope, req).map_err(|e| PortError::Other(e.to_string()))?;
        Ok(resp.items.into_iter().map(|s| brief_to_item(s.track)).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rng_shuffle_is_a_permutation() {
        let mut v: Vec<u32> = (0..50).collect();
        Rng::with_seed(42).shuffle(&mut v);
        let mut s = v.clone();
        s.sort();
        assert_eq!(s, (0..50).collect::<Vec<_>>());
        assert_ne!(v, (0..50).collect::<Vec<_>>());
    }
}
