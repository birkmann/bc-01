//! Suggest what could come next while a set is playing (port of `recommend/nextup.py`).
//!
//! The DJ picks; the machine proposes. Given the *seed* (the track a set is heading out of), a
//! *direction* (tempo / energy / tags) and *wishes*, a pool of candidates is scored and ranked.
//!
//! This file has two halves: the pure scorer (no database, every weight testable against
//! hand-built tracks) and [`suggest`], the in-process pipeline that builds the pool with indexed
//! SQL, ranks it, and hydrates only the winners.
//!
//! Score = weighted blend of key, tempo, tag and energy fit (each 0..1), plus additive wish
//! boosts and small penalties for "the same thing again". Terms that cannot be measured score
//! neutral rather than zero, so an unanalysed library still gets tag-driven suggestions.

use std::collections::{BTreeSet, HashMap, HashSet};

use bc_music::camelot::{bpm_compatibility, compatible_keys, key_compatibility};
use bc_music::{Compatibility, Verdict};
use bc_types::suggest::{
    EnergyDir, Harmonic, SeedOverride, SuggestRequest, SuggestResponse, SuggestionOut, TagMode, Tempo };

use bc_types::library::TrackOut;
use crate::error::Result;
use crate::hydrate;
use crate::pooling;
use crate::scope::{Scope, ScopeExt};
use crate::sqlutil::{PRESENT, in_list, name_key_list, now_secs, parse_ts, round4, tagged_tracks_sql};

pub const W_KEY: f64 = 0.35;
pub const W_TEMPO: f64 = 0.30;
pub const W_TAGS: f64 = 0.15;
pub const W_ENERGY: f64 = 0.10;

pub const BOOST_WISH_TRACK: f64 = 1.0;
pub const BOOST_WISH_ARTIST: f64 = 0.6;
pub const BOOST_WISH_LABEL: f64 = 0.5;
pub const W_WISH_TAGS: f64 = 0.6;

pub const PEN_SAME_ARTIST: f64 = 0.15;
pub const PEN_SAME_RELEASE: f64 = 0.10;
pub const PEN_RECENT: f64 = 0.20;
pub const BONUS_LOVED: f64 = 0.03;

/// "Recently played" window, seconds (24 h).
pub const RECENT_SECS: f64 = 24.0 * 3600.0;
pub const NEUTRAL: f64 = 0.5;
/// Where "drift" wants to sit: some overlap with the seed, not a clone of it.
pub const DRIFT_TARGET: f64 = 0.4;

/// Candidates handed to the scorer per requested row (legacy `POOL_PER_ROW/MIN/MAX`).
pub const POOL_PER_ROW: i64 = 10;
pub const POOL_MIN: i64 = 200;
pub const POOL_MAX: i64 = 500;

pub fn tempo_shift(t: Tempo) -> f64 {
    match t {
        Tempo::Lower => -0.04,
        Tempo::Keep => 0.0,
        Tempo::Raise => 0.04,
    }
}

pub fn energy_shift(e: EnergyDir) -> f64 {
    match e {
        EnergyDir::Down => -0.12,
        EnergyDir::Keep => 0.0,
        EnergyDir::Up => 0.12,
    }
}

/// The track the set is heading out of. Any field may be unknown.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Seed {
    pub bpm: Option<f64>,
    pub camelot: Option<String>,
    pub energy: Option<f64>,
    pub tags: BTreeSet<String>,
    pub artist_id: Option<i64>,
    pub release_id: Option<i64>,
}

/// Scorer-side direction (the DTO's `allow_tags`/`deny_tags` are pool filters, not scoring).
#[derive(Debug, Clone, PartialEq)]
pub struct Direction {
    pub tempo: Tempo,
    pub energy: EnergyDir,
    pub tag_mode: TagMode,
    pub tags: BTreeSet<String>,
    pub harmonic: Harmonic,
    pub bpm_tolerance: f64,
}

impl Default for Direction {
    fn default() -> Self {
        Self {
            tempo: Tempo::Keep,
            energy: EnergyDir::Keep,
            tag_mode: TagMode::Drift,
            tags: BTreeSet::new(),
            harmonic: Harmonic::Strict,
            bpm_tolerance: 0.06,
        }
    }
}

impl Direction {
    pub fn from_request(req: &SuggestRequest) -> Self {
        let d = &req.direction;
        Self {
            tempo: d.tempo,
            energy: d.energy,
            tag_mode: d.tag_mode,
            tags: d.tags.iter().cloned().collect(),
            harmonic: d.harmonic,
            bpm_tolerance: d.bpm_tolerance,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Wishes {
    pub track_ids: HashSet<i64>,
    pub artist_ids: HashSet<i64>,
    pub label_ids: HashSet<i64>,
    pub tags: BTreeSet<String>,
}

impl Wishes {
    pub fn from_request(req: &SuggestRequest) -> Self {
        let w = &req.wishes;
        Self {
            track_ids: w.wish_track_ids.iter().copied().collect(),
            artist_ids: w.wish_artist_ids.iter().copied().collect(),
            label_ids: w.wish_label_ids.iter().copied().collect(),
            tags: w.wish_tags.iter().cloned().collect(),
        }
    }
    pub fn is_empty(&self) -> bool {
        self.track_ids.is_empty() && self.artist_ids.is_empty() && self.label_ids.is_empty() && self.tags.is_empty()
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Candidate {
    pub track_id: i64,
    pub bpm: Option<f64>,
    pub camelot: Option<String>,
    pub energy: Option<f64>,
    pub tags: BTreeSet<String>,
    pub artist_id: Option<i64>,
    pub release_id: Option<i64>,
    pub label_id: Option<i64>,
    pub loved: bool,
    /// Unix seconds.
    pub last_played_at: Option<f64>,
}

impl Candidate {
    pub fn new(track_id: i64) -> Self {
        Self { track_id, ..Default::default() }
    }
}

#[derive(Debug, Clone)]
pub struct Scored {
    pub candidate: Candidate,
    pub score: f64,
    pub key: Compatibility,
    pub bpm: Compatibility,
    pub why: Vec<String>,
}

/// The tempo the next track should sit around, given where we want to go.
pub fn target_bpm(seed_bpm: Option<f64>, tempo: Tempo) -> Option<f64> {
    let b = seed_bpm.filter(|b| *b > 0.0)?;
    Some(bc_music::setmath::round_to(b * (1.0 + tempo_shift(tempo)), 2))
}

/// The tempo bands worth querying: straight, double and half time.
pub fn bpm_windows(target: f64, tolerance: f64) -> Vec<(f64, f64)> {
    let (lo, hi) = (target * (1.0 - tolerance), target * (1.0 + tolerance));
    vec![(lo, hi), (lo * 2.0, hi * 2.0), (lo / 2.0, hi / 2.0)]
}

pub fn target_energy(seed_energy: Option<f64>, d: EnergyDir) -> Option<f64> {
    seed_energy.map(|e| (e + energy_shift(d)).clamp(0.0, 1.0))
}

/// How much a tag says: `electronic` on 40 % of a library says almost nothing.
pub fn idf(track_count: i64, total_tracks: i64) -> f64 {
    (1.0 + total_tracks as f64 / track_count.max(1) as f64).ln()
}

pub fn norm(tag: &str) -> String {
    tag.trim().to_lowercase()
}

fn normed<'a>(tags: impl IntoIterator<Item = &'a String>) -> BTreeSet<String> {
    tags.into_iter().map(|t| norm(t)).collect()
}

/// IDF-weighted cosine between two tag sets, 0..1. Unknown tags weigh 1.
pub fn tag_similarity<'a, 'b>(
    a: impl IntoIterator<Item = &'a String>,
    b: impl IntoIterator<Item = &'b String>,
    idf_of: &HashMap<String, f64>,
) -> f64 {
    let left = normed(a);
    let right = normed(b);
    if left.is_empty() || right.is_empty() {
        return 0.0;
    }
    let w = |t: &String| idf_of.get(t).copied().unwrap_or(1.0);
    let shared: f64 = left.intersection(&right).map(|t| w(t).powi(2)).sum();
    if shared == 0.0 {
        return 0.0;
    }
    let nl = left.iter().map(|t| w(t).powi(2)).sum::<f64>().sqrt();
    let nr = right.iter().map(|t| w(t).powi(2)).sum::<f64>().sqrt();
    (shared / (nl * nr)).min(1.0)
}

fn shared_sorted(a: &BTreeSet<String>, b: &BTreeSet<String>) -> Vec<String> {
    let l = normed(a);
    let r = normed(b);
    l.intersection(&r).cloned().collect()
}

fn tag_term(c: &Candidate, seed: &Seed, d: &Direction, idf_of: &HashMap<String, f64>) -> (f64, Option<String>) {
    let sim = tag_similarity(&c.tags, &seed.tags, idf_of);
    if d.tag_mode == TagMode::Switch && !d.tags.is_empty() {
        let towards = tag_similarity(&c.tags, &d.tags, idf_of);
        let hit = shared_sorted(&c.tags, &d.tags);
        let note = if hit.is_empty() { None } else { Some(format!("towards: {}", hit.iter().take(2).cloned().collect::<Vec<_>>().join(", "))) };
        return (0.7 * towards + 0.3 * sim, note);
    }
    if seed.tags.is_empty() {
        return (NEUTRAL, None);
    }
    let shared = shared_sorted(&c.tags, &seed.tags);
    let note = if shared.is_empty() { None } else { Some(format!("shares: {}", shared.iter().take(2).cloned().collect::<Vec<_>>().join(", "))) };
    if d.tag_mode == TagMode::Stick {
        return (sim, note);
    }
    // drift: reward some overlap, not a clone
    (0.5 * sim + 0.5 * (1.0 - (sim - DRIFT_TARGET).abs()), note)
}

pub fn score(c: &Candidate, seed: &Seed, d: &Direction, w: &Wishes, idf_of: &HashMap<String, f64>, now: f64) -> Scored {
    let mut why: Vec<String> = vec![];

    let key = key_compatibility(seed.camelot.as_deref(), c.camelot.as_deref());
    let key_term = if d.harmonic == Harmonic::Off || seed.camelot.as_deref().is_none_or(str::is_empty) || c.camelot.as_deref().is_none_or(str::is_empty) {
        NEUTRAL
    } else {
        why.push(key.reason.clone());
        key.score
    };

    let target = target_bpm(seed.bpm, d.tempo);
    let bpm = bpm_compatibility(target, c.bpm, d.bpm_tolerance);
    let bpm_term = if d.harmonic == Harmonic::Off || target.is_none() || c.bpm.is_none_or(|b| b == 0.0) {
        NEUTRAL
    } else {
        why.push(bpm.reason.clone());
        bpm.score
    };

    let (tag_t, tag_note) = tag_term(c, seed, d, idf_of);
    if let Some(n) = tag_note {
        why.push(n);
    }

    let energy_term = match (target_energy(seed.energy, d.energy), c.energy) {
        (Some(t), Some(e)) => 1.0 - ((e - t).abs() / 0.5).min(1.0),
        _ => NEUTRAL,
    };

    let mut total = W_KEY * key_term + W_TEMPO * bpm_term + W_TAGS * tag_t + W_ENERGY * energy_term;

    if w.track_ids.contains(&c.track_id) {
        total += BOOST_WISH_TRACK;
        why.push("wished track".into());
    }
    let wished_artist = c.artist_id.is_some_and(|a| w.artist_ids.contains(&a));
    if wished_artist {
        total += BOOST_WISH_ARTIST;
        why.push("wished artist".into());
    }
    if c.label_id.is_some_and(|l| w.label_ids.contains(&l)) {
        total += BOOST_WISH_LABEL;
        why.push("wished label".into());
    }
    if !w.tags.is_empty() {
        let hit = tag_similarity(&c.tags, &w.tags, idf_of);
        if hit > 0.0 {
            total += W_WISH_TAGS * hit;
            let names = shared_sorted(&c.tags, &w.tags);
            why.push(format!("wished: {}", names.iter().take(2).cloned().collect::<Vec<_>>().join(", ")));
        }
    }

    if seed.artist_id.is_some() && c.artist_id == seed.artist_id && !wished_artist {
        total -= PEN_SAME_ARTIST;
    }
    if seed.release_id.is_some() && c.release_id == seed.release_id {
        total -= PEN_SAME_RELEASE;
    }
    if let Some(p) = c.last_played_at
        && now - p < RECENT_SECS
    {
        total -= PEN_RECENT;
        why.push("played recently".into());
    }
    if c.loved {
        total += BONUS_LOVED;
    }

    Scored { candidate: c.clone(), score: round4(total), key, bpm, why }
}

pub fn rank<'a>(
    candidates: impl IntoIterator<Item = &'a Candidate>,
    seed: &Seed,
    d: &Direction,
    w: &Wishes,
    idf_of: &HashMap<String, f64>,
    limit: usize,
    now: f64,
) -> Vec<Scored> {
    let mut scored: Vec<Scored> = candidates.into_iter().map(|c| score(c, seed, d, w, idf_of, now)).collect();
    scored.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.candidate.track_id.cmp(&b.candidate.track_id)));
    scored.truncate(limit);
    scored
}

// ======================================================================================
// The pipeline: pool -> rank -> hydrate winners
// ======================================================================================

/// How the pool is narrowed beyond the request itself.
#[derive(Debug, Clone, Default)]
pub struct RunOpts<'a> {
    /// A caller-built seed (a set's tail slot at its *effective* key and tempo).
    pub seed: Option<Seed>,
    /// Echoed as `seed_track_id` when the seed row is not looked up.
    pub seed_track_id: Option<i64>,
    /// SQL `SELECT` of track ids the pool must stay inside (a set's pool sources).
    pub restrict_sql: Option<&'a str>,
}

/// Seed facts read from a track row.
pub(crate) struct SeedRow {
    pub id: i64,
    pub bpm: Option<f64>,
    pub camelot: Option<String>,
    pub energy: Option<f64>,
    pub tags: BTreeSet<String>,
    pub artist_id: Option<i64>,
    pub release_id: Option<i64>,
    pub label_id: Option<i64>,
}

/// One light row for the seed track (`None` when the id has no row).
pub(crate) fn load_seed_row(conn: &bc_db::rusqlite::Connection, track_id: i64) -> Result<Option<SeedRow>> {
    use bc_db::rusqlite::OptionalExtension;
    let row = conn
        .query_row(
            "SELECT t.id, a.bpm, a.camelot, a.energy, COALESCE(t.artist_id, r.artist_id), t.release_id, r.label_id
             FROM tracks t LEFT JOIN releases r ON r.id = t.release_id LEFT JOIN analysis a ON a.track_id = t.id
             WHERE t.id = ?1",
            [track_id],
            |r| {
                Ok(SeedRow {
                    id: r.get(0)?,
                    bpm: r.get(1)?,
                    camelot: r.get(2)?,
                    energy: r.get(3)?,
                    tags: BTreeSet::new(),
                    artist_id: r.get(4)?,
                    release_id: r.get(5)?,
                    label_id: r.get(6)?,
                })
            },
        )
        .optional()?;
    let Some(mut row) = row else { return Ok(None) };
    let (tags, _) = pooling::tags_by_track(conn, &[row.id])?;
    row.tags = tags.get(&row.id).cloned().unwrap_or_default();
    Ok(Some(row))
}

pub(crate) fn seed_from_override(o: Option<&SeedOverride>) -> Seed {
    match o {
        None => Seed::default(),
        Some(o) => Seed {
            bpm: o.bpm,
            camelot: o.camelot.clone(),
            energy: o.energy,
            tags: o.tags.iter().cloned().collect(),
            ..Default::default()
        },
    }
}

fn clean(tags: &[String]) -> Vec<String> {
    tags.iter().filter(|t| !t.trim().is_empty()).cloned().collect()
}

/// The pool query: light columns for candidates, one indexed statement.
fn pool_sql(seed: &Seed, req: &SuggestRequest, exclude: &[i64], limit: i64, scope: &Scope, restrict: Option<&str>) -> String {
    let d = &req.direction;
    let target = target_bpm(seed.bpm, d.tempo);
    let join_analysis = d.harmonic != Harmonic::Off && (seed.camelot.as_deref().is_some_and(|c| !c.is_empty()) || target.is_some());
    let mut sql = String::from(
        "SELECT t.id, a.bpm, a.camelot, a.energy, COALESCE(t.artist_id, r.artist_id), t.release_id, r.label_id, t.loved, t.last_played_at \
         FROM tracks t LEFT JOIN releases r ON r.id = t.release_id ",
    );
    sql.push_str(if join_analysis { "JOIN analysis a ON a.track_id = t.id " } else { "LEFT JOIN analysis a ON a.track_id = t.id " });
    sql.push_str(&format!("WHERE {PRESENT} AND {}", scope.tp()));
    if let Some(r) = restrict {
        sql.push_str(&format!(" AND t.id IN ({r})"));
    }
    if !exclude.is_empty() {
        sql.push_str(&format!(" AND t.id NOT IN ({})", in_list(exclude)));
    }
    if let Some(p) = req.playlist_id {
        sql.push_str(&format!(" AND t.id IN (SELECT pi.track_id FROM playlist_items pi WHERE pi.playlist_id = {p})"));
    }
    if req.loved == Some(true) {
        sql.push_str(" AND t.loved = 1");
    }
    let mut order = "t.id".to_string();
    if join_analysis {
        if let Some(c) = seed.camelot.as_deref().filter(|c| !c.is_empty()) {
            let keys = compatible_keys(Some(c), d.harmonic == Harmonic::Loose);
            let list = keys.iter().map(|k| crate::sqlutil::lit(k)).collect::<Vec<_>>().join(",");
            sql.push_str(&format!(" AND a.camelot IN ({list})"));
        }
        if let Some(t) = target {
            let ors = bpm_windows(t, d.bpm_tolerance)
                .iter()
                .map(|(lo, hi)| format!("a.bpm BETWEEN {lo:?} AND {hi:?}"))
                .collect::<Vec<_>>()
                .join(" OR ");
            sql.push_str(&format!(" AND ({ors})"));
            order = format!("abs(a.bpm - {t:?}), t.id");
        }
    }
    if d.tag_mode == TagMode::Switch && !d.tags.is_empty() {
        if let Some(keys) = name_key_list(d.tags.iter()) {
            sql.push_str(&format!(" AND t.id IN ({})", tagged_tracks_sql(&keys)));
        }
    } else if d.tag_mode == TagMode::Stick
        && !seed.tags.is_empty()
        && let Some(keys) = name_key_list(seed.tags.iter())
    {
        sql.push_str(&format!(" AND t.id IN ({})", tagged_tracks_sql(&keys)));
    }
    if let Some(keys) = name_key_list(clean(&d.allow_tags).iter()) {
        sql.push_str(&format!(" AND t.id IN ({})", tagged_tracks_sql(&keys)));
    }
    if let Some(keys) = name_key_list(clean(&d.deny_tags).iter()) {
        sql.push_str(&format!(" AND t.id NOT IN ({})", tagged_tracks_sql(&keys)));
    }
    sql.push_str(&format!(" ORDER BY {order} LIMIT {limit}"));
    sql
}

/// The pool statement, exposed so tests can `EXPLAIN QUERY PLAN` it.
#[doc(hidden)]
pub fn pool_sql_for_tests(seed: &Seed, req: &SuggestRequest, scope: &Scope, restrict: Option<&str>) -> String {
    pool_sql(seed, req, &[], 200, scope, restrict)
}

type PoolRow = Candidate;

fn read_pool(conn: &bc_db::rusqlite::Connection, sql: &str) -> Result<Vec<PoolRow>> {
    let mut st = conn.prepare(sql)?;
    let rows = st.query_map([], |r| {
        Ok(Candidate {
            track_id: r.get(0)?,
            bpm: r.get(1)?,
            camelot: r.get(2)?,
            energy: r.get(3)?,
            artist_id: r.get(4)?,
            release_id: r.get(5)?,
            label_id: r.get(6)?,
            loved: r.get::<_, Option<bool>>(7)?.unwrap_or(false),
            last_played_at: r.get::<_, Option<String>>(8)?.and_then(|s| parse_ts(&s)),
            tags: BTreeSet::new(),
        })
    })?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

/// The ranking pipeline behind `POST /suggest/next`, on an open connection.
pub fn run(conn: &bc_db::rusqlite::Connection, scope: &Scope, req: &SuggestRequest, opts: RunOpts<'_>) -> Result<SuggestResponse<TrackOut>> {
    let mut seed_row: Option<SeedRow> = None;
    let seed = if let Some(s) = opts.seed.clone() {
        s
    } else {
        if let Some(id) = req.seed_track_id.filter(|i| *i > 0) {
            seed_row = load_seed_row(conn, id)?;
        }
        match &seed_row {
            Some(r) => {
                let mut s = Seed {
                    bpm: r.bpm,
                    camelot: r.camelot.clone(),
                    energy: r.energy,
                    tags: r.tags.clone(),
                    artist_id: r.artist_id,
                    release_id: r.release_id,
                };
                // A client override fills in what the row lacks.
                if let Some(o) = &req.seed {
                    s = Seed {
                        bpm: s.bpm.or(o.bpm),
                        camelot: s.camelot.filter(|c| !c.is_empty()).or_else(|| o.camelot.clone()),
                        energy: s.energy.or(o.energy),
                        tags: s.tags.union(&o.tags.iter().cloned().collect()).cloned().collect(),
                        artist_id: s.artist_id,
                        release_id: s.release_id,
                    };
                }
                s
            }
            None => seed_from_override(req.seed.as_ref()),
        }
    };

    let mut exclude: BTreeSet<i64> = req.exclude_track_ids.iter().copied().collect();
    if let Some(r) = &seed_row {
        exclude.insert(r.id);
    }
    let exclude_v: Vec<i64> = exclude.iter().copied().collect();

    let limit = req.limit.clamp(1, 100);
    let pool_size = POOL_MAX.min(POOL_MIN.max(limit * POOL_PER_ROW));
    let mut pool = read_pool(conn, &pool_sql(&seed, req, &exclude_v, pool_size, scope, opts.restrict_sql))?;

    // Wished tracks join the pool whatever the filters say, so the DJ sees them with their real
    // verdict rather than not at all.
    let have: HashSet<i64> = pool.iter().map(|c| c.track_id).collect();
    let wanted: Vec<i64> = req
        .wishes
        .wish_track_ids
        .iter()
        .copied()
        .filter(|i| !exclude.contains(i) && !have.contains(i))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    if !wanted.is_empty() {
        let sql = format!(
            "SELECT t.id, a.bpm, a.camelot, a.energy, COALESCE(t.artist_id, r.artist_id), t.release_id, r.label_id, t.loved, t.last_played_at \
             FROM tracks t LEFT JOIN releases r ON r.id = t.release_id LEFT JOIN analysis a ON a.track_id = t.id WHERE t.id IN ({})",
            in_list(&wanted)
        );
        pool.extend(read_pool(conn, &sql)?);
    }

    let ids: Vec<i64> = pool.iter().map(|c| c.track_id).collect();
    let (tag_map, tag_counts) = pooling::tags_by_track(conn, &ids)?;
    let total_tracks = pooling::total_tracks(conn)?;
    let idf_of: HashMap<String, f64> = tag_counts.iter().map(|(k, n)| (k.clone(), idf(*n, total_tracks))).collect();
    for c in &mut pool {
        c.tags = tag_map.get(&c.track_id).cloned().unwrap_or_default();
    }

    let dir = Direction::from_request(req);
    let wishes = Wishes::from_request(req);
    let ranked = rank(pool.iter(), &seed, &dir, &wishes, &idf_of, limit as usize, now_secs());

    let win: Vec<i64> = ranked.iter().map(|s| s.candidate.track_id).collect();
    let mut briefs = hydrate::briefs(conn, &win)?;
    let items = ranked
        .into_iter()
        .filter_map(|s| {
            let track = briefs.remove(&s.candidate.track_id)?;
            Some(SuggestionOut {
                track,
                score: s.score,
                key_verdict: s.key.verdict.as_str().to_string(),
                key_reason: s.key.reason,
                bpm_verdict: s.bpm.verdict.as_str().to_string(),
                bpm_reason: s.bpm.reason,
                why: s.why,
            })
        })
        .collect();

    Ok(SuggestResponse {
        seed_track_id: seed_row.as_ref().map(|r| r.id).or(opts.seed_track_id),
        seed_bpm: seed.bpm,
        seed_camelot: seed.camelot.clone(),
        seed_energy: seed.energy,
        target_bpm: target_bpm(seed.bpm, d_tempo(req)),
        items,
    })
}

fn d_tempo(req: &SuggestRequest) -> Tempo {
    req.direction.tempo
}

/// In-process entry for callers outside HTTP (WS4's player auto-fill): the whole library, a
/// playlist (`req.playlist_id`) or the loved tracks (`req.loved`).
pub fn suggest(db: &bc_db::Db, scope: &Scope, req: &SuggestRequest) -> Result<SuggestResponse<TrackOut>> {
    crate::error::read(db, |c| run(c, scope, req, RunOpts::default()))
}

/// Like [`suggest`], restricted to an explicit id set (a set's pool sources, a crate, ...).
pub fn suggest_among(db: &bc_db::Db, scope: &Scope, req: &SuggestRequest, ids: &[i64]) -> Result<SuggestResponse<TrackOut>> {
    let sql = format!("SELECT id FROM tracks WHERE id IN ({})", in_list(ids));
    crate::error::read(db, |c| run(c, scope, req, RunOpts { restrict_sql: Some(&sql), ..Default::default() }))
}

/// Async flavour of [`suggest`] for axum handlers.
pub async fn suggest_async(db: &bc_db::Db, scope: Scope, req: SuggestRequest) -> Result<SuggestResponse<TrackOut>> {
    crate::error::read_async(db, move |c| run(c, &scope, &req, RunOpts::default())).await
}

#[allow(dead_code)]
fn _verdict(_: Verdict) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|x| x.to_string()).collect()
    }

    fn rank_ids(cands: &[Candidate], seed: &Seed, d: &Direction, w: &Wishes, idf: &HashMap<String, f64>, limit: usize, now: f64) -> Vec<i64> {
        rank(cands.iter(), seed, d, w, idf, limit, now).iter().map(|x| x.candidate.track_id).collect()
    }

    fn quick(cands: &[Candidate], seed: &Seed, d: &Direction) -> Vec<i64> {
        rank_ids(cands, seed, d, &Wishes::default(), &HashMap::new(), 10, 0.0)
    }

    fn cand(id: i64, bpm: Option<f64>, cam: Option<&str>) -> Candidate {
        Candidate { track_id: id, bpm, camelot: cam.map(String::from), ..Default::default() }
    }

    fn tagged(id: i64, tags: &[&str]) -> Candidate {
        Candidate { track_id: id, tags: s(tags), ..Default::default() }
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn target_bpm_shifts_by_direction() {
        assert_eq!(target_bpm(Some(128.0), Tempo::Keep), Some(128.0));
        assert!(close(target_bpm(Some(128.0), Tempo::Raise).unwrap(), 133.12));
        assert!(close(target_bpm(Some(128.0), Tempo::Lower).unwrap(), 122.88));
        assert_eq!(target_bpm(None, Tempo::Raise), None);
        assert_eq!(target_bpm(Some(0.0), Tempo::Raise), None);
    }

    #[test]
    fn bpm_windows_cover_half_and_double_time() {
        let w = bpm_windows(100.0, 0.06);
        assert!(close(w[0].0, 94.0) && close(w[0].1, 106.0));
        assert!(close(w[1].0, 188.0) && close(w[1].1, 212.0));
        assert!(close(w[2].0, 47.0) && close(w[2].1, 53.0));
    }

    #[test]
    fn raise_prefers_the_faster_track_and_lower_the_slower() {
        let seed = Seed { bpm: Some(128.0), camelot: Some("8A".into()), ..Default::default() };
        let cands = [cand(1, Some(132.0), Some("8A")), cand(2, Some(124.0), Some("8A"))];
        assert_eq!(quick(&cands, &seed, &Direction { tempo: Tempo::Raise, ..Default::default() }), [1, 2]);
        assert_eq!(quick(&cands, &seed, &Direction { tempo: Tempo::Lower, ..Default::default() }), [2, 1]);
    }

    #[test]
    fn keep_prefers_the_same_tempo() {
        let seed = Seed { bpm: Some(128.0), camelot: Some("8A".into()), ..Default::default() };
        let cands = [cand(2, Some(133.0), Some("8A")), cand(1, Some(128.0), Some("8A"))];
        assert_eq!(quick(&cands, &seed, &Direction::default()), [1, 2]);
    }

    #[test]
    fn tag_similarity_extremes() {
        let none = HashMap::new();
        assert!(close(tag_similarity(&s(&["techno", "dub"]), &s(&["Techno", "DUB"]), &none), 1.0));
        assert_eq!(tag_similarity(&s(&["techno"]), &s(&["ambient"]), &none), 0.0);
        assert_eq!(tag_similarity(&s(&[]), &s(&["ambient"]), &none), 0.0);
    }

    #[test]
    fn a_rare_shared_tag_outweighs_a_common_one() {
        let idf_of: HashMap<String, f64> =
            [("electronic".to_string(), idf(40_000, 100_000)), ("gqom".to_string(), idf(12, 100_000))].into();
        let seed_tags = s(&["electronic", "gqom"]);
        let rare = tag_similarity(&s(&["gqom", "house"]), &seed_tags, &idf_of);
        let common = tag_similarity(&s(&["electronic", "house"]), &seed_tags, &idf_of);
        assert!(rare > common);
    }

    #[test]
    fn stick_keeps_the_genre_and_switch_moves_towards_the_target() {
        let seed = Seed { tags: s(&["techno"]), ..Default::default() };
        let techno = tagged(1, &["techno"]);
        let breaks = tagged(2, &["breaks"]);
        let bridge = tagged(3, &["techno", "breaks"]);
        let stick = Direction { tag_mode: TagMode::Stick, ..Default::default() };
        assert_eq!(quick(&[breaks.clone(), techno.clone()], &seed, &stick), [1, 2]);
        let to_breaks = Direction { tag_mode: TagMode::Switch, tags: s(&["breaks"]), ..Default::default() };
        let switched = quick(&[techno, breaks, bridge], &seed, &to_breaks);
        // The bridging track carries both, so it leads; pure techno trails.
        assert_eq!(switched[0], 3);
        assert_eq!(*switched.last().unwrap(), 1);
    }

    #[test]
    fn switch_reasons_name_the_target_tag() {
        let seed = Seed { tags: s(&["techno"]), ..Default::default() };
        let d = Direction { tag_mode: TagMode::Switch, tags: s(&["breaks"]), ..Default::default() };
        let scored = score(&tagged(1, &["breaks"]), &seed, &d, &Wishes::default(), &HashMap::new(), 0.0);
        assert!(scored.why.contains(&"towards: breaks".to_string()));
    }

    #[test]
    fn a_wished_label_outranks_an_equal_stranger() {
        let seed = Seed { bpm: Some(128.0), camelot: Some("8A".into()), ..Default::default() };
        let mut stranger = cand(1, Some(128.0), Some("8A"));
        stranger.label_id = Some(7);
        let mut wished = cand(2, Some(128.0), Some("8A"));
        wished.label_id = Some(9);
        let w = Wishes { label_ids: [9].into(), ..Default::default() };
        let ranked = rank([&stranger, &wished], &seed, &Direction::default(), &w, &HashMap::new(), 5, 0.0);
        assert_eq!(ranked.iter().map(|x| x.candidate.track_id).collect::<Vec<_>>(), [2, 1]);
        assert!(ranked[0].why.contains(&"wished label".to_string()));
    }

    #[test]
    fn wished_track_and_artist_boost() {
        let seed = Seed { bpm: Some(128.0), camelot: Some("8A".into()), ..Default::default() };
        let mut plain = cand(1, Some(128.0), Some("8A"));
        plain.artist_id = Some(1);
        let mut by_artist = cand(2, Some(128.0), Some("8A"));
        by_artist.artist_id = Some(42);
        let mut the_one = cand(3, Some(118.0), Some("3B")); // clashes, but wished outright
        let w = Wishes { track_ids: [3].into(), artist_ids: [42].into(), ..Default::default() };
        let r = |c: &[Candidate]| rank_ids(c, &seed, &Direction::default(), &w, &HashMap::new(), 10, 0.0);
        // Both wishes surface above the stranger; the clashing track is still shown (that is the
        // point of a wish) but a wish that also mixes leads.
        assert_eq!(r(&[plain.clone(), by_artist.clone(), the_one.clone()]), [2, 3, 1]);
        the_one.bpm = Some(128.0);
        the_one.camelot = Some("8A".into());
        assert_eq!(r(&[plain, by_artist, the_one]), [3, 2, 1]);
    }

    #[test]
    fn same_artist_and_recent_plays_are_penalised() {
        let now = parse_ts("2026-08-17 12:00:00").unwrap();
        let seed = Seed { bpm: Some(128.0), camelot: Some("8A".into()), artist_id: Some(5), ..Default::default() };
        let mut other = cand(1, Some(128.0), Some("8A"));
        other.artist_id = Some(6);
        let mut same_artist = cand(2, Some(128.0), Some("8A"));
        same_artist.artist_id = Some(5);
        let mut just_played = cand(3, Some(128.0), Some("8A"));
        just_played.artist_id = Some(7);
        just_played.last_played_at = Some(now - 2.0 * 3600.0);
        let ranked = rank_ids(&[same_artist, just_played, other], &seed, &Direction::default(), &Wishes::default(), &HashMap::new(), 10, now);
        assert_eq!(ranked[0], 1);
        assert_eq!(ranked[1..].iter().copied().collect::<HashSet<_>>(), [2, 3].into());
    }

    #[test]
    fn unanalysed_seed_lets_tags_decide() {
        let seed = Seed { tags: s(&["dub techno"]), ..Default::default() };
        let mut m = tagged(1, &["dub techno"]);
        m.bpm = Some(140.0);
        m.camelot = Some("1B".into());
        let mut stranger = tagged(2, &["pop"]);
        stranger.bpm = Some(128.0);
        stranger.camelot = Some("8A".into());
        let d = Direction { tag_mode: TagMode::Stick, ..Default::default() };
        assert_eq!(quick(&[stranger, m], &seed, &d), [1, 2]);
    }

    #[test]
    fn limit_and_stable_order() {
        let cands: Vec<Candidate> = [5, 3, 9, 1].iter().map(|i| Candidate::new(*i)).collect();
        assert_eq!(rank_ids(&cands, &Seed::default(), &Direction::default(), &Wishes::default(), &HashMap::new(), 2, 0.0), [1, 3]);
    }
}
