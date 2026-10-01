//! State for the tracklist review table, kept pure so it can be tested: a port of the legacy
//! `lib/tracklist.ts` (+ its vitest cases) and the `Gate`-style bounded pool.
//!
//! The table is the whole point of the feature: a matcher that guesses is only safe if the
//! guesses are visible and correctable before anything downloads. Everything here is about
//! *what is selected*; the searching lives in the page and the ranking in the backend.
use std::cell::RefCell;
use std::collections::{HashSet, VecDeque};
use std::future::Future;

use bc_types::bandcamp::MatchOut;

/// Confidence tiers (server-side thresholds, mirrored for display fallbacks).
pub const STRONG: f64 = 0.88;
pub const LIKELY: f64 = 0.70;

/// Two at a time: Bandcamp sits behind one shared token bucket at well under a request a
/// second, so more parallelism buys nothing and risks a rate limit on the whole app.
pub const CONCURRENCY: usize = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RowStatus {
    #[default]
    Idle,
    Searching,
    Done,
    Error,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct RowState {
    pub status: RowStatus,
    pub matched: Option<MatchOut>,
    pub error: Option<String>,
    /// Index into `matched.candidates`, or `None` for "download nothing for this row".
    pub chosen: Option<usize>,
    /// Deliberately excluded by the user, however good the match is.
    pub skipped: bool,
}

pub const IDLE: RowState = RowState { status: RowStatus::Idle, matched: None, error: None, chosen: None, skipped: false };

fn index_of(m: &MatchOut, url: &str) -> Option<usize> {
    m.candidates.iter().position(|c| c.hit.url == url)
}

/// The Bandcamp page this row will download, if any.
pub fn url_of(s: &RowState) -> Option<String> {
    if s.skipped {
        return None;
    }
    let i = s.chosen?;
    s.matched.as_ref()?.candidates.get(i).map(|c| c.hit.url.clone())
}

/// Fold a finished search into a row, honouring a choice already made: re-running the pass
/// over a row the user has corrected must not undo the correction, so an explicit choice
/// survives and only an untouched row takes the server's `best_index`.
pub fn apply_match(prev: &RowState, m: MatchOut) -> RowState {
    let keep = if prev.status == RowStatus::Done && prev.chosen.is_some() { url_of(&RowState { skipped: false, ..prev.clone() }) } else { None };
    let best = m.best_index.and_then(|b| usize::try_from(b).ok());
    let chosen = match keep {
        Some(url) => index_of(&m, &url).or(best),
        None => best,
    };
    RowState { status: RowStatus::Done, matched: Some(m), error: None, chosen, skipped: prev.skipped }
}

pub fn apply_error(prev: &RowState, error: String) -> RowState {
    RowState { status: RowStatus::Error, error: Some(error), chosen: None, ..prev.clone() }
}

/// A hand-typed query replaces what the matcher found outright and takes the top result even
/// when the scorer would not have committed: the user has just said what they were looking for.
pub fn apply_manual(prev: &RowState, m: MatchOut) -> RowState {
    let chosen = if m.candidates.is_empty() { None } else { Some(m.best_index.and_then(|b| usize::try_from(b).ok()).unwrap_or(0)) };
    RowState { status: RowStatus::Done, matched: Some(m), error: None, chosen, skipped: prev.skipped }
}

/// What the Download button queues: every chosen URL, each one once (two rows can resolve to
/// the same page: an original and its edit, or a correction pointing at a neighbour's match).
pub fn selected_urls<'a>(states: impl IntoIterator<Item = &'a RowState>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = vec![];
    for s in states {
        if let Some(u) = url_of(s) {
            if seen.insert(u.clone()) {
                out.push(u);
            }
        }
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Summary {
    pub total: usize,
    pub searched: usize,
    pub selected: usize,
    pub unmatched: usize,
    pub skipped: usize,
    pub failed: usize,
    /// Selected rows the library already holds: queued anyway under `force`.
    pub already_have: usize,
}

pub fn summarise<'a>(states: impl IntoIterator<Item = &'a RowState>) -> Summary {
    let mut s = Summary::default();
    for st in states {
        s.total += 1;
        match st.status {
            RowStatus::Done => s.searched += 1,
            RowStatus::Error => s.failed += 1,
            _ => {}
        }
        if st.skipped {
            s.skipped += 1;
            continue;
        }
        if let Some(i) = st.chosen {
            s.selected += 1;
            if st.matched.as_ref().and_then(|m| m.candidates.get(i)).is_some_and(|c| c.hit.in_library) {
                s.already_have += 1;
            }
        } else if st.status == RowStatus::Done {
            s.unmatched += 1;
        }
    }
    s
}

/// Tier of a score when the server did not say.
pub fn tier_of(score: f64) -> &'static str {
    if score >= STRONG {
        "strong"
    } else if score >= LIKELY {
        "likely"
    } else {
        "weak"
    }
}

/// `Name \u{2014} Artist [album] \u{b7} 93%` for the candidate dropdown.
pub fn candidate_label(name: &str, subtitle: &str, kind: &str, score: f64) -> String {
    let pct = (score * 100.0).round() as i64;
    let kind = if kind == "album" { " [album]" } else { "" };
    format!("{name} \u{2014} {}{kind} \u{b7} {pct}%", if subtitle.is_empty() { "unknown" } else { subtitle })
}

/// Run `task` over `items`, at most `concurrency` in flight. A task must not panic; the page
/// records a failed row and returns normally, so one rate-limited request never abandons the rest.
pub async fn run_pool<T, F, Fut>(items: Vec<T>, concurrency: usize, task: F)
where
    F: Fn(T) -> Fut,
    Fut: Future<Output = ()>,
{
    let n = concurrency.min(items.len());
    let queue = RefCell::new(items.into_iter().collect::<VecDeque<_>>());
    let workers = (0..n).map(|_| async {
        loop {
            let next = queue.borrow_mut().pop_front();
            let Some(item) = next else { return };
            task(item).await;
        }
    });
    futures::future::join_all(workers).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use bc_types::bandcamp::{CandidateOut, SearchHitOut};
    use std::cell::Cell;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    fn candidate(url: &str, in_library: bool) -> CandidateOut {
        CandidateOut {
            hit: SearchHitOut { kind: "track".into(), name: "Lifetimes".into(), url: url.into(), subtitle: "Slam".into(), in_library, ..Default::default() },
            score: 1.0,
            tier: "strong".into(),
            label_match: false,
        }
    }
    fn matched(urls: &[&str], best: Option<i64>) -> MatchOut {
        MatchOut { query: "Slam Lifetimes".into(), candidates: urls.iter().map(|u| candidate(u, false)).collect(), best_index: best, searches: 1 }
    }
    fn done(urls: &[&str]) -> RowState {
        apply_match(&IDLE, matched(urls, Some(0)))
    }

    #[test]
    fn apply_match_preselects_what_the_server_chose() {
        let s = apply_match(&IDLE, matched(&["https://a/track/x", "https://b/track/y"], Some(0)));
        assert_eq!(s.status, RowStatus::Done);
        assert_eq!(url_of(&s).as_deref(), Some("https://a/track/x"));
    }

    #[test]
    fn apply_match_leaves_an_ambiguous_row_unselected() {
        assert_eq!(url_of(&apply_match(&IDLE, matched(&["https://a/track/x"], None))), None);
    }

    #[test]
    fn apply_match_keeps_a_correction_the_user_already_made() {
        let corrected = RowState { chosen: Some(1), ..done(&["https://a/x", "https://b/y"]) };
        let again = apply_match(&corrected, matched(&["https://a/x", "https://b/y"], Some(0)));
        assert_eq!(url_of(&again).as_deref(), Some("https://b/y"));
    }

    #[test]
    fn apply_match_falls_back_to_the_new_best_when_the_correction_is_gone() {
        let corrected = RowState { chosen: Some(1), ..done(&["https://a/x", "https://b/y"]) };
        let again = apply_match(&corrected, matched(&["https://c/z"], Some(0)));
        assert_eq!(url_of(&again).as_deref(), Some("https://c/z"));
    }

    #[test]
    fn apply_match_clears_a_stale_error() {
        let failed = apply_error(&IDLE, "Bandcamp is rate-limiting us".into());
        assert_eq!(failed.status, RowStatus::Error);
        assert_eq!(apply_match(&failed, matched(&["https://a/x"], Some(0))).error, None);
    }

    #[test]
    fn url_of_is_nothing_for_a_skipped_row_however_good_the_match() {
        let s = RowState { skipped: true, ..done(&["https://a/x"]) };
        assert_eq!(url_of(&s), None);
    }

    #[test]
    fn manual_search_takes_the_top_result_even_when_ambiguous() {
        let s = apply_manual(&IDLE, matched(&["https://a/x", "https://b/y"], None));
        assert_eq!(url_of(&s).as_deref(), Some("https://a/x"));
        let none = apply_manual(&IDLE, matched(&[], None));
        assert_eq!(none.chosen, None);
        assert_eq!(none.status, RowStatus::Done);
    }

    #[test]
    fn selected_urls_queue_each_page_once() {
        let states = [done(&["https://a/track/x"]), done(&["https://a/track/x"]), done(&["https://b/track/y"])];
        assert_eq!(selected_urls(&states), ["https://a/track/x", "https://b/track/y"]);
    }

    #[test]
    fn selected_urls_are_empty_before_anything_was_searched() {
        assert!(selected_urls(&[IDLE, IDLE]).is_empty());
    }

    #[test]
    fn summarise_counts_what_the_footer_has_to_say() {
        let states = [
            done(&["https://a/x"]),
            apply_match(&IDLE, matched(&[], None)),
            RowState { skipped: true, ..done(&["https://c/z"]) },
            apply_error(&IDLE, "nope".into()),
            IDLE,
        ];
        assert_eq!(summarise(&states), Summary { total: 5, searched: 3, selected: 1, unmatched: 1, skipped: 1, failed: 1, already_have: 0 });
    }

    #[test]
    fn summarise_flags_selected_rows_the_library_already_holds() {
        let owned = MatchOut { candidates: vec![candidate("https://a/x", true)], ..matched(&["https://a/x"], Some(0)) };
        assert_eq!(summarise(&[apply_match(&IDLE, owned)]).already_have, 1);
    }

    #[test]
    fn tiers_follow_the_documented_thresholds() {
        assert_eq!(tier_of(0.88), "strong");
        assert_eq!(tier_of(0.879), "likely");
        assert_eq!(tier_of(0.70), "likely");
        assert_eq!(tier_of(0.69), "weak");
    }

    #[test]
    fn candidate_labels_name_album_hits() {
        assert_eq!(candidate_label("Lifetimes", "Slam", "track", 0.931), "Lifetimes \u{2014} Slam \u{b7} 93%");
        assert_eq!(candidate_label("Lifetimes", "", "album", 0.7), "Lifetimes \u{2014} unknown [album] \u{b7} 70%");
    }

    /// A future that is pending once, so tasks interleave like real network waits.
    struct YieldOnce(bool);
    impl Future for YieldOnce {
        type Output = ();
        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            if self.0 {
                Poll::Ready(())
            } else {
                self.0 = true;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }
    }

    #[test]
    fn run_pool_never_runs_more_than_the_given_number_at_once() {
        let (live, peak, seen) = (Cell::new(0usize), Cell::new(0usize), RefCell::new(vec![]));
        futures::executor::block_on(run_pool((1..=7).collect(), 2, |n| {
            let (live, peak, seen) = (&live, &peak, &seen);
            async move {
                live.set(live.get() + 1);
                peak.set(peak.get().max(live.get()));
                YieldOnce(false).await;
                seen.borrow_mut().push(n);
                live.set(live.get() - 1);
            }
        }));
        assert_eq!(peak.get(), 2);
        assert_eq!(seen.borrow().len(), 7);
    }

    #[test]
    fn run_pool_returns_immediately_for_an_empty_list() {
        let calls = Cell::new(0);
        futures::executor::block_on(run_pool(Vec::<i32>::new(), 4, |_| {
            calls.set(calls.get() + 1);
            async {}
        }));
        assert_eq!(calls.get(), 0);
    }
}
