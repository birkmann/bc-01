//! More like what you love: suggestions drawn from the loved shelf as a whole (port of
//! `recommend/taste.py` and the `GET /suggest/loved` pipeline).
//!
//! The loved tracks are boiled down to a *profile* (IDF-weighted tags, artists and labels that
//! keep turning up, tempo median, mean energy). Every not-yet-loved candidate is scored against
//! it; ranking thins out any one artist or release.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use bc_db::rusqlite::Connection;
use bc_types::suggest::{LovedProfileOut, LovedQuery, LovedSuggestResponse, LovedSuggestionOut, TagWeightOut };

use bc_types::library::TrackOut;
use crate::error::Result;
use crate::nextup::{bpm_windows, idf};
use crate::pooling;
use crate::scope::{Scope, ScopeExt};
use crate::sqlutil::{PRESENT, in_list, name_key_list, round4, tagged_tracks_sql};

pub const W_TAGS: f64 = 0.40;
pub const W_ARTIST: f64 = 0.22;
pub const W_LABEL: f64 = 0.13;
pub const W_TEMPO: f64 = 0.17;
pub const W_ENERGY: f64 = 0.08;
pub const BONUS_UNPLAYED: f64 = 0.03;

/// How many loved tracks by an artist (or on a label) count as "a favourite".
pub const FAMILIAR_AT: i64 = 3;
pub const BPM_TOLERANCE: f64 = 0.06;
pub const MAX_PER_ARTIST: usize = 3;
pub const MAX_PER_RELEASE: usize = 2;
/// Tags shown in the profile.
pub const TOP_TAGS: usize = 12;

pub const POOL_BY_PEOPLE: i64 = 1500;
pub const POOL_BY_TAGS: i64 = 2500;
/// Profile tags used to draw the pool -- wider than what is shown.
pub const POOL_TAGS: usize = 30;

/// What the profile needs to know about one loved track (also a playlist track).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LovedRow {
    pub track_id: i64,
    pub tags: BTreeSet<String>,
    pub artist_id: Option<i64>,
    pub label_id: Option<i64>,
    pub bpm: Option<f64>,
    pub energy: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Candidate {
    pub track_id: i64,
    pub tags: BTreeSet<String>,
    pub artist_id: Option<i64>,
    pub release_id: Option<i64>,
    pub label_id: Option<i64>,
    pub bpm: Option<f64>,
    pub energy: Option<f64>,
    pub play_count: i64,
}

/// The loved shelf, summarised.
#[derive(Debug, Clone, Default)]
pub struct Profile {
    pub count: usize,
    /// tag (casefolded) -> how many loved tracks carry it, times IDF.
    pub tag_weight: BTreeMap<String, f64>,
    /// For display: the tags as written, most telling first, weights relative to the peak.
    pub top_tags: Vec<(String, f64)>,
    pub artist_count: BTreeMap<i64, i64>,
    pub label_count: BTreeMap<i64, i64>,
    pub bpms: Vec<f64>,
    pub energy: Option<f64>,
}

impl Profile {
    /// Where the tempo sits: the median of the analysed loved tracks.
    pub fn bpm(&self) -> Option<f64> {
        if self.bpms.is_empty() {
            return None;
        }
        let mut s = self.bpms.clone();
        s.sort_by(f64::total_cmp);
        let mid = s.len() / 2;
        Some(if s.len() % 2 == 1 { s[mid] } else { (s[mid - 1] + s[mid]) / 2.0 })
    }
}

#[derive(Debug, Clone)]
pub struct Scored {
    pub candidate: Candidate,
    pub score: f64,
    pub why: Vec<String>,
}

fn norm(t: &str) -> String {
    t.trim().to_lowercase()
}

/// Boil the loved tracks down. `idf_of` maps casefolded tag -> IDF.
pub fn build_profile(rows: &[LovedRow], idf_of: &HashMap<String, f64>) -> Profile {
    let mut tag_hits: BTreeMap<String, i64> = BTreeMap::new();
    let mut spelling: BTreeMap<String, String> = BTreeMap::new();
    let mut artists: BTreeMap<i64, i64> = BTreeMap::new();
    let mut labels: BTreeMap<i64, i64> = BTreeMap::new();
    let mut bpms = vec![];
    let mut energies = vec![];
    for r in rows {
        for t in &r.tags {
            let k = norm(t);
            if k.is_empty() {
                continue;
            }
            *tag_hits.entry(k.clone()).or_default() += 1;
            spelling.entry(k).or_insert_with(|| t.trim().to_string());
        }
        if let Some(a) = r.artist_id {
            *artists.entry(a).or_default() += 1;
        }
        if let Some(l) = r.label_id {
            *labels.entry(l).or_default() += 1;
        }
        if let Some(b) = r.bpm.filter(|b| *b != 0.0) {
            bpms.push(b);
        }
        if let Some(e) = r.energy {
            energies.push(e);
        }
    }
    let tag_weight: BTreeMap<String, f64> =
        tag_hits.iter().map(|(k, n)| (k.clone(), *n as f64 * idf_of.get(k).copied().unwrap_or(1.0))).collect();
    let mut sorted: Vec<(&String, &f64)> = tag_weight.iter().collect();
    sorted.sort_by(|a, b| b.1.total_cmp(a.1).then(a.0.cmp(b.0)));
    sorted.truncate(TOP_TAGS);
    let peak = sorted.first().map(|x| *x.1).unwrap_or(1.0);
    let top_tags = sorted.iter().map(|(k, w)| (spelling[*k].clone(), **w / peak)).collect();
    Profile {
        count: rows.len(),
        tag_weight,
        top_tags,
        artist_count: artists,
        label_count: labels,
        bpms,
        energy: if energies.is_empty() { None } else { Some(energies.iter().sum::<f64>() / energies.len() as f64) },
    }
}

/// Weighted cosine between a track's tags and the profile's tag vector, 0..1.
pub fn profile_tag_fit(tags: &BTreeSet<String>, profile: &Profile) -> f64 {
    let mine: BTreeSet<String> = tags.iter().map(|t| norm(t)).filter(|t| !t.is_empty()).collect();
    if mine.is_empty() || profile.tag_weight.is_empty() {
        return 0.0;
    }
    let shared: f64 = mine.iter().map(|t| profile.tag_weight.get(t).copied().unwrap_or(0.0).powi(2)).sum();
    if shared == 0.0 {
        return 0.0;
    }
    // Both sides in profile weights, so a candidate cannot win by carrying every tag.
    let norm_c = mine.iter().map(|t| profile.tag_weight.get(t).copied().unwrap_or(1.0).powi(2)).sum::<f64>().sqrt();
    let norm_p = profile.tag_weight.values().map(|w| w * w).sum::<f64>().sqrt();
    (shared / (norm_c * norm_p)).min(1.0)
}

/// The share of loved tracks whose tempo this one could sit beside (half and double time
/// count). `None` when either side is unanalysed.
pub fn tempo_fit(bpm: Option<f64>, profile: &Profile) -> Option<f64> {
    let bpm = bpm.filter(|b| *b != 0.0)?;
    if profile.bpms.is_empty() {
        return None;
    }
    let windows = bpm_windows(bpm, BPM_TOLERANCE);
    let near = profile.bpms.iter().filter(|b| windows.iter().any(|(lo, hi)| lo <= *b && *b <= hi)).count();
    Some(near as f64 / profile.bpms.len() as f64)
}

fn familiar(n: i64) -> f64 {
    (n as f64 / FAMILIAR_AT as f64).min(1.0)
}

pub fn score(c: &Candidate, profile: &Profile) -> Scored {
    let mut why = vec![];
    let mut total = 0.0;

    let tag_fit = profile_tag_fit(&c.tags, profile);
    total += W_TAGS * tag_fit;
    if tag_fit > 0.0 {
        let mut hits: Vec<(f64, &String)> = c.tags.iter().map(|t| (profile.tag_weight.get(&norm(t)).copied().unwrap_or(0.0), t)).collect();
        hits.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(b.1)));
        let named: Vec<&str> = hits.iter().filter(|(w, _)| *w > 0.0).take(3).map(|(_, t)| t.as_str()).collect();
        if !named.is_empty() {
            why.push(format!("tags: {}", named.join(", ")));
        }
    }

    let n_artist = c.artist_id.and_then(|a| profile.artist_count.get(&a).copied()).unwrap_or(0);
    if n_artist > 0 {
        total += W_ARTIST * familiar(n_artist);
        why.push(if n_artist > 1 { format!("{n_artist} loved by this artist") } else { "an artist you love".into() });
    }
    let n_label = c.label_id.and_then(|a| profile.label_count.get(&a).copied()).unwrap_or(0);
    if n_label > 0 {
        total += W_LABEL * familiar(n_label);
        why.push(if n_label > 1 { format!("{n_label} loved on this label") } else { "a label you love".into() });
    }

    match tempo_fit(c.bpm, profile) {
        None => total += W_TEMPO * 0.5,
        Some(t) => {
            total += W_TEMPO * t;
            if t >= 0.5
                && let Some(b) = c.bpm
            {
                why.push(format!("{:.0} BPM, like {:.0}% of loved", b, t * 100.0));
            }
        }
    }

    match (c.energy, profile.energy) {
        (Some(e), Some(p)) => total += W_ENERGY * (1.0 - (e - p).abs() / 0.5).max(0.0),
        _ => total += W_ENERGY * 0.5,
    }

    if c.play_count == 0 {
        total += BONUS_UNPLAYED;
        why.push("never played".into());
    }
    Scored { candidate: c.clone(), score: round4(total), why }
}

/// Score, sort, and thin: no artist takes more than [`MAX_PER_ARTIST`] rows, no release more
/// than [`MAX_PER_RELEASE`], so the page reads as a shelf and not one discography.
pub fn rank<'a>(candidates: impl IntoIterator<Item = &'a Candidate>, profile: &Profile, limit: usize) -> Vec<Scored> {
    let mut scored: Vec<Scored> = candidates.into_iter().map(|c| score(c, profile)).collect();
    scored.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.candidate.track_id.cmp(&b.candidate.track_id)));
    let mut out = vec![];
    let mut per_artist: HashMap<i64, usize> = HashMap::new();
    let mut per_release: HashMap<i64, usize> = HashMap::new();
    for s in scored {
        if s.score <= 0.0 {
            continue;
        }
        let (a, r) = (s.candidate.artist_id, s.candidate.release_id);
        if a.is_some_and(|a| per_artist.get(&a).copied().unwrap_or(0) >= MAX_PER_ARTIST) {
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
        if out.len() >= limit {
            break;
        }
    }
    out
}

// ======================================================================================
// The pipeline
// ======================================================================================

/// Light columns of a loved track (also reused for playlist profiles).
/// `(track id, track artist, release artist, label, bpm, energy)`
pub(crate) type LovedLight = (i64, Option<i64>, Option<i64>, Option<i64>, Option<f64>, Option<f64>);

pub(crate) fn loved_rows(conn: &Connection) -> Result<Vec<LovedLight>> {
    let mut st = conn.prepare(
        "SELECT t.id, t.artist_id, r.artist_id, r.label_id, a.bpm, a.energy FROM tracks t \
         LEFT JOIN releases r ON r.id = t.release_id LEFT JOIN analysis a ON a.track_id = t.id WHERE t.loved = 1",
    )?;
    let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

fn profile_out(p: &Profile) -> LovedProfileOut {
    LovedProfileOut {
        loved_count: p.count as i64,
        tags: p
            .top_tags
            .iter()
            .map(|(n, w)| TagWeightOut { name: n.clone(), weight: bc_music::setmath::round_to(*w, 3) })
            .collect(),
        bpm: p.bpm().map(|b| bc_music::setmath::round_to(b, 1)),
        energy: p.energy.map(|e| bc_music::setmath::round_to(e, 3)),
        artists: p.artist_count.len() as i64,
        labels: p.label_count.len() as i64,
    }
}

const POOL_COLS: &str = "t.id, t.artist_id, t.release_id, r.artist_id, r.label_id, t.play_count, a.bpm, a.energy";

type PoolRow = (i64, Option<i64>, Option<i64>, Option<i64>, Option<i64>, Option<i64>, Option<f64>, Option<f64>);

fn read_rows(conn: &Connection, sql: &str, into: &mut HashMap<i64, PoolRow>) -> Result<()> {
    let mut st = conn.prepare(sql)?;
    let rows = st.query_map([], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?))
    })?;
    for row in rows {
        let row = row?;
        into.entry(row.0).or_insert(row);
    }
    Ok(())
}

/// The statements of the loved pool, exposed for `EXPLAIN QUERY PLAN` tests. The "people"
/// branch is **three statements, not one `OR`** across tracks and releases (PLAN 9l).
#[doc(hidden)]
pub fn pool_statements(profile: &Profile, scope: &Scope, seed: i64, only_tags: &[String], tag_ids: &[i64]) -> Vec<String> {
    let base = format!(
        "SELECT {POOL_COLS} FROM tracks t LEFT JOIN releases r ON r.id = t.release_id LEFT JOIN analysis a ON a.track_id = t.id \
         WHERE t.loved = 0 AND {PRESENT} AND {}",
        scope.tp()
    );
    let mut base_filtered = base;
    if let Some(keys) = name_key_list(only_tags.iter()) {
        base_filtered.push_str(&format!(" AND t.id IN ({})", tagged_tracks_sql(&keys)));
    }
    let order = format!(" ORDER BY {}", pooling::shuffled(seed));
    let mut out = vec![];
    let artist_ids: Vec<i64> = profile.artist_count.keys().copied().collect();
    let label_ids: Vec<i64> = profile.label_count.keys().copied().collect();
    if !artist_ids.is_empty() {
        let l = in_list(&artist_ids);
        out.push(format!("{base_filtered} AND t.artist_id IN ({l}){order} LIMIT {POOL_BY_PEOPLE}"));
        out.push(format!(
            "{base_filtered} AND t.release_id IN (SELECT sr.id FROM releases sr WHERE sr.artist_id IN ({l})){order} LIMIT {POOL_BY_PEOPLE}"
        ));
    }
    if !label_ids.is_empty() {
        out.push(format!(
            "{base_filtered} AND t.release_id IN (SELECT sr.id FROM releases sr WHERE sr.label_id IN ({})){order} LIMIT {POOL_BY_PEOPLE}",
            in_list(&label_ids)
        ));
    }
    if !tag_ids.is_empty() {
        out.push(format!(
            "{base_filtered} AND t.id IN (SELECT tt.track_id FROM track_tags tt WHERE tt.tag_id IN ({})){order} LIMIT {POOL_BY_TAGS}",
            in_list(tag_ids)
        ));
    }
    out
}

/// The tag ids the pool is drawn from: the profile's heaviest tags (wider than what is shown),
/// walked rarest-first up to the cumulative track budget of PLAN 9l, so a library-wide tag such
/// as `Electronic` never makes the pool query sort most of the table (the rarest is always kept).
pub fn pool_tag_ids(conn: &Connection, profile: &Profile) -> Result<Vec<i64>> {
    let mut wider: Vec<(&String, &f64)> = profile.tag_weight.iter().collect();
    wider.sort_by(|a, b| b.1.total_cmp(a.1).then(a.0.cmp(b.0)));
    let names: Vec<String> = wider.into_iter().take(POOL_TAGS).map(|(k, _)| k.clone()).collect();
    if names.is_empty() {
        return Ok(vec![]);
    }
    let rows = pooling::tag_rows(conn, names.iter())?;
    Ok(pooling::drawing_tags(&rows, pooling::TAG_BUDGET).into_iter().map(|r| r.0).collect())
}

/// `GET /suggest/loved` on an open connection.
pub fn run(conn: &Connection, scope: &Scope, q: &LovedQuery) -> Result<LovedSuggestResponse<TrackOut>> {
    let limit = q.limit.clamp(1, 100) as usize;
    let rows = loved_rows(conn)?;
    let ids: Vec<i64> = rows.iter().map(|r| r.0).collect();
    let (loved_tags, tag_counts) = pooling::tags_by_track(conn, &ids)?;
    let total = pooling::total_tracks(conn)?;
    let idf_of: HashMap<String, f64> = tag_counts.iter().map(|(k, n)| (k.clone(), idf(*n, total))).collect();

    let loved: Vec<LovedRow> = rows
        .iter()
        .map(|(tid, t_artist, r_artist, label, bpm, energy)| LovedRow {
            track_id: *tid,
            tags: loved_tags.get(tid).cloned().unwrap_or_default(),
            artist_id: t_artist.or(*r_artist),
            label_id: *label,
            bpm: *bpm,
            energy: *energy,
        })
        .collect();
    let profile = build_profile(&loved, &idf_of);
    if profile.count == 0 {
        return Ok(LovedSuggestResponse { profile: profile_out(&profile), items: vec![] });
    }

    let only: Vec<String> = q.tags.split(',').map(|t| t.to_string()).filter(|t| !t.trim().is_empty()).collect();
    let mut pool: HashMap<i64, PoolRow> = HashMap::new();
    let tag_ids = pool_tag_ids(conn, &profile)?;
    for sql in pool_statements(&profile, scope, q.seed.max(0), &only, &tag_ids) {
        read_rows(conn, &sql, &mut pool)?;
    }
    if pool.is_empty() {
        return Ok(LovedSuggestResponse { profile: profile_out(&profile), items: vec![] });
    }

    let pool_ids: Vec<i64> = pool.keys().copied().collect();
    let (cand_tags, _) = pooling::tags_by_track(conn, &pool_ids)?;
    let candidates: Vec<Candidate> = pool
        .values()
        .map(|(tid, t_artist, release_id, r_artist, label, plays, bpm, energy)| Candidate {
            track_id: *tid,
            tags: cand_tags.get(tid).cloned().unwrap_or_default(),
            artist_id: t_artist.or(*r_artist),
            release_id: *release_id,
            label_id: *label,
            bpm: *bpm,
            energy: *energy,
            play_count: plays.unwrap_or(0),
        })
        .collect();
    let ranked = rank(candidates.iter(), &profile, limit);

    // Full rows only for the winners.
    let win: Vec<i64> = ranked.iter().map(|s| s.candidate.track_id).collect();
    let mut briefs = crate::hydrate::briefs(conn, &win)?;
    let items = ranked
        .into_iter()
        .filter_map(|s| Some(LovedSuggestionOut { track: briefs.remove(&s.candidate.track_id)?, score: s.score, why: s.why }))
        .collect();
    Ok(LovedSuggestResponse { profile: profile_out(&profile), items })
}

/// In-process entry: `GET /suggest/loved`.
pub fn suggest_loved(db: &bc_db::Db, scope: &Scope, q: &LovedQuery) -> Result<LovedSuggestResponse<TrackOut>> {
    crate::error::read(db, |c| run(c, scope, q))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idf_map() -> HashMap<String, f64> {
        [("electronic", 0.2), ("hardgroove", 3.0), ("techno", 1.0), ("ambient", 3.0)].iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    fn set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|x| x.to_string()).collect()
    }

    /// `(tags, artist, label, bpm, energy)`
    type LovedSpec<'a> = (&'a [&'a str], Option<i64>, Option<i64>, Option<f64>, Option<f64>);

    fn loved(rows: &[LovedSpec<'_>]) -> Profile {
        let rows: Vec<LovedRow> = rows
            .iter()
            .enumerate()
            .map(|(i, r)| LovedRow { track_id: i as i64 + 1, tags: set(r.0), artist_id: r.1, label_id: r.2, bpm: r.3, energy: r.4 })
            .collect();
        build_profile(&rows, &idf_map())
    }

    fn cand(id: i64, tags: &[&str]) -> Candidate {
        Candidate { track_id: id, tags: set(tags), ..Default::default() }
    }

    #[test]
    fn profile_counts_and_weights_tags_by_idf() {
        let p = loved(&[(&["electronic", "hardgroove"], Some(1), Some(10), Some(143.0), None), (&["electronic", "techno"], Some(1), Some(11), Some(145.0), None)]);
        assert_eq!(p.count, 2);
        // electronic appears twice but says little; hardgroove once and says a lot.
        assert!(p.tag_weight["hardgroove"] > p.tag_weight["electronic"]);
        assert_eq!(p.top_tags[0].0, "hardgroove");
        assert_eq!(p.artist_count, [(1, 2)].into());
        assert_eq!(p.label_count, [(10, 1), (11, 1)].into());
        assert_eq!(p.bpm(), Some(144.0));
    }

    #[test]
    fn telling_tags_beat_common_ones() {
        let p = loved(&[(&["electronic", "hardgroove"], None, None, None, None), (&["electronic", "hardgroove"], None, None, None, None)]);
        let a = score(&cand(1, &["hardgroove"]), &p);
        let b = score(&cand(2, &["electronic"]), &p);
        assert!(a.score > b.score);
        assert!(a.why.iter().any(|w| w.starts_with("tags: hardgroove")));
    }

    #[test]
    fn a_loved_artist_and_label_lift_a_candidate() {
        let row: LovedSpec = (&["techno"], Some(7), Some(3), None, None);
        let p = loved(&[row, row, row]);
        let mut same = cand(1, &["techno"]);
        same.artist_id = Some(7);
        same.label_id = Some(3);
        let mut other = cand(2, &["techno"]);
        other.artist_id = Some(8);
        other.label_id = Some(4);
        let (same, other) = (score(&same, &p), score(&other, &p));
        assert!(same.score > other.score);
        assert!(same.why.contains(&"3 loved by this artist".to_string()));
        assert!(same.why.contains(&"3 loved on this label".to_string()));
    }

    #[test]
    fn tempo_fit_is_the_share_of_loved_nearby_with_half_time() {
        let p = loved(&[(&["techno"], None, None, Some(140.0), None), (&["techno"], None, None, Some(142.0), None), (&["techno"], None, None, Some(100.0), None)]);
        assert_eq!(tempo_fit(Some(141.0), &p), Some(2.0 / 3.0));
        assert_eq!(tempo_fit(Some(70.0), &p), Some(2.0 / 3.0)); // half time counts
        assert_eq!(tempo_fit(None, &p), None);
    }

    #[test]
    fn rank_thins_one_artist_and_one_release() {
        let p = loved(&[(&["hardgroove"], Some(1), None, None, None)]);
        let mut cands: Vec<Candidate> = (0..10)
            .map(|i| Candidate { track_id: i, tags: set(&["hardgroove"]), artist_id: Some(1), release_id: Some(i % 2), ..Default::default() })
            .collect();
        cands.push(Candidate { track_id: 99, tags: set(&["hardgroove"]), artist_id: Some(2), release_id: Some(50), ..Default::default() });
        let out = rank(cands.iter(), &p, 10);
        assert!(out.iter().filter(|s| s.candidate.artist_id == Some(1)).count() <= MAX_PER_ARTIST);
        assert!(out.iter().any(|s| s.candidate.track_id == 99));
        let rel: Vec<i64> = out.iter().filter(|s| s.candidate.artist_id == Some(1)).filter_map(|s| s.candidate.release_id).collect();
        assert!(rel.iter().all(|r| rel.iter().filter(|x| *x == r).count() <= MAX_PER_RELEASE));
    }

    #[test]
    fn nothing_in_common_scores_out() {
        let p = loved(&[(&["hardgroove"], Some(1), None, None, None)]);
        let mut c = cand(1, &["ambient"]);
        c.play_count = 5;
        let out = rank([&c], &p, 5);
        // Neutral tempo/energy terms alone are not a recommendation... but they are not zero
        // either; the row survives with a low score.
        assert!(!out.is_empty() && out[0].score < 0.2);
        assert!(out[0].why.is_empty());
    }
}
