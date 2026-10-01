//! What else sounds like this track (port of `recommend/similar.py` and `POST /suggest/similar`).
//!
//! A third scorer, not `nextup` retuned: artist and label *pay* here instead of costing, key and
//! tempo are soft terms, and the weights (tags .40 / artist .18 / label .14 / tempo .14 /
//! key .08 / energy .06) are renormalised over the signals that are enabled **and** that the seed
//! can supply. Key and tempo alone are not similarity (the evidence guard).
//!
//! Hot-path SQL (PLAN 9l), both measured on the 170k-track library:
//! * "tracks by this artist" is two statements, never `OR` across tracks and releases;
//! * "file on disk" is a correlated `EXISTS`, never an `IN (SELECT track_id FROM files ..)`.

use std::collections::{BTreeSet, HashMap, HashSet};

use bc_db::rusqlite::{Connection, OptionalExtension};
use bc_music::camelot::{bpm_compatibility, key_compatibility};
use bc_types::suggest::{SeedOverride, Signals, SimilarItem, SimilarRequest, SimilarResponse };

use bc_types::library::TrackOut;
use crate::error::Result;
use crate::nextup::{self, idf, tag_similarity};
use crate::pooling::{self, TAG_BUDGET};
use crate::scope::{Scope, ScopeExt};
use crate::sqlutil::{PRESENT, in_list, round4};

pub const W_TAGS: f64 = 0.40;
pub const W_ARTIST: f64 = 0.18;
pub const W_LABEL: f64 = 0.14;
pub const W_TEMPO: f64 = 0.14;
pub const W_KEY: f64 = 0.08;
pub const W_ENERGY: f64 = 0.06;
/// Additive, and only on a candidate that already matched something.
pub const BONUS_LOVED: f64 = 0.06;
/// Tempo decay: 0 % apart 1.00, 6 % 0.51, 12 % 0.26, 25 % 0.06.
pub const TEMPO_SCALE: f64 = 0.09;
pub const NEUTRAL: f64 = 0.5;
pub const MAX_PER_ARTIST: usize = 4;
pub const MAX_PER_RELEASE: usize = 2;
pub const WHY_TAGS: usize = 3;
/// How hard a reroll shakes the ranking (0-12 %).
pub const JITTER: f64 = 0.12;

pub const POOL_BY_TAGS: i64 = 2000;
pub const POOL_BY_ARTIST: i64 = 300;
pub const POOL_BY_LABEL: i64 = 500;
pub const POOL_BY_LOVED: i64 = 600;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Seed {
    pub bpm: Option<f64>,
    pub camelot: Option<String>,
    pub energy: Option<f64>,
    pub tags: BTreeSet<String>,
    pub artist_id: Option<i64>,
    pub release_id: Option<i64>,
    pub label_id: Option<i64>,
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
    pub play_count: i64,
}

#[derive(Debug, Clone)]
pub struct Scored {
    pub candidate: Candidate,
    pub score: f64,
    pub why: Vec<String>,
    /// Did a tag, the artist or the label match? (A near tempo in a compatible key does not count.)
    pub evidence: bool,
}

/// How close two tempos sit, 0..1, half and double time counting as the same tempo. `None` when
/// either side is unanalysed. Deliberately a decay and not `bpm_compatibility`'s ±6 % cliff.
pub fn tempo_proximity(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    let (a, b) = (a.filter(|v| *v > 0.0)?, b.filter(|v| *v > 0.0)?);
    let mut best = 0.0f64;
    for (m, penalty) in [(1.0, 1.0), (2.0, 0.8), (0.5, 0.8)] {
        let drift = (b * m - a).abs() / a;
        best = best.max(penalty * (-drift / TEMPO_SCALE).exp());
    }
    Some(best)
}

/// A stable number in `[0, 1)` per (track, reroll). Legacy formula (`shuffle * 40503`), kept
/// exactly so rerolls order the same as the Python app.
pub fn jitter(track_id: i64, shuffle: i64) -> f64 {
    let h = track_id.wrapping_mul(2_654_435_761).wrapping_add(shuffle.wrapping_mul(40_503)).rem_euclid(4_294_967_296);
    h as f64 / 4_294_967_296.0
}

fn shared_tags(c: &Candidate, seed: &Seed, idf_of: &HashMap<String, f64>) -> Vec<String> {
    let spelling: HashMap<String, String> = c.tags.iter().map(|t| (nextup::norm(t), t.trim().to_string())).collect();
    let seed_norm: HashSet<String> = seed.tags.iter().map(|t| nextup::norm(t)).collect();
    let mut shared: Vec<&String> = spelling.keys().filter(|k| seed_norm.contains(*k)).collect();
    shared.sort_by(|a, b| {
        idf_of.get(*b).copied().unwrap_or(1.0).total_cmp(&idf_of.get(*a).copied().unwrap_or(1.0)).then(a.cmp(b))
    });
    shared.into_iter().take(WHY_TAGS).map(|k| spelling[k].clone()).collect()
}

pub fn score(c: &Candidate, seed: &Seed, signals: &Signals, idf_of: &HashMap<String, f64>) -> Scored {
    let mut why: Vec<String> = vec![];
    let mut terms: Vec<(f64, f64)> = vec![];
    // tags / artist / label -- the terms that mean "the same kind of music".
    let mut identity: Vec<f64> = vec![];

    if signals.tags && !seed.tags.is_empty() {
        let fit = tag_similarity(&c.tags, &seed.tags, idf_of);
        terms.push((W_TAGS, fit));
        identity.push(fit);
        let named = if fit > 0.0 { shared_tags(c, seed, idf_of) } else { vec![] };
        if !named.is_empty() {
            why.push(format!("shares: {}", named.join(", ")));
        }
    }

    if signals.artist && seed.artist_id.is_some() {
        let same = c.artist_id.is_some() && c.artist_id == seed.artist_id;
        terms.push((W_ARTIST, if same { 1.0 } else { 0.0 }));
        identity.push(if same { 1.0 } else { 0.0 });
        if same {
            why.push("same artist".into());
        }
    }

    if signals.label && seed.label_id.is_some() {
        let same = c.label_id.is_some() && c.label_id == seed.label_id;
        terms.push((W_LABEL, if same { 1.0 } else { 0.0 }));
        identity.push(if same { 1.0 } else { 0.0 });
        if same {
            why.push("same label".into());
        }
    }

    if signals.tempo && seed.bpm.is_some_and(|b| b != 0.0) {
        match tempo_proximity(seed.bpm, c.bpm) {
            None => terms.push((W_TEMPO, NEUTRAL)),
            Some(near) => {
                terms.push((W_TEMPO, near));
                // Score from the smooth curve, prose from the existing helper.
                let mixable = bpm_compatibility(seed.bpm, c.bpm, 0.06);
                if let Some(b) = c.bpm {
                    if mixable.score > 0.0 {
                        why.push(format!("{:.0} BPM, {}", b, mixable.reason));
                    } else if near >= 0.4 {
                        why.push(format!("{b:.0} BPM"));
                    }
                }
            }
        }
        // Energy rides with the tempo chip.
        if let Some(se) = seed.energy {
            match c.energy {
                None => terms.push((W_ENERGY, NEUTRAL)),
                Some(ce) => terms.push((W_ENERGY, (1.0 - (ce - se).abs() / 0.5).max(0.0))),
            }
        }
    }

    if signals.key && seed.camelot.as_deref().is_some_and(|c| !c.is_empty()) {
        match c.camelot.as_deref().filter(|c| !c.is_empty()) {
            None => terms.push((W_KEY, NEUTRAL)),
            Some(cc) => {
                let key = key_compatibility(seed.camelot.as_deref(), Some(cc));
                terms.push((W_KEY, key.score));
                if key.score > 0.0 {
                    why.push(format!("{cc} \u{2014} {}", key.reason));
                }
            }
        }
    }

    // Every enabled signal was one the seed cannot supply: fall back to tags rather than empty.
    if terms.is_empty() && !seed.tags.is_empty() {
        let fit = tag_similarity(&c.tags, &seed.tags, idf_of);
        terms.push((W_TAGS, fit));
        identity.push(fit);
    }

    let weight: f64 = terms.iter().map(|(w, _)| w).sum();
    let mut total = if weight != 0.0 { terms.iter().map(|(w, t)| w * t).sum::<f64>() / weight } else { 0.0 };

    if total > 0.0 && signals.loved && c.loved {
        total += BONUS_LOVED;
        why.push("loved".into());
    }

    Scored { candidate: c.clone(), score: round4(total), why, evidence: identity.is_empty() || identity.iter().any(|t| *t > 0.0) }
}

/// Score, sort, thin, then page. `shuffle` is the reroll: it knocks each score down by 0-12 % on
/// a stable per-(track, seed) hash, so close calls reorder and clear winners keep winning.
/// Rows without identity evidence are dropped however well they score; the per-artist thinning
/// is lifted when the tag signal is off.
pub fn rank<'a>(
    candidates: impl IntoIterator<Item = &'a Candidate>,
    seed: &Seed,
    signals: &Signals,
    idf_of: &HashMap<String, f64>,
    limit: usize,
    offset: usize,
    shuffle: i64,
) -> Vec<Scored> {
    let mut scored: Vec<Scored> = candidates
        .into_iter()
        .map(|c| {
            let mut s = score(c, seed, signals, idf_of);
            if shuffle != 0 {
                s.score = round4(s.score * (1.0 - JITTER * jitter(c.track_id, shuffle)));
            }
            s
        })
        .collect();
    scored.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.candidate.track_id.cmp(&b.candidate.track_id)));
    let max_per_artist = if signals.tags { Some(MAX_PER_ARTIST) } else { None };
    let mut out: Vec<Scored> = vec![];
    let mut per_artist: HashMap<i64, usize> = HashMap::new();
    let mut per_release: HashMap<i64, usize> = HashMap::new();
    for s in scored {
        if s.score <= 0.0 || !s.evidence {
            continue;
        }
        let (a, r) = (s.candidate.artist_id, s.candidate.release_id);
        if let (Some(max), Some(a)) = (max_per_artist, a)
            && per_artist.get(&a).copied().unwrap_or(0) >= max
        {
            continue;
        }
        if r.is_some_and(|r| per_release.get(&r).copied().unwrap_or(0) >= MAX_PER_RELEASE) {
            continue;
        }
        if let Some(a) = a {
            *per_artist.entry(a).or_default() += 1;
        }
        if let Some(r) = r {
            *per_release.entry(r).or_default() += 1;
        }
        out.push(s);
        if out.len() >= offset + limit {
            break;
        }
    }
    out.into_iter().skip(offset).take(limit).collect()
}

// ======================================================================================
// The pipeline
// ======================================================================================

/// Light pool columns: one row of candidate facts.
const COLS: &str = "t.id, t.artist_id, t.release_id, r.artist_id, r.label_id, t.loved, t.play_count, a.bpm, a.camelot, a.energy";

const FROM: &str = "FROM tracks t LEFT JOIN releases r ON r.id = t.release_id LEFT JOIN analysis a ON a.track_id = t.id";

/// The pool statements (one per branch), exposed for `EXPLAIN QUERY PLAN` tests and for the
/// before/after timing example. Each is skipped when its signal is off.
#[doc(hidden)]
pub fn pool_statements(seed: &Seed, signals: &Signals, scope: &Scope, exclude: &[i64], tag_ids: &[i64], shuffle_seed: i64) -> Vec<(&'static str, String)> {
    let mut base = format!("SELECT {COLS} {FROM} WHERE {PRESENT} AND {}", scope.tp());
    if !exclude.is_empty() {
        base.push_str(&format!(" AND t.id NOT IN ({})", in_list(exclude)));
    }
    let mut out = vec![];
    if !tag_ids.is_empty() {
        // The computed ORDER BY sorts the whole matched set: affordable only because the tag
        // budget caps it. It is also what makes the reroll free and a fixed seed's page stable.
        out.push((
            "tags",
            format!(
                "{base} AND t.id IN (SELECT tt.track_id FROM track_tags tt WHERE tt.tag_id IN ({})) ORDER BY {} LIMIT {POOL_BY_TAGS}",
                in_list(tag_ids),
                pooling::shuffled(shuffle_seed)
            ),
        ));
    }
    if signals.artist
        && let Some(a) = seed.artist_id
    {
        // Two statements rather than one OR: an OR across tracks and releases can use neither
        // ix_tracks_artist_id nor ix_releases_artist_id and scans the table (97 ms vs 0.3 ms).
        out.push(("artist/tracks", format!("{base} AND t.artist_id = {a} LIMIT {POOL_BY_ARTIST}")));
        out.push((
            "artist/releases",
            format!("{base} AND t.release_id IN (SELECT sr.id FROM releases sr WHERE sr.artist_id = {a}) LIMIT {POOL_BY_ARTIST}"),
        ));
    }
    if signals.label
        && let Some(l) = seed.label_id
    {
        out.push(("label", format!("{base} AND r.label_id = {l} LIMIT {POOL_BY_LABEL}")));
    }
    if signals.loved {
        // A loved track sharing the seed's tags may have missed the tag branch's shuffled cut.
        out.push(("loved", format!("{base} AND t.loved = 1 LIMIT {POOL_BY_LOVED}")));
    }
    out
}

fn read_pool(conn: &Connection, sql: &str, into: &mut HashMap<i64, Candidate>) -> Result<()> {
    let mut st = conn.prepare(sql)?;
    let rows = st.query_map([], |r| {
        let (t_artist, r_artist): (Option<i64>, Option<i64>) = (r.get(1)?, r.get(3)?);
        Ok(Candidate {
            track_id: r.get(0)?,
            artist_id: t_artist.or(r_artist),
            release_id: r.get(2)?,
            label_id: r.get(4)?,
            loved: r.get::<_, Option<bool>>(5)?.unwrap_or(false),
            play_count: r.get::<_, Option<i64>>(6)?.unwrap_or(0),
            bpm: r.get(7)?,
            camelot: r.get(8)?,
            energy: r.get(9)?,
            tags: BTreeSet::new(),
        })
    })?;
    for c in rows {
        let c = c?;
        into.entry(c.track_id).or_insert(c);
    }
    Ok(())
}

fn name_of(conn: &Connection, table: &str, id: Option<i64>) -> Result<Option<String>> {
    let Some(id) = id else { return Ok(None) };
    Ok(conn.query_row(&format!("SELECT name FROM {table} WHERE id = ?1"), [id], |r| r.get(0)).optional()?)
}

pub(crate) fn seed_of(conn: &Connection, req: &SimilarRequest) -> Result<(Option<i64>, Seed)> {
    let row = match req.seed_track_id.filter(|i| *i > 0) {
        Some(id) => nextup::load_seed_row(conn, id)?,
        None => None,
    };
    let from_override = |o: &SeedOverride| Seed {
        bpm: o.bpm,
        camelot: o.camelot.clone(),
        energy: o.energy,
        tags: o.tags.iter().cloned().collect(),
        ..Default::default()
    };
    Ok(match row {
        Some(r) => {
            let mut s = Seed {
                bpm: r.bpm,
                camelot: r.camelot,
                energy: r.energy,
                tags: r.tags,
                artist_id: r.artist_id,
                release_id: r.release_id,
                label_id: r.label_id,
            };
            if let Some(o) = &req.seed {
                // A client override fills in what the row lacks.
                s.bpm = s.bpm.or(o.bpm);
                s.camelot = s.camelot.filter(|c| !c.is_empty()).or_else(|| o.camelot.clone());
                s.energy = s.energy.or(o.energy);
                s.tags.extend(o.tags.iter().cloned());
            }
            (Some(r.id), s)
        }
        None => (None, req.seed.as_ref().map(from_override).unwrap_or_default()),
    })
}

/// `POST /suggest/similar` on an open connection.
pub fn run(conn: &Connection, scope: &Scope, req: &SimilarRequest) -> Result<SimilarResponse<TrackOut>> {
    let (seed_track_id, seed) = seed_of(conn, req)?;
    let signals = req.signals;
    let limit = req.limit.clamp(1, 100) as usize;
    let offset = req.offset.clamp(0, 500) as usize;
    let shuffle = req.shuffle_seed.max(0);

    let mut exclude: BTreeSet<i64> = req.exclude_track_ids.iter().copied().collect();
    if let Some(id) = seed_track_id {
        exclude.insert(id);
    }
    let exclude_v: Vec<i64> = exclude.into_iter().collect();

    let seed_artist = name_of(conn, "artists", seed.artist_id)?;
    let seed_label = name_of(conn, "labels", seed.label_id)?;

    let seed_tag_rows = if seed.tags.is_empty() { vec![] } else { pooling::tag_rows(conn, seed.tags.iter())? };
    let drawing = pooling::drawing_tags(&seed_tag_rows, TAG_BUDGET);
    let tag_ids: Vec<i64> = drawing.iter().map(|r| r.0).collect();
    let pool_tags: Vec<String> = drawing.iter().map(|r| r.1.clone()).collect();

    let empty = |pool_size: i64| SimilarResponse {
        seed_track_id,
        seed_bpm: seed.bpm,
        seed_camelot: seed.camelot.clone(),
        seed_tags: seed.tags.iter().cloned().collect(),
        seed_artist: seed_artist.clone(),
        seed_label: seed_label.clone(),
        pool_tags: pool_tags.clone(),
        pool_size,
        items: vec![],
    };

    let mut pool: HashMap<i64, Candidate> = HashMap::new();
    for (_, sql) in pool_statements(&seed, &signals, scope, &exclude_v, &tag_ids, shuffle) {
        read_pool(conn, &sql, &mut pool)?;
    }
    if pool.is_empty() {
        return Ok(empty(0));
    }

    let ids: Vec<i64> = pool.keys().copied().collect();
    let (cand_tags, tag_counts) = pooling::tags_by_track(conn, &ids)?;
    let total = pooling::total_tracks(conn)?;
    let mut idf_of: HashMap<String, f64> = tag_counts.iter().map(|(k, n)| (k.clone(), idf(*n, total))).collect();
    // A seed tag no candidate shares would fall through to the default weight of 1.0, which sits
    // between `Electronic` (0.8) and any rare tag (>4.0), quietly mis-weighing both ends.
    for (_, name, count) in &seed_tag_rows {
        idf_of.entry(name.to_lowercase()).or_insert_with(|| idf(*count, total));
    }
    for c in pool.values_mut() {
        c.tags = cand_tags.get(&c.track_id).cloned().unwrap_or_default();
    }
    let pool_size = pool.len() as i64;

    let ranked = rank(pool.values(), &seed, &signals, &idf_of, limit, offset, shuffle);
    if ranked.is_empty() {
        return Ok(empty(pool_size));
    }

    let win: Vec<i64> = ranked.iter().map(|s| s.candidate.track_id).collect();
    let mut briefs = crate::hydrate::briefs(conn, &win)?;
    let items = ranked
        .into_iter()
        .filter_map(|s| Some(SimilarItem { track: briefs.remove(&s.candidate.track_id)?, score: s.score, why: s.why }))
        .collect();
    Ok(SimilarResponse {
        items,
        pool_size,
        ..empty(pool_size)
    })
}

/// In-process entry: `POST /suggest/similar`.
pub fn suggest(db: &bc_db::Db, scope: &Scope, req: &SimilarRequest) -> Result<SimilarResponse<TrackOut>> {
    crate::error::read(db, |c| run(c, scope, req))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idf_map() -> HashMap<String, f64> {
        [("electronic", 0.2), ("techno", 1.0), ("hypnotic techno", 3.0), ("dub techno", 3.0)].iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    fn set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|x| x.to_string()).collect()
    }

    fn seed_t(tags: &[&str]) -> Seed {
        Seed { tags: set(tags), ..Default::default() }
    }

    fn c(id: i64, tags: &[&str]) -> Candidate {
        Candidate { track_id: id, tags: set(tags), ..Default::default() }
    }

    fn all() -> Signals {
        Signals::default()
    }

    fn rk(cands: &[Candidate], seed: &Seed, signals: &Signals, limit: usize, offset: usize) -> Vec<i64> {
        rank(cands.iter(), seed, signals, &idf_map(), limit, offset, 0).iter().map(|s| s.candidate.track_id).collect()
    }

    fn why(cand: &Candidate, seed: &Seed) -> Vec<String> {
        score(cand, seed, &all(), &idf_map()).why
    }

    fn sc(cand: &Candidate, seed: &Seed, signals: &Signals) -> f64 {
        score(cand, seed, signals, &idf_map()).score
    }

    fn sig(tags: bool, artist: bool, label: bool, tempo: bool, key: bool, loved: bool) -> Signals {
        Signals { tags, artist, label, tempo, key, loved }
    }

    #[test]
    fn same_artist_outranks_an_equally_tagged_stranger() {
        let seed = Seed { artist_id: Some(7), ..seed_t(&["techno"]) };
        let mut mine = c(1, &["techno"]);
        mine.artist_id = Some(7);
        let mut stranger = c(2, &["techno"]);
        stranger.artist_id = Some(9);
        assert_eq!(rk(&[stranger, mine.clone()], &seed, &all(), 10, 0), [1, 2]);
        assert!(why(&mine, &seed).contains(&"same artist".to_string()));
    }

    #[test]
    fn same_release_is_not_penalised() {
        let seed = Seed { artist_id: Some(7), release_id: Some(3), ..seed_t(&["techno"]) };
        let mut same_record = c(1, &["techno"]);
        same_record.artist_id = Some(7);
        same_record.release_id = Some(3);
        let mut other = c(2, &["techno"]);
        other.artist_id = Some(7);
        other.release_id = Some(4);
        assert_eq!(sc(&same_record, &seed, &all()), sc(&other, &seed, &all()));
    }

    #[test]
    fn same_label_lifts_a_track() {
        let seed = Seed { label_id: Some(5), ..seed_t(&["techno"]) };
        let mut mate = c(1, &["techno"]);
        mate.label_id = Some(5);
        let mut outsider = c(2, &["techno"]);
        outsider.label_id = Some(6);
        assert_eq!(rk(&[outsider, mate.clone()], &seed, &all(), 10, 0), [1, 2]);
        assert!(why(&mate, &seed).contains(&"same label".to_string()));
    }

    #[test]
    fn a_rare_shared_tag_beats_a_common_one() {
        let seed = seed_t(&["electronic", "hypnotic techno"]);
        assert_eq!(rk(&[c(2, &["electronic"]), c(1, &["hypnotic techno"])], &seed, &all(), 10, 0), [1, 2]);
    }

    #[test]
    fn why_names_the_telling_tags_first_as_the_candidate_spells_them() {
        let seed = seed_t(&["electronic", "Hypnotic Techno"]);
        let cand = c(1, &["Electronic", "Hypnotic Techno"]);
        assert_eq!(why(&cand, &seed)[0], "shares: Hypnotic Techno, Electronic");
    }

    #[test]
    fn turning_tempo_off_makes_two_tracks_differing_only_in_bpm_tie() {
        let seed = Seed { bpm: Some(128.0), ..seed_t(&["techno"]) };
        let mut near = c(1, &["techno"]);
        near.bpm = Some(128.0);
        let mut far = c(2, &["techno"]);
        far.bpm = Some(175.0);
        assert!(sc(&near, &seed, &all()) > sc(&far, &seed, &all()));
        let off = Signals { tempo: false, ..all() };
        assert_eq!(sc(&near, &seed, &off), sc(&far, &seed, &off));
    }

    #[test]
    fn label_alone_ranks_labelmates_and_drops_everyone_else() {
        let seed = Seed { label_id: Some(5), artist_id: Some(7), bpm: Some(128.0), camelot: Some("8A".into()), ..seed_t(&["techno"]) };
        let mut mate = c(1, &["ambient"]);
        mate.label_id = Some(5);
        mate.bpm = Some(90.0);
        mate.camelot = Some("3B".into());
        let mut outsider = c(2, &["techno"]);
        outsider.label_id = Some(6);
        outsider.artist_id = Some(7);
        outsider.bpm = Some(128.0);
        outsider.camelot = Some("8A".into());
        assert_eq!(rk(&[outsider, mate], &seed, &sig(false, false, true, false, false, false), 10, 0), [1]);
    }

    #[test]
    fn scores_stay_comparable_across_toggle_states() {
        let seed = Seed { artist_id: Some(7), label_id: Some(5), bpm: Some(128.0), camelot: Some("8A".into()), ..seed_t(&["techno"]) };
        let twin = Candidate { artist_id: Some(7), label_id: Some(5), bpm: Some(128.0), camelot: Some("8A".into()), ..c(1, &["techno"]) };
        assert_eq!(sc(&twin, &seed, &Signals { loved: false, ..all() }), 1.0);
        assert_eq!(sc(&twin, &seed, &sig(true, false, false, true, true, false)), 1.0);
    }

    #[test]
    fn a_seed_with_no_label_still_returns_rows_with_label_on() {
        let seed = seed_t(&["techno"]);
        let mut cand = c(1, &["techno"]);
        cand.label_id = Some(5);
        assert_eq!(rk(&[cand], &seed, &Signals { label: true, ..all() }, 10, 0), [1]);
    }

    #[test]
    fn an_unanalysed_seed_still_returns_rows() {
        let seed = Seed { artist_id: Some(7), ..seed_t(&["hypnotic techno"]) };
        let mut cand = c(1, &["hypnotic techno"]);
        cand.artist_id = Some(9);
        cand.bpm = Some(128.0);
        cand.camelot = Some("8A".into());
        assert_eq!(rk(std::slice::from_ref(&cand), &seed, &all(), 10, 0), [1]);
        assert!(sc(&cand, &seed, &all()) > 0.0);
    }

    #[test]
    fn a_candidate_sharing_nothing_is_dropped() {
        assert!(rk(&[c(1, &["ambient"])], &seed_t(&["hypnotic techno"]), &all(), 10, 0).is_empty());
    }

    #[test]
    fn every_signal_off_falls_back_to_tags_rather_than_emptying() {
        let none_on = sig(false, false, false, false, false, false);
        assert_eq!(rk(&[c(1, &["hypnotic techno"])], &seed_t(&["hypnotic techno"]), &none_on, 10, 0), [1]);
    }

    #[test]
    fn a_seed_with_nothing_at_all_scores_nothing() {
        assert!(rk(&[c(1, &["techno"])], &Seed::default(), &all(), 10, 0).is_empty());
    }

    #[test]
    fn an_unanalysed_candidate_scores_neutral_not_zero_on_tempo() {
        let seed = Seed { bpm: Some(128.0), ..seed_t(&["techno"]) };
        let unknown = c(1, &["techno"]);
        let mut clashing = c(2, &["techno"]);
        clashing.bpm = Some(175.0);
        assert!(sc(&unknown, &seed, &all()) > sc(&clashing, &seed, &all()));
    }

    #[test]
    fn loved_lifts_a_match_but_cannot_rescue_a_non_match() {
        let seed = Seed { label_id: Some(5), ..seed_t(&["hypnotic techno"]) };
        let mut loved_match = c(1, &["hypnotic techno"]);
        loved_match.loved = true;
        let plain = c(2, &["hypnotic techno"]);
        let mut loved_stranger = c(3, &["ambient"]);
        loved_stranger.label_id = Some(6);
        loved_stranger.loved = true;
        assert_eq!(rk(&[plain, loved_match.clone(), loved_stranger], &seed, &all(), 10, 0), [1, 2]);
        assert!(why(&loved_match, &seed).contains(&"loved".to_string()));
    }

    #[test]
    fn the_loved_bonus_can_be_switched_off() {
        let seed = seed_t(&["techno"]);
        let mut loved = c(1, &["techno"]);
        loved.loved = true;
        let plain = c(2, &["techno"]);
        let off = Signals { loved: false, ..all() };
        assert_eq!(sc(&loved, &seed, &off), sc(&plain, &seed, &off));
    }

    #[test]
    fn one_release_cannot_flood_the_panel() {
        let crowd: Vec<Candidate> = (1..6).map(|i| Candidate { release_id: Some(1), artist_id: Some(i), ..c(i, &["techno"]) }).collect();
        assert_eq!(rk(&crowd, &seed_t(&["techno"]), &all(), 10, 0).len(), MAX_PER_RELEASE);
    }

    #[test]
    fn one_artist_is_capped_while_tags_are_ranking() {
        let crowd: Vec<Candidate> = (1..9).map(|i| Candidate { release_id: Some(i), artist_id: Some(1), ..c(i, &["techno"]) }).collect();
        assert_eq!(rk(&crowd, &seed_t(&["techno"]), &all(), 10, 0).len(), MAX_PER_ARTIST);
    }

    #[test]
    fn the_artist_cap_lifts_when_ranking_by_artist() {
        let seed = Seed { artist_id: Some(1), ..seed_t(&["techno"]) };
        let crowd: Vec<Candidate> = (1..9).map(|i| Candidate { release_id: Some(i), artist_id: Some(1), ..c(i, &["techno"]) }).collect();
        assert_eq!(rk(&crowd, &seed, &Signals { tags: false, ..all() }, 10, 0).len(), 8);
    }

    #[test]
    fn offset_pages_without_overlap() {
        let crowd: Vec<Candidate> = (1..7).map(|i| Candidate { release_id: Some(i), artist_id: Some(i), ..c(i, &["techno"]) }).collect();
        let seed = seed_t(&["techno"]);
        let first = rk(&crowd, &seed, &all(), 3, 0);
        let second = rk(&crowd, &seed, &all(), 3, 3);
        assert!(first.len() == 3 && second.len() == 3);
        assert!(first.iter().all(|i| !second.contains(i)));
    }

    #[test]
    fn tempo_proximity_decays_instead_of_falling_off_a_cliff() {
        let near = tempo_proximity(Some(128.0), Some(128.0)).unwrap();
        let edge = tempo_proximity(Some(128.0), Some(138.0)).unwrap();
        let far = tempo_proximity(Some(128.0), Some(175.0)).unwrap();
        assert_eq!(near, 1.0);
        assert!(near > edge && edge > far && far > 0.0);
    }

    #[test]
    fn tempo_proximity_counts_half_and_double_time() {
        let far = tempo_proximity(Some(128.0), Some(175.0)).unwrap();
        assert!(tempo_proximity(Some(128.0), Some(256.0)).unwrap() > far);
        assert!(tempo_proximity(Some(128.0), Some(64.0)).unwrap() > far);
    }

    #[test]
    fn tempo_proximity_is_unknown_when_either_side_is() {
        assert_eq!(tempo_proximity(Some(128.0), None), None);
        assert_eq!(tempo_proximity(None, Some(128.0)), None);
        assert_eq!(tempo_proximity(Some(0.0), Some(128.0)), None);
    }

    #[test]
    fn a_slightly_off_tempo_beats_a_wildly_off_one() {
        let seed = Seed { bpm: Some(128.0), ..seed_t(&["techno"]) };
        let mut close = c(1, &["techno"]);
        close.bpm = Some(138.0);
        let mut distant = c(2, &["techno"]);
        distant.bpm = Some(175.0);
        assert!(sc(&close, &seed, &all()) > sc(&distant, &seed, &all()));
    }

    #[test]
    fn a_compatible_key_and_tempo_alone_do_not_get_a_track_in() {
        let seed = Seed { artist_id: Some(1), label_id: Some(1), bpm: Some(128.0), camelot: Some("8A".into()), ..seed_t(&["hypnotic techno"]) };
        let pad = Candidate { artist_id: Some(2), label_id: Some(2), bpm: Some(128.0), camelot: Some("8A".into()), ..c(9, &["polka"]) };
        assert!(!score(&pad, &seed, &all(), &idf_map()).evidence);
        assert!(rk(&[pad], &seed, &all(), 10, 0).is_empty());
    }

    #[test]
    fn one_shared_tag_is_evidence_enough() {
        let seed = Seed { artist_id: Some(1), bpm: Some(128.0), camelot: Some("8A".into()), ..seed_t(&["techno"]) };
        let cand = Candidate { artist_id: Some(2), bpm: Some(175.0), camelot: Some("2B".into()), ..c(1, &["techno"]) };
        assert_eq!(rk(&[cand], &seed, &all(), 10, 0), [1]);
    }

    #[test]
    fn the_artist_alone_is_evidence_when_nothing_else_matches() {
        let seed = Seed { artist_id: Some(1), bpm: Some(128.0), camelot: Some("8A".into()), ..seed_t(&["techno"]) };
        let cand = Candidate { artist_id: Some(1), bpm: Some(90.0), camelot: Some("3B".into()), ..c(1, &["ambient"]) };
        assert_eq!(rk(&[cand], &seed, &all(), 10, 0), [1]);
    }

    fn crowd24() -> Vec<Candidate> {
        (1..25).map(|i| Candidate { artist_id: Some(i), release_id: Some(i), ..c(i, &["techno"]) }).collect()
    }

    fn ids(r: Vec<Scored>) -> Vec<i64> {
        r.iter().map(|s| s.candidate.track_id).collect()
    }

    #[test]
    fn reroll_reorders_the_close_calls() {
        let seed = seed_t(&["techno"]);
        let crowd = crowd24();
        let plain = rk(&crowd, &seed, &all(), 8, 0);
        let rerolled = ids(rank(crowd.iter(), &seed, &all(), &idf_map(), 8, 0, 3));
        assert_ne!(plain, rerolled);
    }

    #[test]
    fn the_same_reroll_gives_the_same_page() {
        let seed = seed_t(&["techno"]);
        let crowd = crowd24();
        let once = ids(rank(crowd.iter(), &seed, &all(), &idf_map(), 8, 0, 7));
        let again = ids(rank(crowd.iter(), &seed, &all(), &idf_map(), 8, 0, 7));
        assert_eq!(once, again);
    }

    #[test]
    fn a_reroll_cannot_promote_a_poor_match_over_a_strong_one() {
        let seed = seed_t(&["hypnotic techno", "electronic"]);
        let strong = Candidate { artist_id: Some(1), ..c(1, &["hypnotic techno", "electronic"]) };
        let weak = Candidate { artist_id: Some(2), ..c(2, &["electronic"]) };
        for shuffle in 1..25 {
            let r = rank([&strong, &weak], &seed, &all(), &idf_map(), 2, 0, shuffle);
            assert_eq!(r[0].candidate.track_id, 1, "shuffle={shuffle} demoted the strong match");
        }
    }
}
