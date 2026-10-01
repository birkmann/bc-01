//! Finding the Bandcamp page a tracklist row is talking about.
//!
//! Split deliberately in two: [`score_hit`] and [`rank`] are pure and hold
//! every judgement this module makes, and [`match_row`] is the thin async part
//! that decides how many rate-limited searches that judgement is worth.
//!
//! The house rule elsewhere in the library is *never guess*. This is a looser
//! problem: the CSV is a human transcription of a radio show, Bandcamp is a
//! shop, and the two agree on the spelling of a remix suffix perhaps half the
//! time. So this ranks rather than decides, and hands anything short of
//! convincing to the review table.

use std::collections::HashSet;
use std::sync::LazyLock;

use regex::Regex;

use super::difflib;
use super::parse::{TrackRow, name_key};
use super::{SearchHit, SearchKind, Searcher};
use crate::error::HarvestError;

pub const STRONG: f64 = 0.88;
/// Below this nothing is pre-selected: the row goes to the human, and the
/// search escalates to its next query rather than settling for what it has.
pub const LIKELY: f64 = 0.70;

/// How far the winner must clear the runner-up to be pre-selected.
const MARGIN: f64 = 0.05;
/// How many alternates the review table's dropdown offers.
const CANDIDATES: usize = 8;
const ARTIST_FLOOR: f64 = 0.5;
/// A hit whose artist is not even plausible cannot rise above `weak`, however
/// perfect its title ("2AM" by anyone scored 0.72 against "Boogie Vice - 2AM").
const WEAK_CAP: f64 = 0.6;
/// What it is worth to find the artist's name inside the track's own title.
const ARTIST_IN_TITLE: f64 = 0.9;
/// What the artist comparison is worth when the hit's band name *is* the record
/// label: Bandcamp's `subtitle` for a track is the band, and on a label-run
/// page that is the label, so there is no artist to agree or disagree with.
const NEUTRAL_ARTIST: f64 = 0.6;

/// Default `limit` passed to the searcher.
#[allow(dead_code)]
pub const DEFAULT_LIMIT: usize = 20;

#[derive(Debug, Clone, PartialEq)]
pub struct ScoredHit {
    pub hit: SearchHit,
    pub score: f64,
    /// `strong` | `likely` | `weak` -- the badge the review table shows.
    pub tier: &'static str,
    /// The CSV's record label agrees. A tiebreaker, never part of the score.
    pub label_match: bool,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct MatchResult {
    pub query: String,
    pub candidates: Vec<ScoredHit>,
    pub best_index: Option<usize>,
    /// Rate-limited requests spent, so the caller can explain a slow pass.
    pub searches: usize,
}

// ---------------------------------------------------------------------------
// Scoring (pure)
// ---------------------------------------------------------------------------

static BRACKETED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s*[(\[][^()\[\]]*[)\]]\s*$").expect("static regex"));
static DASH_SUFFIX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\s+-\s+[^-]*\b(?:mix|remix|edit|dub|version|rework|refix|vip|instrumental)\b[^-]*$")
        .expect("static regex")
});
static NON_SLUG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[^a-z0-9]").expect("static regex"));

/// The title with its mix suffix removed. "2AM (Extended Mix)" and "2AM" are
/// the same record. Trailing groups are stripped repeatedly because "Chicago
/// (DJ Pierre Chicago Club Mix) (Remastered)" is a real shape.
pub fn base_title(title: &str) -> String {
    let mut out = title.trim().to_string();
    loop {
        let stripped = BRACKETED.replace_all(&out, "").trim().to_string();
        if stripped == out || stripped.is_empty() {
            break;
        }
        out = stripped;
    }
    let without_dash = DASH_SUFFIX.replace_all(&out, "").trim().to_string();
    if without_dash.is_empty() { out } else { without_dash }
}

fn has_suffix(title: &str) -> bool {
    name_key(&base_title(title)) != name_key(title)
}

/// An artist without its disambiguating bracket: "Pinto (NYC)" -> "Pinto".
/// For the *search text* only.
pub fn bare_name(name: &str) -> String {
    let bare = BRACKETED.replace_all(name.trim(), "").trim().to_string();
    if bare.is_empty() { name.trim().to_string() } else { bare }
}

/// Similarity of two already-folded names, 0..1. Containment scores high on
/// purpose ("Pinto (NYC)" vs "Pinto"); short folds are exempt.
fn ratio(left: &str, right: &str) -> f64 {
    if left.is_empty() || right.is_empty() {
        return 0.0;
    }
    if left == right {
        return 1.0;
    }
    let r = difflib::ratio(left, right);
    let (shorter, longer) = if right.chars().count() < left.chars().count() { (right, left) } else { (left, right) };
    if shorter.chars().count() >= 4 && longer.contains(shorter) {
        return r.max(0.85);
    }
    r
}

/// How much of `wanted`'s wording is present in `found`, 0..1. Recall rather
/// than a symmetric measure: the extra words are usually the shop's.
fn token_recall(wanted: &str, found: &str) -> f64 {
    let wanted_words: HashSet<&str> = wanted.split_whitespace().collect();
    let found_words: HashSet<&str> = found.split_whitespace().collect();
    if wanted_words.is_empty() {
        return 0.0;
    }
    wanted_words.intersection(&found_words).count() as f64 / wanted_words.len() as f64
}

fn slug(value: &str) -> String {
    NON_SLUG.replace_all(&name_key(value), "").into_owned()
}

/// Python `urlparse(url).netloc`.
fn netloc(url: &str) -> &str {
    let mut rest = url.trim_start_matches(|c: char| c <= ' ');
    if let Some(colon) = rest.find(':') {
        let scheme = &rest[..colon];
        let mut cs = scheme.chars();
        if cs.next().is_some_and(|c| c.is_ascii_alphabetic())
            && cs.all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
        {
            rest = &rest[colon + 1..];
        }
    }
    let Some(after) = rest.strip_prefix("//") else { return "" };
    let end = after.find(['/', '?', '#']).unwrap_or(after.len());
    &after[..end]
}

/// Whether the CSV's record label is the Bandcamp page we are looking at
/// (`clonerecords.bandcamp.com`, or the page's band is the label).
fn label_matches(row: &TrackRow, hit: &SearchHit) -> bool {
    let label = slug(&row.label);
    if label.chars().count() < 4 {
        return false;
    }
    let host = netloc(&hit.url).split('.').next().unwrap_or("");
    label == slug(&hit.subtitle) || slug(host).contains(&label)
}

/// How well the hit's band answers the row's artist. Neither confirms nor
/// denies when the band is the label; only ever raises the score.
fn artist_score(row: &TrackRow, hit: &SearchHit) -> f64 {
    let artist_fold = name_key(&row.artist);
    let direct = ratio(&artist_fold, &name_key(&hit.subtitle));
    if direct >= NEUTRAL_ARTIST {
        return direct;
    }
    // Label uploads pack the credit into the track name --
    // "Tuccillo, The Checkup - Aguas Congas" under the band "The Checkup".
    if artist_fold.chars().count() >= 4 && name_key(&hit.name).contains(&artist_fold) {
        return direct.max(ARTIST_IN_TITLE);
    }
    let label = slug(&row.label);
    if !label.is_empty() && label == slug(&hit.subtitle) {
        return NEUTRAL_ARTIST;
    }
    direct
}

/// Python `round(x, 4)` (round-half-even on the exact decimal value).
fn round4(x: f64) -> f64 {
    format!("{x:.4}").parse().unwrap_or(x)
}

/// How well one search hit answers one tracklist row, 0..1. Names only; the
/// label agreeing is handled in [`rank`] as a tiebreaker.
pub fn score_hit(row: &TrackRow, hit: &SearchHit) -> f64 {
    let artist = artist_score(row, hit);
    let title_full = ratio(&name_key(&row.title), &name_key(&hit.name));
    let mut title = title_full;

    // Suffix-stripping rescues one case only: a shop that writes the plain
    // title where the tracklist wrote the mix. When *both* sides name a mix and
    // the names differ, that difference is the whole point.
    if !(has_suffix(&row.title) && has_suffix(&hit.name)) {
        let title_base = ratio(&name_key(&base_title(&row.title)), &name_key(&base_title(&hit.name)));
        // Still discounted: a match that needed the strip is a weaker claim.
        title = title_full.max(0.95 * title_base);
    }

    // Held down to what the wording supports. Skipped for one-word titles,
    // where tokenising is unreliable ("2AM" vs "2 AM").
    let wanted = name_key(&row.title);
    let found = name_key(&hit.name);
    if wanted.split_whitespace().count() > 1 && found.split_whitespace().count() > 1 {
        title = title.min(token_recall(&wanted, &found));
    }

    let mut score = 0.55 * title + 0.45 * artist;
    if artist < ARTIST_FLOOR {
        score = score.min(WEAK_CAP);
    }
    round4(score)
}

pub fn tier_of(score: f64) -> &'static str {
    if score >= STRONG {
        "strong"
    } else if score >= LIKELY {
        "likely"
    } else {
        "weak"
    }
}

/// Every hit scored, best first, releases only. Ties break on the label
/// agreeing, then towards `track` hits, then by name.
pub fn rank(row: &TrackRow, hits: &[SearchHit]) -> Vec<ScoredHit> {
    let mut scored: Vec<ScoredHit> = hits
        .iter()
        .filter(|h| h.kind == "album" || h.kind == "track")
        .map(|h| {
            let score = score_hit(row, h);
            ScoredHit { hit: h.clone(), score, tier: tier_of(score), label_match: label_matches(row, h) }
        })
        .collect();
    scored.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| (!a.label_match).cmp(&!b.label_match))
            .then_with(|| (a.hit.kind != "track").cmp(&(b.hit.kind != "track")))
            .then_with(|| a.hit.name.cmp(&b.hit.name))
    });
    scored
}

/// One record, however the two pages spell its mix suffix.
fn same_recording(left: &SearchHit, right: &SearchHit) -> bool {
    (name_key(&left.subtitle), name_key(&base_title(&left.name)))
        == (name_key(&right.subtitle), name_key(&base_title(&right.name)))
}

/// Which candidate to pre-select, or `None` to make the user choose. A winner
/// has to clear `LIKELY` *and* clear the runner-up by a margin -- unless the
/// runner-up is the same recording offered twice (track page + its album).
pub fn pick_best(candidates: &[ScoredHit]) -> Option<usize> {
    let best = candidates.first()?;
    if best.score < LIKELY {
        return None;
    }
    if let Some(runner) = candidates.get(1) {
        if !same_recording(&best.hit, &runner.hit) && best.score - runner.score < MARGIN {
            return None;
        }
    }
    Some(0)
}

// ---------------------------------------------------------------------------
// Searching
// ---------------------------------------------------------------------------

/// Canonical dedupe key for a page URL (`urls.normalise(url).lower()`):
/// lowercase host, no trailing slash, tracking query keys dropped.
fn url_key(raw: &str) -> String {
    const STRIP: [&str; 13] = [
        "action", "from", "label", "tab", "sig", "ref", "search_item_id", "search_page_id", "utm_source",
        "utm_medium", "utm_campaign", "utm_content", "utm_term",
    ];
    let raw = raw.trim();
    let (before_frag, _) = raw.split_once('#').unwrap_or((raw, ""));
    let (head, query) = before_frag.split_once('?').unwrap_or((before_frag, ""));
    let (scheme, rest) = match head.split_once("://") {
        Some((s, r)) if !s.is_empty() => (s, r),
        _ => ("https", head.trim_start_matches("//")),
    };
    let (host, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let host = host.split(':').next().unwrap_or("");
    let path = path.trim_end_matches('/');
    // parse_qs keeps the first value of each key, drops blanks; sorted by key.
    let mut kept: Vec<(String, String)> = Vec::new();
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        if v.is_empty() || STRIP.contains(&k.to_lowercase().as_str()) || kept.iter().any(|(kk, _)| kk == k) {
            continue;
        }
        kept.push((k.to_string(), v.to_string()));
    }
    kept.sort();
    let q = kept.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&");
    let base = format!("{scheme}://{host}{path}");
    let out = if q.is_empty() { base } else { format!("{base}?{q}") };
    out.to_lowercase()
}

fn absorb(pool: &mut Vec<SearchHit>, seen: &mut HashSet<String>, hits: Vec<SearchHit>) {
    for hit in hits {
        if seen.insert(url_key(&hit.url)) {
            pool.push(hit);
        }
    }
}

/// Search Bandcamp for one tracklist row.
///
/// At most three requests, and the second and third are only spent when the
/// first has not answered. At ~0.67 req/s a 28-row crate is the difference
/// between 40 seconds and two minutes.
///
/// `query` replaces the text we would have guessed, for a row the user is
/// searching by hand. The *scoring* still compares against the row, so a
/// hand-found candidate carries a real, comparable confidence.
pub async fn match_row(
    searcher: &dyn Searcher,
    row: &TrackRow,
    limit: usize,
    query: Option<&str>,
) -> Result<MatchResult, HarvestError> {
    let guessed = format!("{} {}", row.artist, row.title).trim().to_string();
    let simplified = format!("{} {}", bare_name(&row.artist), base_title(&row.title)).trim().to_string();

    // In escalation order: the plain query (most specific), the simplified one
    // (a bracket added for disambiguation is often the only reason Bandcamp
    // returned nothing), the catalogue last (a track sold only as part of a
    // record).
    let attempts: Vec<(String, SearchKind)> = match query {
        Some(q) => vec![(q.to_string(), SearchKind::Track), (q.to_string(), SearchKind::All)],
        None => {
            let mut a = vec![(guessed.clone(), SearchKind::Track)];
            if name_key(&simplified) != name_key(&guessed) {
                a.push((simplified.clone(), SearchKind::Track));
            }
            a.push((simplified.clone(), SearchKind::All));
            a
        }
    };

    let mut pool: Vec<SearchHit> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut ranked: Vec<ScoredHit> = Vec::new();
    let mut searches = 0usize;

    for (text, kind) in attempts {
        absorb(&mut pool, &mut seen, searcher.search(&text, kind, limit).await?);
        searches += 1;
        ranked = rank(row, &pool);
        if ranked.first().is_some_and(|b| b.score >= LIKELY) {
            break;
        }
    }

    let best_index = pick_best(&ranked);
    ranked.truncate(CANDIDATES);
    // pick_best looked at the full ranking; the winner is always index 0.
    Ok(MatchResult {
        query: query.map(str::to_string).unwrap_or(guessed),
        candidates: ranked,
        best_index,
        searches,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    fn row(artist: &str, title: &str, label: &str) -> TrackRow {
        TrackRow::new(artist, title, label)
    }

    fn hit(name: &str, subtitle: &str) -> SearchHit {
        hit_k(name, subtitle, "track", "")
    }

    fn hit_k(name: &str, subtitle: &str, kind: &str, url: &str) -> SearchHit {
        let slug = name.to_lowercase().replace(' ', "-");
        SearchHit {
            kind: kind.into(),
            name: name.into(),
            subtitle: subtitle.into(),
            url: if url.is_empty() { format!("https://example.bandcamp.com/{kind}/{slug}") } else { url.into() },
            ..SearchHit::default()
        }
    }

    fn hit_u(name: &str, subtitle: &str, url: &str) -> SearchHit {
        hit_k(name, subtitle, "track", url)
    }

    #[test]
    fn mix_suffixes_are_stripped_without_emptying_the_title() {
        for (written, expected) in [
            ("2AM (Extended Mix)", "2AM"),
            ("In Chicago (original mix)", "In Chicago"),
            ("Chicago (DJ Pierre Chicago Club Mix)", "Chicago"),
            ("City Heat (G's Underground Dub)", "City Heat"),
            ("Each Step [Powerhouse Extended Mix]", "Each Step"),
            ("Some Track - Extended Mix", "Some Track"),
            ("MXM", "MXM"),
            ("(Reprise)", "(Reprise)"),
        ] {
            assert_eq!(base_title(written), expected, "{written}");
        }
    }

    #[test]
    fn a_shop_that_omits_the_mix_suffix_still_matches() {
        let s = score_hit(&row("Boogie Vice", "2AM (Extended Mix)", ""), &hit("2AM", "Boogie Vice"));
        assert!(s >= LIKELY, "{s}");
    }

    #[test]
    fn the_exact_title_outranks_the_stripped_one() {
        let ranked = rank(
            &row("Boogie Vice", "2AM (Extended Mix)", ""),
            &[hit("2AM", "Boogie Vice"), hit("2AM (Extended Mix)", "Boogie Vice")],
        );
        assert_eq!(ranked[0].hit.name, "2AM (Extended Mix)");
    }

    #[test]
    fn a_disambiguating_suffix_on_the_artist_is_tolerated() {
        let s = score_hit(&row("Pinto (NYC)", "Dangerous", ""), &hit("Dangerous", "Pinto"));
        assert!(s >= LIKELY, "{s}");
    }

    #[test]
    fn case_and_accents_do_not_matter() {
        assert_eq!(score_hit(&row("Roman Flügel", "Tippex", ""), &hit("TIPPEX", "roman flugel")), 1.0);
    }

    #[test]
    fn the_wrong_artist_loses_however_right_the_title_is() {
        let s = score_hit(&row("Boogie Vice", "2AM", ""), &hit("2AM", "Someone Else Entirely"));
        assert_eq!(tier_of(s), "weak");
        assert!(s < LIKELY);
    }

    #[test]
    fn a_label_run_page_names_the_label_as_the_band() {
        let on_label = hit("Freak Like U", "Clone Royal Oak");
        let s = score_hit(&row("Masarima", "Freak Like U", "Clone Royal Oak"), &on_label);
        assert!(s >= LIKELY, "{s}");
        assert!(s < score_hit(&row("Masarima", "Freak Like U", ""), &hit("Freak Like U", "Masarima")));
    }

    #[test]
    fn a_long_title_sharing_only_letters_is_not_a_match() {
        let s = score_hit(
            &row("K'alexi Shelby", "Chicago (DJ Pierre Chicago Club Mix)", ""),
            &hit("Inch By-K' Alexi Shelby(Klassik Chicago)128", "K' ALEXI SHELBY"),
        );
        assert_eq!(tier_of(s), "weak", "{s}");
    }

    #[test]
    fn extra_words_in_the_shop_s_title_do_not_count_against_a_hit() {
        let s = score_hit(
            &row("NY Stomp", "Never Forget House", ""),
            &hit("NY Stomp - Never Forget House", "Gerd"),
        );
        assert!(s >= LIKELY, "{s}");
    }

    #[test]
    fn a_one_word_title_is_judged_on_characters() {
        assert!(score_hit(&row("Boogie Vice", "2AM", ""), &hit("2 AM", "Boogie Vice")) >= LIKELY);
    }

    #[test]
    fn the_artist_named_inside_the_track_title_counts() {
        let s = score_hit(
            &row("Tuccillo", "Aguas Congas (Reboots Extrapicante Rework)", "Heattraxx"),
            &hit("Tuccillo, The Checkup - Aguas Congas (Reboots Extrapicante Rework)", "The Checkup"),
        );
        assert!(s >= LIKELY, "{s}");
    }

    #[test]
    fn the_label_breaks_a_tie_without_moving_the_score() {
        let r = row("Boogie Vice", "2AM", "DFTD");
        let on_label = hit_u("2AM", "Boogie Vice", "https://dftd.bandcamp.com/track/2am");
        let elsewhere = hit_u("2AM", "Boogie Vice", "https://other.bandcamp.com/track/2am");
        assert_eq!(score_hit(&r, &on_label), score_hit(&r, &elsewhere));
        assert_eq!(rank(&r, &[elsewhere, on_label.clone()])[0].hit.url, on_label.url);
    }

    #[test]
    fn a_two_letter_label_is_not_evidence() {
        let r = row("Slam", "Lifetimes", "XL");
        let ranked = rank(&r, &[hit_u("Lifetimes", "Slam", "https://xl.bandcamp.com/track/l")]);
        assert!(!ranked[0].label_match);
    }

    #[test]
    fn a_clean_match_is_pre_selected() {
        let ranked = rank(&row("Slam", "Lifetimes", ""), &[hit("Lifetimes", "Slam")]);
        assert_eq!(pick_best(&ranked), Some(0));
    }

    #[test]
    fn two_different_records_scoring_alike_go_to_the_human() {
        let ranked = rank(&row("Slam", "Lifetimes", ""), &[hit("Lifetime", "Slam"), hit("Lifetimez", "Slam")]);
        assert!(ranked[0].score - ranked[1].score < 0.05);
        assert_eq!(pick_best(&ranked), None);
    }

    #[test]
    fn one_record_sold_twice_is_not_a_close_contest() {
        let ranked = rank(
            &row("Boogie Vice", "2AM (Extended Mix)", ""),
            &[
                hit("2AM (Extended Mix)", "Boogie Vice"),
                hit_k("2AM", "Boogie Vice", "album", "https://x.bandcamp.com/album/2am"),
            ],
        );
        assert_eq!(pick_best(&ranked), Some(0));
    }

    #[test]
    fn nothing_convincing_is_pre_selected() {
        let ranked = rank(&row("Slam", "Lifetimes", ""), &[hit("Completely Other", "Nobody At All")]);
        assert_eq!(ranked[0].tier, "weak");
        assert_eq!(pick_best(&ranked), None);
    }

    #[test]
    fn no_candidates_at_all() {
        assert_eq!(pick_best(&[]), None);
        assert!(rank(&row("Slam", "Lifetimes", ""), &[]).is_empty());
    }

    #[test]
    fn bands_and_fans_are_not_candidates() {
        let ranked = rank(
            &row("Slam", "Lifetimes", ""),
            &[hit_k("Slam", "Slam", "artist", ""), hit("Lifetimes", "Slam")],
        );
        assert_eq!(ranked.iter().map(|c| c.hit.kind.as_str()).collect::<Vec<_>>(), ["track"]);
    }

    #[test]
    fn a_track_page_beats_an_album_page_at_the_same_score() {
        let ranked = rank(
            &row("Slam", "Lifetimes", ""),
            &[
                hit_k("Lifetimes", "Slam", "album", "https://x.bandcamp.com/album/l"),
                hit_k("Lifetimes", "Slam", "track", "https://x.bandcamp.com/track/l"),
            ],
        );
        assert_eq!(ranked[0].hit.kind, "track");
    }

    #[test]
    fn tiers() {
        for (score, expected) in [(1.0, "strong"), (0.9, "strong"), (0.75, "likely"), (0.4, "weak")] {
            assert_eq!(tier_of(score), expected);
        }
    }

    #[test]
    fn round4_matches_python() {
        assert_eq!(round4(0.123449), 0.1234);
        assert_eq!(round4(0.72345), 0.7235_f64.min(0.7235));
    }

    #[test]
    fn netloc_like_urlparse() {
        assert_eq!(netloc("https://dftd.bandcamp.com/track/x"), "dftd.bandcamp.com");
        assert_eq!(netloc("dftd.bandcamp.com/track/x"), "");
        assert_eq!(netloc("//a.b/c"), "a.b");
    }

    #[test]
    fn url_keys_ignore_tracking_noise() {
        assert_eq!(
            url_key("https://X.bandcamp.com/track/Y/?from=search&utm_source=a"),
            url_key("https://x.bandcamp.com/track/y")
        );
    }

    /// Stands in for the rate-limited client, recording what was asked.
    struct FakeSearches {
        answers: Mutex<Vec<Vec<SearchHit>>>,
        calls: Mutex<Vec<(String, &'static str)>>,
    }

    impl FakeSearches {
        fn new(answers: Vec<Vec<SearchHit>>) -> Self {
            Self { answers: Mutex::new(answers), calls: Mutex::new(Vec::new()) }
        }
        fn calls(&self) -> Vec<(String, &'static str)> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl Searcher for FakeSearches {
        async fn search(&self, query: &str, kind: SearchKind, _limit: usize) -> Result<Vec<SearchHit>, HarvestError> {
            self.calls.lock().unwrap().push((query.to_string(), kind.as_filter()));
            let mut a = self.answers.lock().unwrap();
            Ok(if a.is_empty() { Vec::new() } else { a.remove(0) })
        }
    }

    #[tokio::test]
    async fn a_clean_first_answer_costs_one_request() {
        let fake = FakeSearches::new(vec![vec![hit("Lifetimes", "Slam")]]);
        let result = match_row(&fake, &row("Slam", "Lifetimes", ""), DEFAULT_LIMIT, None).await.unwrap();
        assert_eq!(result.searches, 1);
        assert_eq!(fake.calls(), vec![("Slam Lifetimes".to_string(), "t")]);
        assert_eq!(result.best_index, Some(0));
    }

    #[tokio::test]
    async fn a_miss_retries_without_the_mix_suffix_then_widens() {
        let fake = FakeSearches::new(vec![vec![], vec![], vec![hit_k("2AM", "Boogie Vice", "album", "")]]);
        let result =
            match_row(&fake, &row("Boogie Vice", "2AM (Extended Mix)", ""), DEFAULT_LIMIT, None).await.unwrap();
        let calls = fake.calls();
        assert_eq!(calls.iter().map(|c| c.1).collect::<Vec<_>>(), ["t", "t", ""]);
        assert_eq!(calls[1].0, "Boogie Vice 2AM");
        assert_eq!(result.searches, 3);
        assert_eq!(result.candidates[0].hit.kind, "album");
    }

    #[tokio::test]
    async fn the_same_page_returned_by_two_searches_is_offered_once() {
        let same = hit_u("2AM", "Boogie Vice", "https://x.bandcamp.com/track/2am");
        let fake = FakeSearches::new(vec![vec![same.clone()], vec![same.clone()], vec![same]]);
        let result =
            match_row(&fake, &row("Boogie Vice", "2AM (Some Unknown Mix)", ""), DEFAULT_LIMIT, None).await.unwrap();
        assert_eq!(result.candidates.len(), 1);
    }

    #[tokio::test]
    async fn a_manual_query_replaces_the_guess_but_scoring_uses_the_row() {
        let fake = FakeSearches::new(vec![vec![hit("Lifetimes", "Slam")]]);
        let result =
            match_row(&fake, &row("Slam", "Lifetimes", ""), DEFAULT_LIMIT, Some("slam life")).await.unwrap();
        assert_eq!(result.query, "slam life");
        assert_eq!(fake.calls(), vec![("slam life".to_string(), "t")]);
        assert_eq!(result.best_index, Some(0));
    }

    #[tokio::test]
    async fn a_manual_query_escalates_to_the_catalogue() {
        let fake = FakeSearches::new(vec![]);
        let result = match_row(&fake, &row("Slam", "Lifetimes", ""), DEFAULT_LIMIT, Some("zzz")).await.unwrap();
        assert_eq!(result.searches, 2);
        assert_eq!(fake.calls(), vec![("zzz".to_string(), "t"), ("zzz".to_string(), "")]);
        assert_eq!(result.best_index, None);
    }
}
