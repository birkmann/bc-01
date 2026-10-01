//! The request behind "what could come next", built once so the crate in the panel and the
//! server-side auto-fill ask the same question (port of `plan/suggestBody.ts`).
use bc_types::player::{ItemOrigin, Pool, PlanState, QueueItem, Wish};
use bc_types::suggest::{Direction, SeedOverride, SuggestRequest, Wishes};

const RECENT_HISTORY: usize = 30;
const EXCLUDE_MAX: usize = 100;

/// A library track (not a Bandcamp stream with a synthetic negative id).
pub fn is_library(t: &QueueItem) -> bool {
    t.track_id > 0 && t.origin == ItemOrigin::Library
}

/// `(playlist_id, loved)` fencing a pool.
pub fn pool_fields(pool: Option<&Pool>) -> (Option<i64>, Option<bool>) {
    match pool {
        None | Some(Pool::Library) => (None, None),
        Some(Pool::Loved) => (None, Some(true)),
        Some(Pool::Playlist { id, .. }) => (Some(*id), None),
    }
}

pub fn suggest_request(seed: &QueueItem, exclude: Vec<i64>, plan: &PlanState, limit: i64) -> SuggestRequest {
    let ids = |f: fn(&Wish) -> Option<i64>| plan.wishes.iter().filter_map(f).collect::<Vec<_>>();
    let (playlist_id, loved) = pool_fields(plan.pools.first());
    SuggestRequest {
        seed_track_id: is_library(seed).then_some(seed.track_id),
        seed: Some(SeedOverride { bpm: seed.bpm, camelot: seed.camelot.clone(), energy: seed.energy, tags: seed.tags.clone() }),
        direction: Direction {
            tempo: plan.tempo,
            energy: plan.energy,
            tag_mode: plan.tag_mode,
            tags: if plan.tag_mode == bc_types::suggest::TagMode::Switch { plan.target_tags.clone() } else { vec![] },
            allow_tags: plan.tag_rules.allow.clone(),
            deny_tags: plan.tag_rules.deny.clone(),
            harmonic: plan.harmonic,
            ..Default::default()
        },
        wishes: Wishes {
            wish_track_ids: ids(|w| if let Wish::Track { id, .. } = w { Some(*id) } else { None }),
            wish_artist_ids: ids(|w| if let Wish::Artist { id, .. } = w { Some(*id) } else { None }),
            wish_label_ids: ids(|w| if let Wish::Label { id, .. } = w { Some(*id) } else { None }),
            wish_tags: plan.wishes.iter().filter_map(|w| if let Wish::Tag { name } = w { Some(name.clone()) } else { None }).collect(),
        },
        exclude_track_ids: exclude,
        limit,
        playlist_id,
        loved,
    }
}

/// What must not be suggested again: the playing track, everything still to come, and the last
/// half hour of what was played.
pub fn exclude_ids(current: Option<&QueueItem>, upcoming: &[&QueueItem], history: &[usize], queue: &[QueueItem]) -> Vec<i64> {
    let mut seen = std::collections::HashSet::new();
    let mut out = vec![];
    let mut add = |t: &QueueItem| {
        if is_library(t) && seen.insert(t.track_id) {
            out.push(t.track_id);
        }
    };
    if let Some(c) = current {
        add(c);
    }
    for t in upcoming {
        add(t);
    }
    let from = history.len().saturating_sub(RECENT_HISTORY);
    for h in &history[from..] {
        if let Some(t) = queue.get(*h) {
            add(t);
        }
    }
    out.truncate(EXCLUDE_MAX);
    out
}

/// Short label of a wish for chips.
pub fn wish_label(w: &Wish) -> String {
    match w {
        Wish::Track { title, artist, .. } => if artist.is_empty() { title.clone() } else { format!("{artist} – {title}") },
        Wish::Artist { name, .. } | Wish::Label { name, .. } => name.clone(),
        Wish::Tag { name } => format!("#{name}"),
    }
}

pub fn pool_label(p: &Pool) -> String {
    match p {
        Pool::Library => "Library".into(),
        Pool::Loved => "Loved".into(),
        Pool::Playlist { name, .. } => name.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bc_types::suggest::{EnergyDir, Harmonic, TagMode, Tempo};

    fn item(id: i64) -> QueueItem {
        QueueItem { track_id: id, title: format!("t{id}"), bpm: Some(128.0), camelot: Some("8A".into()), energy: Some(0.6), tags: vec!["techno".into()], ..Default::default() }
    }

    #[test]
    fn library_seed_sends_id_and_override() {
        let r = suggest_request(&item(42), vec![1], &PlanState::default(), 20);
        assert_eq!(r.seed_track_id, Some(42));
        let s = r.seed.unwrap();
        assert_eq!(s.camelot.as_deref(), Some("8A"));
        assert_eq!(s.tags, vec!["techno".to_string()]);
        assert_eq!(r.limit, 20);
        assert_eq!(r.exclude_track_ids, vec![1]);
    }

    #[test]
    fn stream_seed_withholds_the_id() {
        let mut s = item(-7);
        s.origin = ItemOrigin::Bandcamp;
        assert_eq!(suggest_request(&s, vec![], &PlanState::default(), 20).seed_track_id, None);
        assert!(!is_library(&s));
    }

    #[test]
    fn direction_and_rules_are_carried() {
        let mut plan = PlanState::default();
        plan.tempo = Tempo::Raise;
        plan.energy = EnergyDir::Up;
        plan.harmonic = Harmonic::Loose;
        plan.tag_mode = TagMode::Switch;
        plan.target_tags = vec!["house".into()];
        plan.tag_rules.allow = vec!["a".into()];
        plan.tag_rules.deny = vec!["b".into()];
        let r = suggest_request(&item(1), vec![], &plan, 20);
        assert_eq!(r.direction.tempo, Tempo::Raise);
        assert_eq!(r.direction.tags, vec!["house".to_string()]);
        assert_eq!(r.direction.allow_tags, vec!["a".to_string()]);
        assert_eq!(r.direction.deny_tags, vec!["b".to_string()]);
        // tags only ride along for a switch
        plan.tag_mode = TagMode::Drift;
        assert!(suggest_request(&item(1), vec![], &plan, 20).direction.tags.is_empty());
    }

    #[test]
    fn wishes_split_by_kind_and_pool_fences() {
        let mut plan = PlanState::default();
        plan.wishes = vec![
            Wish::Track { id: 5, title: "x".into(), artist: "y".into() },
            Wish::Artist { id: 6, name: "a".into() },
            Wish::Label { id: 7, name: "l".into() },
            Wish::Tag { name: "dub".into() },
        ];
        plan.pools = vec![Pool::Playlist { id: 9, name: "p".into() }, Pool::Loved];
        let r = suggest_request(&item(1), vec![], &plan, 20);
        assert_eq!(r.wishes.wish_track_ids, vec![5]);
        assert_eq!(r.wishes.wish_artist_ids, vec![6]);
        assert_eq!(r.wishes.wish_label_ids, vec![7]);
        assert_eq!(r.wishes.wish_tags, vec!["dub".to_string()]);
        assert_eq!((r.playlist_id, r.loved), (Some(9), None));
        assert_eq!(pool_fields(Some(&Pool::Loved)), (None, Some(true)));
        assert_eq!(pool_fields(Some(&Pool::Library)), (None, None));
        assert_eq!(pool_fields(None), (None, None));
    }

    #[test]
    fn exclusion_covers_current_upcoming_and_recent_history_deduped() {
        let queue: Vec<QueueItem> = (1..=5).map(item).collect();
        let cur = &queue[1];
        let up: Vec<&QueueItem> = vec![&queue[2], &queue[3]];
        let ex = exclude_ids(Some(cur), &up, &[0, 1], &queue);
        assert_eq!(ex, vec![2, 3, 4, 1]);
        let mut bc = item(-1);
        bc.origin = ItemOrigin::Bandcamp;
        assert!(exclude_ids(Some(&bc), &[], &[], &[]).is_empty());
    }

    #[test]
    fn history_window_and_cap() {
        let queue: Vec<QueueItem> = (1..=200).map(item).collect();
        let hist: Vec<usize> = (0..200).collect();
        let ex = exclude_ids(None, &[], &hist, &queue);
        assert_eq!(ex.len(), 30);
        assert_eq!(ex[0], 171);
    }
}
