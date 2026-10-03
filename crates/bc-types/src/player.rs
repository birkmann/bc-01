//! Player DTOs (workstream 4): queue, state, clock, commands, mix settings,
//! transitions, planner state and output devices.
//!
//! Everything here is plain serde data so it compiles for wasm32 and is shared
//! by `bc-engine` (the session), `bc-server` (WS hub / routes) and the Leptos UI.
//!
//! Topics: [`TOPIC_PLAYER_STATE`] (full [`PlayerState`] on change),
//! [`TOPIC_PLAYER_CLOCK`] ([`Clock`] at about 30 Hz while playing),
//! [`TOPIC_PLAYER_TRANSITION`] (`Option<TransitionState>` on change).

use serde::{Deserialize, Serialize};

// The planner's direction vocabulary is WS3's (suggest-next) one, shared as is.
pub use crate::suggest::{EnergyDir, Harmonic, TagMode, Tempo};

pub const TOPIC_PLAYER_STATE: &str = "player.state";
pub const TOPIC_PLAYER_CLOCK: &str = "player.clock";
pub const TOPIC_PLAYER_TRANSITION: &str = "player.transition";

// ---------------------------------------------------------------------------
// Basic enums
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PlayerStatus {
    #[default]
    Idle,
    Loading,
    Playing,
    Paused,
    Error,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RepeatMode {
    #[default]
    Off,
    All,
    One,
}

/// How one track hands over to the next (PLAN §5.1 "transitions are now real").
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum TransitionKind {
    /// Equal-power crossfade (the browser player's blend).
    #[default]
    Blend,
    /// Low-EQ exchange on a downbeat.
    BassSwap,
    /// High-pass sweep out, low-pass sweep in.
    Filter,
    /// Outgoing is thrown into the echo while the incoming comes in quickly.
    EchoOut,
    /// Hard cut quantised to a downbeat.
    Cut,
    /// A DJ's three-band EQ blend: the incoming comes in with its bass cut, the
    /// basslines swap on a bar line, then the outgoing's mids and highs leave.
    EqBlend,
}

impl TransitionKind {
    pub fn label(self) -> &'static str {
        match self {
            TransitionKind::Blend => "crossfade",
            TransitionKind::BassSwap => "bass swap",
            TransitionKind::Filter => "filter",
            TransitionKind::EchoOut => "echo out",
            TransitionKind::Cut => "cut",
            TransitionKind::EqBlend => "blend",
        }
    }
}

/// What the incoming deck's start is aligned to on the outgoing deck's grid.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Quantise {
    /// Start immediately.
    Off,
    /// Next beat.
    Beat,
    /// Next bar (4 beats).
    #[default]
    Bar,
    /// Next phrase (16 beats).
    Phrase,
}

/// Where the next track comes in (`entry` in the browser player).
/// Key-lock quality: the phase vocoder's frame size (1024 / 2048 / 4096), trading CPU and latency
/// for fewer smearing artefacts. Takes effect for tracks loaded from then on.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum KeyLockQuality {
    Fast,
    #[default]
    Balanced,
    High,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Entry {
    /// At its mix-in point, where the main sounds start.
    #[default]
    Drop,
    /// From its intro, run under the outgoing's outro.
    Intro,
}

// ---------------------------------------------------------------------------
// Queue
// ---------------------------------------------------------------------------

/// Where a queue item's audio comes from.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ItemOrigin {
    /// A library file; resolved by `track_id`.
    #[default]
    Library,
    /// A Bandcamp stream (`stream_url`, resolved through `page_url`).
    Bandcamp,
}

/// One row of the play queue. A denormalised snapshot so the UI never has to
/// join; the session owns the authoritative copy.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct QueueItem {
    /// Unique per queue entry within a session (assigned by the session when 0).
    #[serde(default)]
    pub uid: u64,
    /// Library id (> 0) or a synthetic negative id for a Bandcamp stream.
    pub track_id: i64,
    pub title: String,
    #[serde(default)]
    pub artist: Option<String>,
    #[serde(default)]
    pub artist_id: Option<i64>,
    #[serde(default)]
    pub album: Option<String>,
    #[serde(default)]
    pub release_id: Option<i64>,
    #[serde(default)]
    pub label_id: Option<i64>,
    #[serde(default)]
    pub track_no: Option<i32>,
    #[serde(default)]
    pub duration_ms: Option<i64>,
    #[serde(default)]
    pub bpm: Option<f64>,
    #[serde(default)]
    pub camelot: Option<String>,
    #[serde(default)]
    pub energy: Option<f64>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub loved: bool,
    #[serde(default)]
    pub art_url: Option<String>,
    /// Playable URL for a Bandcamp stream (absolute, or relative to the server).
    #[serde(default)]
    pub stream_url: Option<String>,
    #[serde(default)]
    pub origin: ItemOrigin,
    /// The Bandcamp release page a streamed track belongs to.
    #[serde(default)]
    pub page_url: Option<String>,
    /// The streamed track's own Bandcamp page, when known: lets it be downloaded on its own.
    #[serde(default)]
    pub track_url: Option<String>,
    /// Playlist row id (a playlist can hold a track twice).
    #[serde(default)]
    pub item_id: Option<i64>,
    #[serde(default)]
    pub is_snippet: bool,
}

impl QueueItem {
    pub fn is_library(&self) -> bool {
        self.origin == ItemOrigin::Library && self.track_id > 0
    }
    pub fn duration_s(&self) -> Option<f64> {
        self.duration_ms.map(|d| d as f64 / 1000.0)
    }
}

/// The wishlist cursor (`FanCursor` of the browser player).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FanCursor {
    pub fan_id: i64,
    /// The last item played; `None` before the first.
    #[serde(default)]
    pub item_id: Option<i64>,
    pub order: FanOrder,
    pub seed: i64,
    #[serde(default)]
    pub states: Vec<String>,
    #[serde(default)]
    pub tab: Option<FanTab>,
    #[serde(default)]
    pub fan_name: String,
    /// The folder their downloads go under.
    #[serde(default)]
    pub shelf: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FanOrder {
    Seq,
    Shuffle,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FanTab {
    Wishlist,
    Collection,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LabelMode {
    /// Album order through the folder.
    #[default]
    All,
    /// A random draw from the folder.
    Shuffle,
}

/// One Bandcamp release card of an explore grid (only a URL is needed).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExploreCard {
    pub url: String,
    #[serde(default)]
    pub library_release_id: Option<i64>,
}

/// Where a queue came from, when playback can carry on into something past its
/// end. `Playlist` and `Set` are named origins that deliberately *stop* where
/// the queue stops (`continues()` is false), like in the browser player.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum QueueSource {
    /// An album. `listing` is the filter/order of the listing it was played from.
    Release {
        release_id: i64,
        #[serde(default)]
        listing: serde_json::Value,
    },
    /// One label's catalogue.
    Label {
        label_id: i64,
        #[serde(default)]
        listing: serde_json::Value,
        #[serde(default)]
        mode: LabelMode,
    },
    /// A shuffle of the whole labels shelf (deals again when played out).
    Labels {
        #[serde(default)]
        listing: serde_json::Value,
    },
    /// A wishlist / collection of a fan, with a server-side cursor.
    Fan(FanCursor),
    /// A grid of Bandcamp releases swept on demand (keeps ~12 tracks ahead).
    Explore {
        cards: Vec<ExploreCard>,
        shuffle: bool,
        /// Index of the next card to fetch.
        #[serde(default)]
        next: usize,
    },
    Playlist {
        id: i64,
        name: String,
    },
    Set {
        id: i64,
        name: String,
    },
}

impl QueueSource {
    /// Whether playback carries on past the queue's end.
    pub fn continues(&self) -> bool {
        !matches!(self, QueueSource::Playlist { .. } | QueueSource::Set { .. })
    }
}

// ---------------------------------------------------------------------------
// Mix settings
// ---------------------------------------------------------------------------

/// DJ-mix settings; the first block is `MixSettings` of `beatmatch.ts`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct MixSettings {
    /// Echo the outgoing track out over the blend's last seconds.
    pub echo: bool,
    /// Match the incoming tempo to the outgoing one when they are close.
    pub sync: bool,
    /// Keep the incoming track's pitch when its tempo is changed.
    pub key_lock: bool,
    /// How long a matched blend runs, in beats of the outgoing track (16, 32, 64, 128).
    pub length_beats: u32,
    /// Stay at the matched tempo afterwards instead of gliding back to 1.0.
    pub hold_tempo: bool,
    /// Seconds the incoming's tempo takes to glide back to its own after a matched blend.
    pub glide_back_s: f64,
    pub entry: Entry,
    /// Seconds a track plays before the next is brought in; `None` = to its outro.
    pub max_play_s: Option<f64>,
    pub show_transition_bar: bool,
    pub show_mix_out_marker: bool,
    // --- upgrades over the browser player ---
    /// Which transition to use for a matched/long blend.
    pub transition: TransitionKind,
    /// Align the incoming start to the outgoing grid.
    pub quantise: Quantise,
    /// Continuous phase-lock loop after the initial bend.
    pub phase_lock: bool,
    /// Live loudness normalisation (trim from analysis loudness).
    pub normalise: bool,
    pub target_lufs: f64,
    /// Phase-vocoder size for key lock.
    #[serde(default)]
    pub key_lock_quality: KeyLockQuality,
    /// Settings revision; older stored settings are brought forward by [`MixSettings::migrate`].
    /// Absent in settings stored before revisions existed, hence the field-level default of 0.
    #[serde(default)]
    pub rev: u32,
}

/// The current [`MixSettings::rev`].
pub const MIX_SETTINGS_REV: u32 = 1;

impl Default for MixSettings {
    fn default() -> Self {
        Self {
            echo: true,
            sync: true,
            key_lock: false,
            length_beats: 64,
            hold_tempo: false,
            glide_back_s: 30.0,
            entry: Entry::Drop,
            max_play_s: None,
            show_transition_bar: true,
            show_mix_out_marker: true,
            transition: TransitionKind::EqBlend,
            quantise: Quantise::Phrase,
            phase_lock: true,
            normalise: true,
            target_lufs: -14.0,
            key_lock_quality: KeyLockQuality::Balanced,
            rev: MIX_SETTINGS_REV,
        }
    }
}

/// Partial update of [`MixSettings`]. `max_play_s` uses a double option so
/// `null` can clear it (`{"max_play_s": null}` clears, an absent key keeps).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct MixSettingsPatch {
    pub echo: Option<bool>,
    pub sync: Option<bool>,
    pub key_lock: Option<bool>,
    pub length_beats: Option<u32>,
    pub hold_tempo: Option<bool>,
    pub glide_back_s: Option<f64>,
    pub entry: Option<Entry>,
    #[serde(default, deserialize_with = "double_option")]
    pub max_play_s: Option<Option<f64>>,
    pub show_transition_bar: Option<bool>,
    pub show_mix_out_marker: Option<bool>,
    pub transition: Option<TransitionKind>,
    pub quantise: Option<Quantise>,
    pub phase_lock: Option<bool>,
    pub normalise: Option<bool>,
    pub target_lufs: Option<f64>,
    pub key_lock_quality: Option<KeyLockQuality>,
}

fn double_option<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}

impl MixSettings {
    pub fn apply(&mut self, p: &MixSettingsPatch) {
        macro_rules! set {
            ($f:ident) => {
                if let Some(v) = p.$f.clone() {
                    self.$f = v;
                }
            };
        }
        set!(echo);
        set!(sync);
        set!(key_lock);
        set!(hold_tempo);
        set!(entry);
        set!(show_transition_bar);
        set!(show_mix_out_marker);
        set!(transition);
        set!(quantise);
        set!(phase_lock);
        set!(normalise);
        set!(target_lufs);
        set!(key_lock_quality);
        if let Some(n) = p.length_beats {
            // 16 | 32 | 64 | 128 only: whole phrases.
            self.length_beats = match n {
                0..=24 => 16,
                25..=48 => 32,
                49..=96 => 64,
                _ => 128,
            };
        }
        if let Some(g) = p.glide_back_s.filter(|g| g.is_finite()) {
            self.glide_back_s = g.clamp(2.0, 300.0);
        }
        if let Some(v) = p.max_play_s {
            self.max_play_s = v;
        }
    }
}

impl MixSettings {
    /// Bring settings stored by an older version forward. Revision 0 had no
    /// settings UI, so its transition, length and quantise are the old defaults:
    /// they move to the DJ blend, 64 beats and phrase alignment.
    pub fn migrate(&mut self) {
        if self.rev < 1 {
            if self.transition == TransitionKind::Blend {
                self.transition = TransitionKind::EqBlend;
            }
            if self.length_beats == 32 {
                self.length_beats = 64;
            }
            if self.quantise == Quantise::Bar {
                self.quantise = Quantise::Phrase;
            }
        }
        self.rev = MIX_SETTINGS_REV;
    }
}

/// The pace choices offered, in seconds (`None` = the whole track).
pub const MAX_PLAY_CHOICES: [Option<f64>; 7] =
    [None, Some(120.0), Some(180.0), Some(240.0), Some(300.0), Some(360.0), Some(480.0)];

// ---------------------------------------------------------------------------
// Master strip: 3-band EQ + one-knob filter (user controls)
// ---------------------------------------------------------------------------

/// The user-facing channel strip on the master bus.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Strip {
    pub low_db: f64,
    pub mid_db: f64,
    pub high_db: f64,
    pub kill_low: bool,
    pub kill_mid: bool,
    pub kill_high: bool,
    /// -1 (full low-pass) .. 0 (off) .. +1 (full high-pass).
    pub filter: f64,
    /// Master echo send level (0..1) for manual echo throws.
    pub echo_send: f64,
}

impl Default for Strip {
    fn default() -> Self {
        Self {
            low_db: 0.0,
            mid_db: 0.0,
            high_db: 0.0,
            kill_low: false,
            kill_mid: false,
            kill_high: false,
            filter: 0.0,
            echo_send: 0.0,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StripPatch {
    pub low_db: Option<f64>,
    pub mid_db: Option<f64>,
    pub high_db: Option<f64>,
    pub kill_low: Option<bool>,
    pub kill_mid: Option<bool>,
    pub kill_high: Option<bool>,
    pub filter: Option<f64>,
    pub echo_send: Option<f64>,
}

impl Strip {
    pub fn apply(&mut self, p: &StripPatch) {
        if let Some(v) = p.low_db {
            self.low_db = v.clamp(-24.0, 12.0);
        }
        if let Some(v) = p.mid_db {
            self.mid_db = v.clamp(-24.0, 12.0);
        }
        if let Some(v) = p.high_db {
            self.high_db = v.clamp(-24.0, 12.0);
        }
        if let Some(v) = p.kill_low {
            self.kill_low = v;
        }
        if let Some(v) = p.kill_mid {
            self.kill_mid = v;
        }
        if let Some(v) = p.kill_high {
            self.kill_high = v;
        }
        if let Some(v) = p.filter {
            self.filter = v.clamp(-1.0, 1.0);
        }
        if let Some(v) = p.echo_send {
            self.echo_send = v.clamp(0.0, 1.0);
        }
    }
}

// ---------------------------------------------------------------------------
// Transition (the strip above the player bar)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TransitionSync {
    pub rate: f64,
    pub from_bpm: f64,
    pub to_bpm: f64,
    pub on: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PhaseState {
    /// Bent onto the outgoing grid once (grids are extrapolated, so approximate).
    Est,
    /// The continuous phase-lock loop is tracking.
    Locked,
}

/// The blend that is running right now. Times are wall-clock unix ms.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TransitionState {
    pub kind: TransitionKind,
    pub started_at_ms: u64,
    pub ends_at_ms: u64,
    pub echo: bool,
    pub sync: Option<TransitionSync>,
    pub phase: Option<PhaseState>,
    pub outgoing_uid: Option<u64>,
    pub incoming_uid: Option<u64>,
    /// Residual beat phase error of the incoming against the outgoing, ms.
    pub phase_error_ms: Option<f64>,
}

// ---------------------------------------------------------------------------
// Clock
// ---------------------------------------------------------------------------

/// Published at ~30 Hz while playing. Clients extrapolate the playhead as
/// `position_s + (now - output_timestamp_ns) * rate` while `playing`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Clock {
    /// Frames the engine has produced since it started (monotonic).
    pub frames_played: u64,
    pub sample_rate: u32,
    /// Unix ns at which the frame `frames_played` leaves the device (0 = unknown).
    pub output_timestamp_ns: u64,
    /// Unix ns when this clock frame was sampled on the server.
    pub server_time_ns: u64,
    /// Playback rate of the playing deck (1 = the file's own tempo).
    pub rate: f64,
    pub playing: bool,
    /// Position in the current track at `output_timestamp_ns`, seconds.
    pub position_s: f64,
    pub duration_s: f64,
    /// Seconds of the current track decoded/buffered ahead of the playhead.
    pub buffered_s: f64,
    pub track_uid: Option<u64>,
    /// Where the track will be moved on (outro, pace limit or dragged point).
    pub mix_out_s: Option<f64>,
    pub peak_l: f32,
    pub peak_r: f32,
    /// Audio callback underruns / late buffers since start.
    pub xruns: u64,
}

// ---------------------------------------------------------------------------
// Planner (live set planner, planStore.ts)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Wish {
    Track { id: i64, title: String, artist: String },
    Artist { id: i64, name: String },
    Label { id: i64, name: String },
    Tag { name: String },
}

impl Wish {
    pub fn key(&self) -> String {
        match self {
            Wish::Tag { name } => format!("tag:{}", name.to_lowercase()),
            Wish::Track { id, .. } => format!("track:{id}"),
            Wish::Artist { id, .. } => format!("artist:{id}"),
            Wish::Label { id, .. } => format!("label:{id}"),
        }
    }
}

/// Where suggestions and auto-fill draw from. Pools chain: the first is mixed;
/// when it has nothing left the next takes over.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Pool {
    Library,
    Loved,
    Playlist { id: i64, name: String },
}

impl Pool {
    pub fn key(&self) -> String {
        match self {
            Pool::Library => "library".into(),
            Pool::Loved => "loved".into(),
            Pool::Playlist { id, .. } => format!("playlist:{id}"),
        }
    }
}

/// Hard rules on what may be suggested: with `allow` set only tracks carrying
/// one of those tags come up; a track carrying any of `deny` never does.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct TagRules {
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub deny: Vec<String>,
}

impl TagRules {
    pub fn is_empty(&self) -> bool {
        self.allow.is_empty() && self.deny.is_empty()
    }
    /// Does a track with these tags break the rules? (mirror of `breaksRules`).
    pub fn breaks(&self, tags: &[String]) -> bool {
        if self.is_empty() {
            return false;
        }
        let have: Vec<String> = tags.iter().map(|t| t.trim().to_lowercase()).collect();
        let has = |t: &String| have.contains(&t.trim().to_lowercase());
        if self.deny.iter().any(has) {
            return true;
        }
        !self.allow.is_empty() && !self.allow.iter().any(has)
    }
    fn key(&self) -> String {
        let norm = |l: &[String]| {
            let mut v: Vec<String> = l.iter().map(|t| t.trim().to_lowercase()).collect();
            v.sort();
            v.join("\u{1}")
        };
        format!("{}\u{2}{}", norm(&self.allow), norm(&self.deny))
    }
    pub fn same_as(&self, other: &TagRules) -> bool {
        self.key() == other.key()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TagRulePreset {
    pub id: String,
    pub name: String,
    #[serde(flatten)]
    pub rules: TagRules,
}

/// How many upcoming rows the planner keeps filled and plans over.
pub const HORIZON: usize = 20;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct PlanState {
    pub tempo: Tempo,
    pub energy: EnergyDir,
    pub tag_mode: TagMode,
    pub target_tags: Vec<String>,
    pub harmonic: Harmonic,
    pub tag_rules: TagRules,
    pub tag_presets: Vec<TagRulePreset>,
    pub active_preset_id: Option<String>,
    pub wishes: Vec<Wish>,
    /// Keep the queue topped up with suggestions while DJ mix is on.
    pub auto_fill: bool,
    /// The chain of pools; the first is active. Empty = library.
    pub pools: Vec<Pool>,
    /// Key of a pool auto-fill found nothing more in (session-only hint).
    pub spent_pool: Option<String>,
    /// Set length in minutes; `None` = not planning a set end.
    pub set_length_min: Option<f64>,
    /// Unix ms the set clock was started; `None` = not running.
    pub set_started_at_ms: Option<u64>,
}

impl Default for PlanState {
    fn default() -> Self {
        Self {
            tempo: Tempo::Keep,
            energy: EnergyDir::Keep,
            tag_mode: TagMode::Drift,
            target_tags: vec![],
            harmonic: Harmonic::Strict,
            tag_rules: TagRules::default(),
            tag_presets: vec![],
            active_preset_id: None,
            wishes: vec![],
            auto_fill: true,
            pools: vec![],
            spent_pool: None,
            set_length_min: None,
            set_started_at_ms: None,
        }
    }
}

/// Operations on the planner, one per action of the browser's `planStore`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum PlanOp {
    SetTempo { tempo: Tempo },
    SetEnergy { energy: EnergyDir },
    SetTagMode { mode: TagMode },
    SetTargetTags { tags: Vec<String> },
    ToggleTargetTag { tag: String },
    SetHarmonic { harmonic: Harmonic },
    ToggleAllowTag { tag: String },
    ToggleDenyTag { tag: String },
    SetTagRules { rules: TagRules },
    ClearTagRules,
    SavePreset { name: String },
    UpdatePreset { id: String },
    ApplyPreset { id: String },
    RenamePreset { id: String, name: String },
    DeletePreset { id: String },
    SetAutoFill { on: bool },
    /// Put a pool first (mix from it now), keeping the rest of the chain.
    MixFrom { pool: Pool },
    /// Add a pool to the end of the chain.
    ChainPool { pool: Pool },
    RemovePool { key: String },
    AdvancePool,
    SetPools { pools: Vec<Pool> },
    AddWish { wish: Wish },
    RemoveWish { key: String },
    ClearWishes,
    SetSetLength { minutes: Option<f64> },
    StartSet,
    StopSet,
    Reset,
}

// ---------------------------------------------------------------------------
// Output devices
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OutputDevice {
    /// Stable id used to select the device (`OutputTarget::device`).
    pub name: String,
    /// Human-readable name for the picker.
    pub label: String,
    pub is_default: bool,
    pub max_channels: u16,
    pub default_sample_rate: u32,
}

/// Where audio goes. `None` = system default.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct OutputTarget {
    pub device: Option<String>,
    /// Optional second device for cue / headphone pre-listen.
    pub cue_device: Option<String>,
    /// Requested buffer size in frames; `None` = device default.
    pub buffer_frames: Option<u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct DevicesInfo {
    pub outputs: Vec<OutputDevice>,
    pub target: OutputTarget,
    /// "cpal/ALSA", "null" ...
    pub backend: String,
    pub sample_rate: u32,
    pub buffer_frames: u32,
    /// Estimated output latency in ms.
    pub latency_ms: f64,
    pub xruns: u64,
    /// The cue device that is really open; `None` with a `target.cue_device` set means it failed
    /// to open and previews play through the main output.
    #[serde(default)]
    pub cue_device: Option<String>,
}

// ---------------------------------------------------------------------------
// Preview (cue deck)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PreviewState {
    pub track_id: Option<i64>,
    pub playing: bool,
    pub position_s: f64,
    /// Routed to the cue device (true) or mixed into the main output (false).
    pub on_cue_device: bool,
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// Where a queued track comes in and goes out when mixed, in track seconds. The UI planner's
/// "starts in" offsets are computed from these so they match what the engine will do.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct EntryPoint {
    pub uid: u64,
    /// The drop / mix-in point the incoming track starts at (`Entry::Drop`).
    pub drop_s: f64,
    /// Where the track starts blending out; `None` = it plays to its end.
    pub out_s: Option<f64>,
}

/// The whole player, published on `player.state` whenever something other than
/// the clock changes.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PlayerState {
    /// Monotonic revision, bumped on every publish.
    pub rev: u64,
    pub status: PlayerStatus,
    pub current: Option<QueueItem>,
    /// Bumped whenever the queue contents change.
    pub queue_rev: u64,
    /// `player.state` events carry the queue only when it changed (`true`);
    /// clients keep their last copy otherwise. `GET /player/state` always includes it.
    pub queue_included: bool,
    pub queue: Vec<QueueItem>,
    pub queue_index: i64,
    pub repeat: RepeatMode,
    pub shuffle: bool,
    /// Queue indices in the order played; `history_pos` points at the current.
    pub history: Vec<usize>,
    pub history_pos: i64,
    pub source: Option<QueueSource>,
    pub volume: f64,
    pub muted: bool,
    /// DJ mix on/off.
    pub mix: bool,
    pub mix_settings: MixSettings,
    pub strip: Strip,
    pub transition: Option<TransitionState>,
    pub plan: PlanState,
    pub mix_out_s: Option<f64>,
    pub mix_out_override_s: Option<f64>,
    /// The pace limit (`mix_settings.max_play_s`), mirrored for the planner.
    #[serde(default)]
    pub max_play_s: Option<f64>,
    /// Entry / exit points of the current track and the next 11 queue rows (empty with mix off).
    #[serde(default)]
    pub entry_points: Vec<EntryPoint>,
    pub error: Option<String>,
    pub preview: PreviewState,
    pub devices: DevicesInfo,
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// A player command. Sent as `POST /api/player/command` or over the WebSocket
/// as `{"type":"player","command":{"cmd":"next"}}` (`ClientMsg::Player`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum PlayerCommand {
    // transport
    Play,
    Pause,
    Toggle,
    Stop,
    Seek { seconds: f64 },
    SeekRelative { delta_s: f64 },
    Next,
    Previous,
    JumpTo { index: usize },
    // start playing
    PlayTrack {
        item: QueueItem,
        #[serde(default)]
        queue: Option<Vec<QueueItem>>,
    },
    PlayQueue {
        items: Vec<QueueItem>,
        #[serde(default)]
        start_index: usize,
        #[serde(default)]
        source: Option<QueueSource>,
    },
    /// Build the first queue of a source server-side and play it
    /// (fan cursor, explore sweep, release/label listing).
    StartSource {
        source: QueueSource,
        #[serde(default)]
        shuffle: bool,
    },
    // queue edits (upcoming rows only; the played chain is never touched)
    AddToQueue { items: Vec<QueueItem> },
    InsertAt { index: usize, items: Vec<QueueItem> },
    PlayNext { items: Vec<QueueItem> },
    MoveInQueue { from: usize, to: usize },
    RemoveAt { index: usize },
    RemoveRange { from: usize, to: usize },
    ReplaceAt { index: usize, item: QueueItem },
    // modes and level
    SetVolume { volume: f64 },
    SetMuted { muted: bool },
    ToggleMute,
    SetRepeat { mode: RepeatMode },
    CycleRepeat,
    SetShuffle { on: bool },
    ToggleShuffle,
    // DJ mix
    SetMix { on: bool },
    ToggleMix,
    SetMixSettings { patch: MixSettingsPatch },
    /// Finish the running blend now (0.3 s retime plus quick echo-out).
    CutNow,
    /// Finish the running blend in `factor` times the time left (0.5 faster, 2 slower).
    Retime { factor: f64 },
    SetTransitionEcho { on: bool },
    SetTransitionSync { on: bool },
    /// Push the incoming forward (+) or back (-) by seconds.
    Nudge { delta_s: f64 },
    /// Where the playing track will be moved on; `None` resets to the plan.
    SetMixOutOverride { seconds: Option<f64> },
    /// Start a blend into the next track right now (like the mix trigger).
    MixNow,
    // strip
    SetStrip { patch: StripPatch },
    // planner
    Plan { op: PlanOp },
    /// Replace the whole planner state.
    SetPlan { plan: PlanState },
    /// "DJ mix this": put the pool first, turn mix and auto-fill on.
    StartMixFrom { pool: Pool },
    // devices
    SetOutput { target: OutputTarget },
    ListDevices,
    // pre-listen
    PreviewStart {
        item: QueueItem,
        #[serde(default)]
        at_s: Option<f64>,
    },
    PreviewStop,
    // misc
    MarkLoved { track_id: i64, loved: bool },
    /// Remove the error banner.
    ClearError,
}

#[cfg(test)]
mod tests {
    #[test]
    fn key_lock_quality_patches_and_defaults() {
        let mut m = MixSettings::default();
        assert_eq!(m.key_lock_quality, KeyLockQuality::Balanced);
        let patch: MixSettingsPatch = serde_json::from_str(r#"{"key_lock_quality":"high"}"#).unwrap();
        m.apply(&patch);
        assert_eq!(m.key_lock_quality, KeyLockQuality::High);
        // an old persisted settings blob without the field still loads
        let mut v = serde_json::to_value(MixSettings::default()).unwrap();
        v.as_object_mut().unwrap().remove("key_lock_quality");
        assert_eq!(serde_json::from_value::<MixSettings>(v).unwrap().key_lock_quality, KeyLockQuality::Balanced);
    }

    use super::*;

    #[test]
    fn command_roundtrip_and_shape() {
        let c: PlayerCommand = serde_json::from_str(r#"{"cmd":"seek","seconds":12.5}"#).unwrap();
        assert_eq!(c, PlayerCommand::Seek { seconds: 12.5 });
        let c: PlayerCommand = serde_json::from_str(
            r#"{"cmd":"plan","op":{"op":"set_auto_fill","on":false}}"#,
        )
        .unwrap();
        assert_eq!(c, PlayerCommand::Plan { op: PlanOp::SetAutoFill { on: false } });
    }

    #[test]
    fn patch_clears_max_play() {
        let mut s = MixSettings { max_play_s: Some(180.0), ..Default::default() };
        let p: MixSettingsPatch = serde_json::from_str(r#"{"max_play_s":null}"#).unwrap();
        s.apply(&p);
        assert_eq!(s.max_play_s, None);
        let p: MixSettingsPatch = serde_json::from_str(r#"{"echo":false,"length_beats":50}"#).unwrap();
        s.apply(&p);
        assert!(!s.echo);
        assert_eq!(s.length_beats, 64);
        let keep: MixSettingsPatch = serde_json::from_str(r#"{}"#).unwrap();
        s.max_play_s = Some(120.0);
        s.apply(&keep);
        assert_eq!(s.max_play_s, Some(120.0));
    }

    #[test]
    fn stored_settings_without_rev_migrate_to_the_dj_blend() {
        let mut s: MixSettings = serde_json::from_str(r#"{"transition":"blend","length_beats":32,"quantise":"bar","echo":false}"#).unwrap();
        assert_eq!(s.rev, 0);
        s.migrate();
        assert_eq!((s.transition, s.length_beats, s.quantise, s.rev), (TransitionKind::EqBlend, 64, Quantise::Phrase, MIX_SETTINGS_REV));
        assert!(!s.echo);
        // a current revision keeps what the user chose
        let mut c = MixSettings { transition: TransitionKind::Blend, length_beats: 32, ..Default::default() };
        c.migrate();
        assert_eq!((c.transition, c.length_beats), (TransitionKind::Blend, 32));
        let p: MixSettingsPatch = serde_json::from_str(r#"{"length_beats":128,"glide_back_s":1}"#).unwrap();
        c.apply(&p);
        assert_eq!((c.length_beats, c.glide_back_s), (128, 2.0));
    }

    #[test]
    fn rules_mirror_breaks_rules() {
        let r = TagRules { allow: vec!["Techno".into()], deny: vec!["ambient".into()] };
        assert!(!r.breaks(&["techno".into()]));
        assert!(r.breaks(&["house".into()]));
        assert!(r.breaks(&["techno".into(), "Ambient".into()]));
        assert!(!TagRules::default().breaks(&["x".into()]));
    }

    #[test]
    fn state_default_serialises() {
        let s = PlayerState::default();
        let j = serde_json::to_string(&s).unwrap();
        let back: PlayerState = serde_json::from_str(&j).unwrap();
        assert_eq!(s, back);
    }
}
