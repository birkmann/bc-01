//! The logic part of `similarBody.ts`: what a "more like this track" request is
//! seeded from. A Bandcamp stream has no row on the server, so its (negative)
//! id is withheld and what the client knows (tempo, key, energy, tags) rides in
//! the seed override instead; a library track sends its id *and* the override,
//! which fills in whatever the row lacks.

use bc_types::player::QueueItem;
use bc_types::suggest::{SeedOverride, Signals, SimilarRequest};

/// Whether `item` is a library track the server has a row for.
pub fn is_library_track(item: Option<&QueueItem>) -> bool {
    item.map(|t| t.is_library()).unwrap_or(false)
}

#[derive(Debug, Clone, Default)]
pub struct SimilarOpts {
    pub exclude: Vec<i64>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    pub shuffle_seed: Option<i64>,
}

pub fn similar_request(item: &QueueItem, signals: Signals, opts: SimilarOpts) -> SimilarRequest {
    SimilarRequest {
        seed_track_id: is_library_track(Some(item)).then_some(item.track_id),
        seed: Some(SeedOverride { bpm: item.bpm, camelot: item.camelot.clone(), energy: item.energy, tags: item.tags.clone() }),
        signals,
        exclude_track_ids: opts.exclude,
        limit: opts.limit.unwrap_or(30),
        offset: opts.offset.unwrap_or(0),
        shuffle_seed: opts.shuffle_seed.unwrap_or(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bc_types::player::ItemOrigin;

    fn track(over: impl FnOnce(&mut QueueItem)) -> QueueItem {
        let mut t = QueueItem {
            track_id: 42,
            title: "Nocturne".into(),
            artist: Some("Vril".into()),
            duration_ms: Some(400_000),
            tags: vec!["techno".into(), "dub techno".into()],
            bpm: Some(128.0),
            camelot: Some("8A".into()),
            energy: Some(0.6),
            ..Default::default()
        };
        over(&mut t);
        t
    }
    fn stream() -> QueueItem {
        track(|t| {
            t.track_id = -7;
            t.origin = ItemOrigin::Bandcamp;
        })
    }

    #[test]
    fn is_library_track_rejects_streams_and_null() {
        assert!(is_library_track(Some(&track(|_| {}))));
        assert!(!is_library_track(Some(&stream())));
        assert!(!is_library_track(None));
    }

    #[test]
    fn sends_the_id_for_a_library_track() {
        assert_eq!(similar_request(&track(|_| {}), Signals::default(), SimilarOpts::default()).seed_track_id, Some(42));
    }

    #[test]
    fn withholds_the_id_for_a_stream_but_sends_what_it_knows() {
        let body = similar_request(&stream(), Signals::default(), SimilarOpts::default());
        assert_eq!(body.seed_track_id, None);
        assert_eq!(
            body.seed,
            Some(SeedOverride { bpm: Some(128.0), camelot: Some("8A".into()), energy: Some(0.6), tags: vec!["techno".into(), "dub techno".into()] })
        );
    }

    #[test]
    fn always_sends_the_override() {
        let body = similar_request(&track(|_| {}), Signals::default(), SimilarOpts::default());
        assert_eq!(body.seed.unwrap().tags, vec!["techno", "dub techno"]);
    }

    #[test]
    fn passes_the_signals_through_verbatim() {
        let s = Signals { label: false, key: false, ..Signals::default() };
        assert_eq!(similar_request(&track(|_| {}), s, SimilarOpts::default()).signals, s);
    }

    #[test]
    fn carries_paging_reroll_and_excludes_and_defaults_to_the_first_page() {
        let b = similar_request(
            &track(|_| {}),
            Signals::default(),
            SimilarOpts { exclude: vec![1, 2], limit: Some(60), offset: Some(30), shuffle_seed: Some(3) },
        );
        assert_eq!((b.exclude_track_ids, b.limit, b.offset, b.shuffle_seed), (vec![1, 2], 60, 30, 3));
        let d = similar_request(&track(|_| {}), Signals::default(), SimilarOpts::default());
        assert_eq!((d.offset, d.shuffle_seed, d.limit), (0, 0, 30));
    }
}
