//! State for the tracklist review table, kept pure so it can be tested.
//!
//! Ports `tracklist.ts`: [`RowState`], [`IDLE`]/[`RowState::idle`], [`initial_states`],
//! [`apply_match`], [`apply_error`], [`url_of`], [`selected_urls`], [`summarise`], [`run_pool`].
//! The Tracklists page owns a [`RowStates`] signal, folds each search result in with
//! [`apply_match`]/[`apply_error`], drives the search pass with [`run_pool`] (concurrency 2) and
//! queues [`selected_urls`] on Download.
//!
//! TS `TracklistRow` = [`TrackRowOut`], `TracklistMatch` = [`MatchOut`], `TracklistCandidate` =
//! [`crate::bandcamp::CandidateOut`] (`.hit.url`, `.hit.in_library`).

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::task::Poll;

use crate::bandcamp::{MatchOut, TrackRowOut};

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
    pub match_: Option<MatchOut>,
    pub error: Option<String>,
    /// Index into `match_.candidates`, or `None` for "download nothing for this row".
    pub chosen: Option<usize>,
    /// Deliberately excluded by the user, however good the match is.
    pub skipped: bool,
}

impl RowState {
    pub fn idle() -> Self {
        Self::default()
    }
}

/// `IDLE` of the TS (a value in Rust: use [`RowState::idle`]).
pub const IDLE: RowState = RowState {
    status: RowStatus::Idle,
    match_: None,
    error: None,
    chosen: None,
    skipped: false,
};

/// Insertion-ordered `Map<seq, RowState>` (iteration order matters for [`selected_urls`]).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RowStates {
    order: Vec<i64>,
    map: HashMap<i64, RowState>,
}

impl RowStates {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn insert(&mut self, seq: i64, state: RowState) {
        if self.map.insert(seq, state).is_none() {
            self.order.push(seq);
        }
    }
    pub fn get(&self, seq: i64) -> Option<&RowState> {
        self.map.get(&seq)
    }
    pub fn len(&self) -> usize {
        self.order.len()
    }
    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }
    /// States in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = (i64, &RowState)> {
        self.order.iter().filter_map(|k| self.map.get(k).map(|v| (*k, v)))
    }
}

impl FromIterator<(i64, RowState)> for RowStates {
    fn from_iter<I: IntoIterator<Item = (i64, RowState)>>(iter: I) -> Self {
        let mut s = Self::new();
        for (k, v) in iter {
            s.insert(k, v);
        }
        s
    }
}

pub fn initial_states(rows: &[TrackRowOut]) -> RowStates {
    rows.iter().map(|r| (r.seq, RowState::idle())).collect()
}

fn match_index_of(m: &MatchOut, url: &str) -> Option<usize> {
    m.candidates.iter().position(|c| c.hit.url == url)
}

fn best_index(m: &MatchOut) -> Option<usize> {
    m.best_index.and_then(|i| usize::try_from(i).ok())
}

/// Fold a finished search into a row, honouring a choice already made: an explicit choice
/// survives (found again by URL in the new candidates, else the new best); an untouched row takes
/// the server's `best_index`.
pub fn apply_match(prev: &RowState, m: MatchOut) -> RowState {
    let keep = if prev.status == RowStatus::Done && prev.chosen.is_some() {
        url_of(prev)
    } else {
        None
    };
    let chosen = match keep {
        Some(url) => match_index_of(&m, &url).or_else(|| best_index(&m)),
        None => best_index(&m),
    };
    RowState {
        status: RowStatus::Done,
        match_: Some(m),
        error: None,
        chosen,
        skipped: prev.skipped,
    }
}

pub fn apply_error(prev: &RowState, error: impl Into<String>) -> RowState {
    RowState {
        status: RowStatus::Error,
        error: Some(error.into()),
        chosen: None,
        ..prev.clone()
    }
}

/// The Bandcamp page this row will download, if any.
pub fn url_of(state: &RowState) -> Option<String> {
    if state.skipped {
        return None;
    }
    let chosen = state.chosen?;
    state
        .match_
        .as_ref()?
        .candidates
        .get(chosen)
        .map(|c| c.hit.url.clone())
}

/// What the Download button queues: every chosen URL, each one once (first-seen order).
pub fn selected_urls(states: &RowStates) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for (_, state) in states.iter() {
        if let Some(url) = url_of(state) {
            if seen.insert(url.clone()) {
                out.push(url);
            }
        }
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Summary {
    pub total: usize,
    pub searched: usize,
    pub selected: usize,
    pub unmatched: usize,
    pub skipped: usize,
    pub failed: usize,
    /// Selected rows the library already holds (queued anyway under `force`).
    pub already_have: usize,
}

pub fn summarise(rows: &[TrackRowOut], states: &RowStates) -> Summary {
    let mut s = Summary { total: rows.len(), ..Default::default() };
    for row in rows {
        let state = states.get(row.seq).unwrap_or(&IDLE);
        if state.status == RowStatus::Done {
            s.searched += 1;
        }
        if state.status == RowStatus::Error {
            s.failed += 1;
        }
        if state.skipped {
            s.skipped += 1;
            continue;
        }
        if let Some(chosen) = state.chosen {
            s.selected += 1;
            if state
                .match_
                .as_ref()
                .and_then(|m| m.candidates.get(chosen))
                .is_some_and(|c| c.hit.in_library)
            {
                s.already_have += 1;
            }
        } else if state.status == RowStatus::Done {
            s.unmatched += 1;
        }
    }
    s
}

/// Run `task` over `items`, at most `concurrency` in flight (single-threaded cooperative: the
/// workers are polled from one future, so it needs no spawner and works on wasm32).
///
/// Two at a time, not twenty: Bandcamp is behind a shared token bucket. Like `Promise.all`, the
/// first `Err` is returned at once (callers record failures per item and return `Ok`).
pub async fn run_pool<T, E, F, Fut>(items: Vec<T>, concurrency: usize, task: F) -> Result<(), E>
where
    F: Fn(T) -> Fut,
    Fut: Future<Output = Result<(), E>>,
{
    let queue = std::cell::RefCell::new(items.into_iter());
    let n = concurrency.min(queue.borrow().len());
    let task = &task;
    let queue = &queue;
    let mut workers: Vec<Pin<Box<dyn Future<Output = Result<(), E>> + '_>>> = (0..n)
        .map(|_| {
            Box::pin(async move {
                loop {
                    let next = queue.borrow_mut().next();
                    match next {
                        Some(item) => task(item).await?,
                        None => return Ok(()),
                    }
                }
            }) as Pin<Box<dyn Future<Output = Result<(), E>> + '_>>
        })
        .collect();
    std::future::poll_fn(move |cx| {
        let mut i = 0;
        while i < workers.len() {
            match workers[i].as_mut().poll(cx) {
                Poll::Ready(Ok(())) => {
                    drop(workers.swap_remove(i));
                }
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => i += 1,
            }
        }
        if workers.is_empty() { Poll::Ready(Ok(())) } else { Poll::Pending }
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bandcamp::logic::testutil::{YieldNow, block_on};
    use crate::bandcamp::{CandidateOut, SearchHitOut};
    use std::cell::Cell;

    fn row(seq: i64) -> TrackRowOut {
        TrackRowOut {
            seq,
            artist: "Slam".into(),
            title: "Lifetimes".into(),
            source_file: "set.csv".into(),
            ..Default::default()
        }
    }

    fn candidate(url: &str, in_library: bool) -> CandidateOut {
        CandidateOut {
            hit: SearchHitOut {
                kind: "track".into(),
                name: "Lifetimes".into(),
                url: url.into(),
                subtitle: "Slam".into(),
                in_library,
                ..Default::default()
            },
            score: 1.0,
            tier: "strong".into(),
            label_match: false,
        }
    }

    fn matched(urls: &[&str], best: Option<i64>) -> MatchOut {
        MatchOut {
            query: "Slam Lifetimes".into(),
            candidates: urls.iter().map(|u| candidate(u, false)).collect(),
            best_index: best,
            searches: 1,
        }
    }

    #[test]
    fn apply_match_pre_selects_what_the_server_chose() {
        let state = apply_match(&IDLE, matched(&["https://a/track/x", "https://b/track/y"], Some(0)));
        assert_eq!(state.status, RowStatus::Done);
        assert_eq!(url_of(&state).as_deref(), Some("https://a/track/x"));
    }

    #[test]
    fn apply_match_leaves_an_ambiguous_row_unselected() {
        assert_eq!(url_of(&apply_match(&IDLE, matched(&["https://a/track/x"], None))), None);
    }

    #[test]
    fn apply_match_keeps_a_correction_the_user_already_made() {
        let mut corrected = apply_match(&IDLE, matched(&["https://a/x", "https://b/y"], Some(0)));
        corrected.chosen = Some(1);
        let again = apply_match(&corrected, matched(&["https://a/x", "https://b/y"], Some(0)));
        assert_eq!(url_of(&again).as_deref(), Some("https://b/y"));
    }

    #[test]
    fn apply_match_falls_back_to_the_new_best_when_the_correction_is_gone() {
        let mut corrected = apply_match(&IDLE, matched(&["https://a/x", "https://b/y"], Some(0)));
        corrected.chosen = Some(1);
        let again = apply_match(&corrected, matched(&["https://c/z"], Some(0)));
        assert_eq!(url_of(&again).as_deref(), Some("https://c/z"));
    }

    #[test]
    fn apply_match_clears_a_stale_error() {
        let failed = apply_error(&IDLE, "Bandcamp is rate-limiting us");
        assert_eq!(failed.status, RowStatus::Error);
        assert_eq!(apply_match(&failed, matched(&["https://a/x"], Some(0))).error, None);
    }

    #[test]
    fn url_of_returns_nothing_for_a_skipped_row_however_good_the_match() {
        let mut state = apply_match(&IDLE, matched(&["https://a/x"], Some(0)));
        state.skipped = true;
        assert_eq!(url_of(&state), None);
    }

    #[test]
    fn selected_urls_queues_each_page_once_even_when_two_rows_resolve_to_it() {
        let states: RowStates = [
            (1, apply_match(&IDLE, matched(&["https://a/track/x"], Some(0)))),
            (2, apply_match(&IDLE, matched(&["https://a/track/x"], Some(0)))),
            (3, apply_match(&IDLE, matched(&["https://b/track/y"], Some(0)))),
        ]
        .into_iter()
        .collect();
        assert_eq!(selected_urls(&states), ["https://a/track/x", "https://b/track/y"]);
    }

    #[test]
    fn selected_urls_is_empty_before_anything_has_been_searched() {
        assert!(selected_urls(&initial_states(&[row(1), row(2)])).is_empty());
    }

    #[test]
    fn summarise_counts_what_the_footer_has_to_say() {
        let rows: Vec<_> = (1..=5).map(row).collect();
        let mut skipped = apply_match(&IDLE, matched(&["https://c/z"], Some(0)));
        skipped.skipped = true;
        let states: RowStates = [
            (1, apply_match(&IDLE, matched(&["https://a/x"], Some(0)))),
            (2, apply_match(&IDLE, matched(&[], None))),
            (3, skipped),
            (4, apply_error(&IDLE, "nope")),
            (5, RowState::idle()),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            summarise(&rows, &states),
            Summary { total: 5, searched: 3, selected: 1, unmatched: 1, skipped: 1, failed: 1, already_have: 0 }
        );
    }

    #[test]
    fn summarise_flags_selected_rows_the_library_already_holds() {
        let owned = MatchOut {
            candidates: vec![candidate("https://a/x", true)],
            ..matched(&["https://a/x"], Some(0))
        };
        let states: RowStates = [(1, apply_match(&IDLE, owned))].into_iter().collect();
        assert_eq!(summarise(&[row(1)], &states).already_have, 1);
    }

    #[test]
    fn run_pool_never_runs_more_than_the_given_number_at_once() {
        let live = Cell::new(0usize);
        let peak = Cell::new(0usize);
        let seen = std::cell::RefCell::new(Vec::new());
        let r: Result<(), ()> = block_on(run_pool(vec![1, 2, 3, 4, 5, 6, 7], 2, |n| {
            let (live, peak, seen) = (&live, &peak, &seen);
            async move {
                live.set(live.get() + 1);
                peak.set(peak.get().max(live.get()));
                YieldNow(false).await;
                seen.borrow_mut().push(n);
                live.set(live.get() - 1);
                Ok(())
            }
        }));
        assert!(r.is_ok());
        assert_eq!(peak.get(), 2);
        assert_eq!(seen.borrow().len(), 7);
    }

    #[test]
    fn run_pool_returns_immediately_for_an_empty_list() {
        let called = Cell::new(false);
        let r: Result<(), ()> = block_on(run_pool(Vec::<i32>::new(), 4, |_| {
            called.set(true);
            async { Ok(()) }
        }));
        assert!(r.is_ok());
        assert!(!called.get());
    }

    #[test]
    fn run_pool_surfaces_a_failing_task_so_callers_must_catch_per_item() {
        let r = block_on(run_pool(vec![1, 2, 3], 2, |n| async move {
            if n == 1 { Err("aborted") } else { Ok(()) }
        }));
        assert_eq!(r, Err("aborted"));
    }
}
