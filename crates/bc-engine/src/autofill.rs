//! Never run out of tracks while mixing: port of `web/frontend/src/player/autoFill.ts`.
//!
//! With DJ mix on and auto-fill enabled, whenever fewer than [`FILL_TO`] tracks
//! are queued ahead the session asks the same "what could come next" the
//! planner's crate shows -- after the last planned track, in the chosen
//! direction, towards the wishes -- and appends the top answers. If the rules
//! leave nothing (a strict harmonic filter on an odd key) they are loosened
//! step by step -- loose keys, no harmonic filter, then drift -- and as a last
//! resort a random draw keeps the music going. The tag rules are never on the
//! ladder: they are what the DJ ruled out.

use crate::ports::{NextUpRequest, Ports};
use bc_types::player::{Harmonic, PlanState, Pool, QueueItem, TagMode, Wish};
use bc_types::suggest::{Direction, SeedOverride, Wishes};

/// Keep the whole planning horizon full.
pub const FILL_TO: usize = bc_types::player::HORIZON;
/// A few at a time, each batch seeded from the last of the previous one.
pub const BATCH: usize = 5;
/// After a fill that found nothing, wait this long before asking again.
pub const RETRY_MS: u64 = 60_000;
const RECENT_HISTORY: usize = 30;
const EXCLUDE_MAX: usize = 100;

/// A snapshot of what one fill run needs (sent to the background thread).
#[derive(Debug, Clone)]
pub struct FillJob {
    pub run: u64,
    pub seed: QueueItem,
    pub exclude: Vec<i64>,
    pub want: usize,
    pub plan: PlanState,
}

#[derive(Debug, Clone)]
pub struct FillResult {
    pub run: u64,
    pub seed_id: i64,
    pub found: Vec<QueueItem>,
}

fn ids_of_wishes(wishes: &[Wish], kind: &str) -> Vec<i64> {
    wishes
        .iter()
        .filter_map(|w| match (kind, w) {
            ("track", Wish::Track { id, .. }) | ("artist", Wish::Artist { id, .. }) | ("label", Wish::Label { id, .. }) => {
                Some(*id)
            }
            _ => None,
        })
        .collect()
}

/// The request behind "what could come next" (`suggestRequest` of `plan/suggestBody.ts`).
pub fn suggest_request(seed: &QueueItem, exclude: &[i64], plan: &PlanState, harmonic: Harmonic, tag_mode: TagMode, limit: usize) -> NextUpRequest {
    let (playlist_id, loved) = match plan.pools.first() {
        None | Some(Pool::Library) => (None, None),
        Some(Pool::Loved) => (None, Some(true)),
        Some(Pool::Playlist { id, .. }) => (Some(*id), None),
    };
    NextUpRequest {
        seed_track_id: seed.is_library().then_some(seed.track_id),
        seed: Some(SeedOverride { bpm: seed.bpm, camelot: seed.camelot.clone(), energy: seed.energy, tags: seed.tags.clone() }),
        direction: Direction {
            tempo: plan.tempo,
            energy: plan.energy,
            tag_mode,
            tags: if tag_mode == TagMode::Switch { plan.target_tags.clone() } else { vec![] },
            allow_tags: plan.tag_rules.allow.clone(),
            deny_tags: plan.tag_rules.deny.clone(),
            harmonic,
            ..Default::default()
        },
        wishes: Wishes {
            wish_track_ids: ids_of_wishes(&plan.wishes, "track"),
            wish_artist_ids: ids_of_wishes(&plan.wishes, "artist"),
            wish_label_ids: ids_of_wishes(&plan.wishes, "label"),
            wish_tags: plan.wishes.iter().filter_map(|w| if let Wish::Tag { name } = w { Some(name.clone()) } else { None }).collect(),
        },
        exclude_track_ids: exclude.to_vec(),
        limit: limit as i64,
        playlist_id,
        loved,
    }
}

/// What must not be suggested again: the playing track, everything still to
/// come, and the last half hour of what was played.
pub fn exclude_ids(current: Option<&QueueItem>, upcoming: &[QueueItem], history: &[usize], queue: &[QueueItem]) -> Vec<i64> {
    let mut ids: Vec<i64> = Vec::new();
    let mut push = |t: &QueueItem| {
        if t.is_library() && !ids.contains(&t.track_id) {
            ids.push(t.track_id);
        }
    };
    if let Some(c) = current {
        push(c);
    }
    for t in upcoming {
        push(t);
    }
    let start = history.len().saturating_sub(RECENT_HISTORY);
    for &h in &history[start..] {
        if let Some(t) = queue.get(h) {
            push(t);
        }
    }
    ids.truncate(EXCLUDE_MAX);
    ids
}

/// Ask, loosening the rules until something comes back -- within the pool.
/// An empty answer means the pool is spent (for these rules).
pub fn fetch_fill(ports: &Ports, job: &FillJob) -> Vec<QueueItem> {
    let plan = &job.plan;
    let attempts: [(Harmonic, TagMode); 4] = [
        (plan.harmonic, plan.tag_mode),
        (Harmonic::Loose, plan.tag_mode),
        (Harmonic::Off, plan.tag_mode),
        (Harmonic::Off, TagMode::Drift),
    ];
    for (h, m) in attempts {
        let req = suggest_request(&job.seed, &job.exclude, plan, h, m, job.want);
        match ports.recommend.next_up(&req) {
            Ok(items) if !items.is_empty() => return items,
            Ok(_) => {}
            Err(e) => tracing::debug!("next-up rung failed: {e}"),
        }
    }
    // Nothing fits at all: anything not just played beats silence -- within the
    // rules still. A random draw sees ten times the ask so a rule that keeps a
    // fraction of the library still gets a fill.
    let over = if plan.tag_rules.is_empty() { 1 } else { 10 };
    match ports.library.random_tracks(plan.pools.first(), job.want * over + job.exclude.len()) {
        Ok(items) => items
            .into_iter()
            .filter(|t| !job.exclude.contains(&t.track_id) && !plan.tag_rules.breaks(&t.tags))
            .take(job.want)
            .collect(),
        Err(_) => vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::*;
    use bc_types::player::*;
    use std::sync::{Arc, Mutex};

    fn item(id: i64, tags: &[&str]) -> QueueItem {
        QueueItem { track_id: id, title: format!("t{id}"), tags: tags.iter().map(|s| s.to_string()).collect(), ..Default::default() }
    }

    /// A recommender that answers only at a given rung, recording what it was asked.
    struct Ladder {
        answer_at: usize,
        asked: Mutex<Vec<NextUpRequest>>,
    }
    impl RecommendPort for Ladder {
        fn next_up(&self, req: &NextUpRequest) -> PortResult<Vec<QueueItem>> {
            let mut a = self.asked.lock().unwrap();
            a.push(req.clone());
            if a.len() > self.answer_at { Ok(vec![item(99, &[])]) } else { Ok(vec![]) }
        }
    }
    struct Lib(Vec<QueueItem>);
    impl LibraryPort for Lib {
        fn track_item(&self, _: i64) -> PortResult<Option<QueueItem>> { Ok(None) }
        fn file_path(&self, _: i64) -> PortResult<Option<std::path::PathBuf>> { Ok(None) }
        fn facts(&self, _: i64) -> PortResult<Option<TrackFacts>> { Ok(None) }
        fn record_play(&self, _: i64, _: i64, _: bool, _: bool) -> PortResult<()> { Ok(()) }
        fn playlist_items(&self, _: i64) -> PortResult<Vec<QueueItem>> { Ok(vec![]) }
        fn random_tracks(&self, _: Option<&Pool>, _: usize) -> PortResult<Vec<QueueItem>> { Ok(self.0.clone()) }
        fn next_release(&self, _: i64, _: &serde_json::Value) -> PortResult<Option<i64>> { Ok(None) }
        fn release_tracks(&self, _: i64) -> PortResult<Vec<QueueItem>> { Ok(vec![]) }
        fn next_label(&self, _: i64, _: &serde_json::Value) -> PortResult<Option<i64>> { Ok(None) }
        fn random_label(&self, _: &serde_json::Value, _: i64) -> PortResult<Option<i64>> { Ok(None) }
        fn label_tracks(&self, _: i64, _: LabelMode) -> PortResult<Vec<QueueItem>> { Ok(vec![]) }
        fn shuffle_labels(&self, _: &serde_json::Value, _: usize) -> PortResult<Vec<QueueItem>> { Ok(vec![]) }
    }
    struct NoBc;
    impl BandcampPort for NoBc {
        fn resolve_stream(&self, _: &QueueItem) -> PortResult<String> { Err(PortError::NotFound) }
        fn fan_next(&self, _: &FanCursor, _: Option<i64>, _: usize) -> PortResult<FanPage> { Ok(FanPage::default()) }
        fn release_tracks(&self, _: &str) -> PortResult<Vec<QueueItem>> { Ok(vec![]) }
    }
    struct NoState;
    impl StatePort for NoState {
        fn get(&self, _: &str) -> Option<String> { None }
        fn set(&self, _: &str, _: &str) {}
    }
    fn ports(rec: Arc<Ladder>, random: Vec<QueueItem>) -> Ports {
        Ports { library: Arc::new(Lib(random)), bandcamp: Arc::new(NoBc), recommend: rec, state: Arc::new(NoState), base_url: String::new() }
    }
    fn job(plan: PlanState) -> FillJob {
        FillJob { run: 1, seed: item(1, &[]), exclude: vec![1, 2], want: 3, plan }
    }

    #[test]
    fn ladder_loosens_in_order_and_stops_at_first_answer() {
        let rec = Arc::new(Ladder { answer_at: 2, asked: Mutex::new(vec![]) });
        let found = fetch_fill(&ports(rec.clone(), vec![]), &job(PlanState::default()));
        assert_eq!(found.len(), 1);
        let asked = rec.asked.lock().unwrap();
        assert_eq!(asked.len(), 3);
        assert_eq!(asked[0].direction.harmonic, Harmonic::Strict);
        assert_eq!(asked[1].direction.harmonic, Harmonic::Loose);
        assert_eq!(asked[2].direction.harmonic, Harmonic::Off);
    }

    #[test]
    fn last_rung_is_drift_and_then_a_random_draw_within_the_rules() {
        let rec = Arc::new(Ladder { answer_at: 99, asked: Mutex::new(vec![]) });
        let plan = PlanState {
            tag_mode: TagMode::Switch,
            tag_rules: TagRules { allow: vec![], deny: vec!["pop".into()] },
            ..PlanState::default()
        };
        let random = vec![item(1, &[]), item(5, &["pop"]), item(6, &["house"]), item(7, &[])];
        let found = fetch_fill(&ports(rec.clone(), random), &job(plan));
        let asked = rec.asked.lock().unwrap();
        assert_eq!(asked.len(), 4);
        assert_eq!(asked[3].direction.tag_mode, TagMode::Drift);
        // tag rules are never loosened
        assert!(asked.iter().all(|r| r.direction.deny_tags == vec!["pop".to_string()]));
        // excluded id 1 and the forbidden pop track are filtered
        let ids: Vec<i64> = found.iter().map(|t| t.track_id).collect();
        assert_eq!(ids, vec![6, 7]);
    }

    #[test]
    fn pool_fields_fence_the_request() {
        let mut plan = PlanState { pools: vec![Pool::Playlist { id: 4, name: "x".into() }, Pool::Loved], ..PlanState::default() };
        let r = suggest_request(&item(1, &[]), &[], &plan, Harmonic::Strict, TagMode::Drift, 5);
        assert_eq!(r.playlist_id, Some(4));
        assert_eq!(r.loved, None);
        plan.pools = vec![Pool::Loved];
        let r = suggest_request(&item(1, &[]), &[], &plan, Harmonic::Strict, TagMode::Drift, 5);
        assert_eq!((r.playlist_id, r.loved), (None, Some(true)));
    }

    #[test]
    fn exclude_covers_current_upcoming_and_recent_history() {
        let queue: Vec<QueueItem> = (1..=5).map(|i| item(i, &[])).collect();
        let ex = exclude_ids(Some(&queue[2]), &queue[3..], &[0, 1, 2], &queue);
        assert_eq!(ex, vec![3, 4, 5, 1, 2]);
    }
}
