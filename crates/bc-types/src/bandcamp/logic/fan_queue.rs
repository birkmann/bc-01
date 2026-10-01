//! Playing a wishlist through: what the player asks for when its queue runs out.
//!
//! Ports `fanQueue.ts`. A wishlist record costs one rate-limited fetch (~1.5 s) to become
//! playable tracks (or a free read of its files once in the library), so the continuation is
//! built one step at a time with a server-side cursor: `seq` plays one whole album per step,
//! `shuffle` takes [`FAN_BATCH`] records per step with ONE random track each.
//!
//! Pure: the I/O is injected through [`FanDeps`] (the UI implements it over its HTTP client:
//! `next` = `GET /fans/{id}/next`, `local` = `GET /tracks?release_id=..&sort=album`, `remote` =
//! explore release tracks, playable only). The track type is generic (`Deps::Track`), so the
//! player's own track type is used.
//!
//! | TS | Rust |
//! | --- | --- |
//! | `buildFanBatch` | [`build_fan_batch`] |
//! | `tracksForItem` | [`tracks_for_item`] |
//! | `pickOne` | [`pick_one`] |
//! | `warmFanBatch` / `takeFanBatch` (module-level promise cache) | [`FanWarmCache`] state machine (`should_warm`/`start_warm`/`finish_warm`/`take`), plus the async [`warm_fan_batch`] / [`take_fan_batch`] drivers over a `RefCell<FanWarmCache>` |
//! | `newSeed` | [`new_seed`] (takes the `[0,1)` random number) |
//! | `resetFanQueue` | [`FanWarmCache::reset`] |
//!
//! The player calls `warm_fan_batch` when the last queued track begins and `take_fan_batch` when
//! the queue drains.

#![allow(async_fn_in_trait)]

use std::cell::RefCell;

use crate::bandcamp::FanNextItem;

/// Shuffle: records fetched per step, one track each, so Up Next is never empty.
pub const FAN_BATCH: usize = 3;
/// Records with nothing playable stepped over before giving up.
pub const MAX_EMPTY_HOPS: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FanOrder {
    Seq,
    Shuffle,
}

impl FanOrder {
    pub fn as_str(self) -> &'static str {
        match self {
            FanOrder::Seq => "seq",
            FanOrder::Shuffle => "shuffle",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FanTab {
    Wishlist,
    Collection,
}

impl FanTab {
    pub fn as_str(self) -> &'static str {
        match self {
            FanTab::Wishlist => "wishlist",
            FanTab::Collection => "collection",
        }
    }
}

/// The shape the player keeps for a wishlist queue.
#[derive(Debug, Clone, PartialEq)]
pub struct FanCursor {
    pub fan_id: i64,
    /// The last item played; `None` before the first.
    pub item_id: Option<i64>,
    pub order: FanOrder,
    pub seed: u32,
    /// Inbox states the listing was filtered to, if any.
    pub states: Option<Vec<String>>,
    /// Their wishlist, their collection, or both as one list (`None`).
    pub tab: Option<FanTab>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FanBatch<T> {
    pub tracks: Vec<T>,
    /// The item the last track came from: the cursor for the step after.
    pub last_item_id: i64,
}

/// Query of `GET /fans/{id}/next`.
#[derive(Debug, Clone, PartialEq)]
pub struct FanNextParams {
    pub after: Option<i64>,
    pub order: FanOrder,
    pub seed: u32,
    pub state: Option<Vec<String>>,
    pub tab: Option<FanTab>,
    pub limit: usize,
}

/// Injected I/O. `local`/`remote` errors are swallowed (an empty answer); only `next` errors
/// propagate.
pub trait FanDeps {
    type Track: Clone;
    type Error;
    async fn next(
        &self,
        fan_id: i64,
        p: FanNextParams,
    ) -> Result<(Vec<FanNextItem>, bool), Self::Error>;
    /// Tracks of a release already in the library, album order.
    async fn local(&self, release_id: i64) -> Result<Vec<Self::Track>, Self::Error>;
    /// Playable tracks of a Bandcamp page, streamed.
    async fn remote(&self, url: &str) -> Result<Vec<Self::Track>, Self::Error>;
    /// A random number in `[0, 1)`.
    fn rng(&self) -> f64;
}

/// What one wishlist item plays as: its library files when it has them, the stream otherwise.
/// A failure on either path is an empty answer, not an error.
pub async fn tracks_for_item<D: FanDeps>(item: &FanNextItem, deps: &D) -> Vec<D::Track> {
    if let Some(rid) = item.release_id {
        if let Ok(local) = deps.local(rid).await {
            if !local.is_empty() {
                return local;
            }
        }
        // else fall through to the stream: the row may be stale
    }
    deps.remote(&item.url).await.unwrap_or_default()
}

pub fn pick_one<T: Clone>(items: &[T], rng: f64) -> Vec<T> {
    if items.is_empty() {
        return Vec::new();
    }
    let index = ((rng * items.len() as f64).floor().max(0.0) as usize).min(items.len() - 1);
    vec![items[index].clone()]
}

/// The next step of a wishlist queue, or `None` when the list is played out. Records with nothing
/// playable are stepped over up to [`MAX_EMPTY_HOPS`].
pub async fn build_fan_batch<D: FanDeps>(
    cursor: &FanCursor,
    deps: &D,
) -> Result<Option<FanBatch<D::Track>>, D::Error> {
    let mut after = cursor.item_id;
    let mut tracks: Vec<D::Track> = Vec::new();
    let mut last_item_id: Option<i64> = None;
    let shuffle = cursor.order == FanOrder::Shuffle;
    let want = if shuffle { FAN_BATCH } else { 1 };
    let mut hops = 0usize;

    while tracks.len() < want && hops < MAX_EMPTY_HOPS {
        let (items, exhausted) = deps
            .next(
                cursor.fan_id,
                FanNextParams {
                    after,
                    order: cursor.order,
                    seed: cursor.seed,
                    state: cursor.states.clone(),
                    tab: cursor.tab,
                    limit: want,
                },
            )
            .await?;
        if items.is_empty() {
            break;
        }
        for item in &items {
            after = Some(item.item_id);
            let found = tracks_for_item(item, deps).await;
            let take = if shuffle { pick_one(&found, deps.rng()) } else { found };
            if take.is_empty() {
                hops += 1;
                if hops >= MAX_EMPTY_HOPS {
                    break;
                }
                continue;
            }
            tracks.extend(take);
            last_item_id = Some(item.item_id);
            if tracks.len() >= want {
                break;
            }
        }
        if exhausted {
            break;
        }
    }

    match last_item_id {
        Some(last_item_id) if !tracks.is_empty() => Ok(Some(FanBatch { tracks, last_item_id })),
        _ => Ok(None),
    }
}

fn key_of(c: &FanCursor) -> String {
    format!(
        "{}:{}:{}:{}:{}:{}",
        c.fan_id,
        c.item_id.map(|i| i.to_string()).unwrap_or_default(),
        c.order.as_str(),
        c.seed,
        c.tab.map_or("all", FanTab::as_str),
        c.states.as_deref().unwrap_or(&[]).join(",")
    )
}

/// What [`FanWarmCache::take`] found.
#[derive(Debug, Clone, PartialEq)]
pub enum Taken<T> {
    /// A finished warm batch for this cursor (`None` = the list is played out / the warm failed).
    Ready(Option<FanBatch<T>>),
    /// A warm for this cursor is still running: wait for it, then `take` again.
    Pending,
    /// Nothing warmed for this cursor: build now.
    Miss,
}

/// One-entry cache of the early-built next step (the TS module-level `warmKey`/`warmPromise`).
/// Warming for a new cursor drops the old entry.
#[derive(Debug)]
pub struct FanWarmCache<T> {
    key: Option<String>,
    batch: Option<Option<FanBatch<T>>>,
}

impl<T> Default for FanWarmCache<T> {
    fn default() -> Self {
        Self { key: None, batch: None }
    }
}

impl<T> FanWarmCache<T> {
    pub fn new() -> Self {
        Self::default()
    }

    /// For tests / teardown: forget any warmed step.
    pub fn reset(&mut self) {
        self.key = None;
        self.batch = None;
    }

    /// `false` when this cursor is already warm (or warming).
    pub fn should_warm(&self, cursor: &FanCursor) -> bool {
        self.key.as_deref() != Some(key_of(cursor).as_str())
    }

    /// Mark `cursor` as warming (drops any other entry). Returns the key to hand to `finish_warm`.
    pub fn start_warm(&mut self, cursor: &FanCursor) -> String {
        let key = key_of(cursor);
        self.key = Some(key.clone());
        self.batch = None;
        key
    }

    /// Store the result of a warm; ignored if the cache moved on to another cursor meanwhile.
    pub fn finish_warm(&mut self, key: &str, result: Option<FanBatch<T>>) {
        if self.key.as_deref() == Some(key) {
            self.batch = Some(result);
        }
    }

    /// Consume the warmed step if it is for this cursor. Always clears the cache (one entry,
    /// served once) unless the warm is still pending.
    pub fn take(&mut self, cursor: &FanCursor) -> Taken<T> {
        if self.key.as_deref() == Some(key_of(cursor).as_str()) {
            match self.batch.take() {
                Some(b) => {
                    self.key = None;
                    Taken::Ready(b)
                }
                None => Taken::Pending,
            }
        } else {
            self.key = None;
            self.batch = None;
            Taken::Miss
        }
    }
}

/// Start building the next step early (when the last queued track begins). A failed build is a
/// cached `None`. Do not hold a `RefCell` borrow across the await: this only borrows briefly.
pub async fn warm_fan_batch<D: FanDeps>(
    cache: &RefCell<FanWarmCache<D::Track>>,
    cursor: &FanCursor,
    deps: &D,
) {
    let key = {
        let mut c = cache.borrow_mut();
        if !c.should_warm(cursor) {
            return;
        }
        c.start_warm(cursor)
    };
    let result = build_fan_batch(cursor, deps).await.ok().flatten();
    cache.borrow_mut().finish_warm(&key, result);
}

/// The next step: the warmed one if it is for this cursor and finished, built now otherwise
/// (a still-pending warm is not awaited here; `Pending` falls back to a fresh build).
pub async fn take_fan_batch<D: FanDeps>(
    cache: &RefCell<FanWarmCache<D::Track>>,
    cursor: &FanCursor,
    deps: &D,
) -> Result<Option<FanBatch<D::Track>>, D::Error> {
    let taken = cache.borrow_mut().take(cursor);
    match taken {
        Taken::Ready(b) => Ok(b),
        Taken::Pending | Taken::Miss => build_fan_batch(cursor, deps).await,
    }
}

/// A fresh shuffle order: the seed the server's fixed order is keyed by. `rng` in `[0, 1)`.
pub fn new_seed(rng: f64) -> u32 {
    (rng * 2f64.powi(31)).floor() as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bandcamp::logic::testutil::block_on;
    use std::collections::HashMap;

    #[derive(Debug, Clone, PartialEq)]
    struct Track {
        title: String,
    }
    fn track(title: &str) -> Track {
        Track { title: title.into() }
    }
    fn titles(b: &Option<FanBatch<Track>>) -> Vec<String> {
        b.as_ref().unwrap().tracks.iter().map(|t| t.title.clone()).collect()
    }

    fn item(id: i64, release_id: Option<i64>) -> FanNextItem {
        FanNextItem {
            item_id: id,
            url: format!("https://a.bandcamp.com/album/r{id}"),
            url_kind: "album".into(),
            title: format!("R{id}"),
            artist_name: "A".into(),
            state: "new".into(),
            release_id,
            position: Some(id),
            ..Default::default()
        }
    }

    struct Fake {
        n: usize,
        tracks_of: Box<dyn Fn(i64) -> Vec<Track>>,
        local: HashMap<i64, Vec<Track>>,
        rng: f64,
        calls: RefCell<Vec<String>>,
    }
    fn fake(n: usize, tracks_of: impl Fn(i64) -> Vec<Track> + 'static) -> Fake {
        Fake { n, tracks_of: Box::new(tracks_of), local: HashMap::new(), rng: 0.0, calls: RefCell::new(vec![]) }
    }
    impl Fake {
        fn next_calls(&self) -> usize {
            self.calls.borrow().iter().filter(|c| c.starts_with("next:")).count()
        }
    }

    impl FanDeps for Fake {
        type Track = Track;
        type Error = String;
        async fn next(&self, _fan: i64, p: FanNextParams) -> Result<(Vec<FanNextItem>, bool), String> {
            self.calls.borrow_mut().push(format!(
                "next:{}:{}",
                p.after.map_or("-".to_string(), |a| a.to_string()),
                p.limit
            ));
            let ids: Vec<i64> = (1..=self.n as i64).collect();
            let start = match p.after {
                Some(a) if a != 0 => ids.iter().position(|&i| i == a).map_or(0, |i| i + 1),
                _ => 0,
            };
            let slice: Vec<i64> = ids.iter().skip(start).take(p.limit).copied().collect();
            let items = slice
                .into_iter()
                .map(|id| item(id, self.local.contains_key(&id).then_some(id)))
                .collect();
            Ok((items, start + p.limit >= self.n))
        }
        async fn local(&self, release_id: i64) -> Result<Vec<Track>, String> {
            self.calls.borrow_mut().push(format!("local:{release_id}"));
            Ok(self.local.get(&release_id).cloned().unwrap_or_default())
        }
        async fn remote(&self, url: &str) -> Result<Vec<Track>, String> {
            self.calls.borrow_mut().push(format!("remote:{url}"));
            let id: i64 = url.rsplit("/r").next().unwrap().parse().unwrap();
            Ok((self.tracks_of)(id))
        }
        fn rng(&self) -> f64 {
            self.rng
        }
    }

    fn cur(item_id: Option<i64>, order: FanOrder, seed: u32) -> FanCursor {
        FanCursor { fan_id: 1, item_id, order, seed, states: None, tab: None }
    }

    // ---- in list order ----

    #[test]
    fn plays_the_next_record_whole_and_remembers_where_it_got_to() {
        let deps = fake(3, |i| vec![track(&format!("{i}a")), track(&format!("{i}b"))]);
        let batch = block_on(build_fan_batch(&cur(None, FanOrder::Seq, 0), &deps)).unwrap();
        assert_eq!(titles(&batch), ["1a", "1b"]);
        assert_eq!(batch.unwrap().last_item_id, 1);

        let next = block_on(build_fan_batch(&cur(Some(1), FanOrder::Seq, 0), &deps)).unwrap();
        assert_eq!(titles(&next), ["2a", "2b"]);
        assert_eq!(next.unwrap().last_item_id, 2);
    }

    #[test]
    fn steps_over_records_with_nothing_playable() {
        let deps = fake(3, |i| if i == 3 { vec![track("last")] } else { vec![] });
        let batch = block_on(build_fan_batch(&cur(None, FanOrder::Seq, 0), &deps)).unwrap();
        assert_eq!(titles(&batch), ["last"]);
        assert_eq!(batch.unwrap().last_item_id, 3);
    }

    #[test]
    fn is_none_once_the_list_is_played_out() {
        let deps = fake(2, |i| vec![track(&i.to_string())]);
        assert_eq!(block_on(build_fan_batch(&cur(Some(2), FanOrder::Seq, 0), &deps)).unwrap(), None);
    }

    #[test]
    fn gives_up_after_the_hop_cap_rather_than_crawling_a_dead_list() {
        let deps = fake(50, |_| vec![]);
        assert_eq!(block_on(build_fan_batch(&cur(None, FanOrder::Seq, 0), &deps)).unwrap(), None);
        assert!(deps.next_calls() <= MAX_EMPTY_HOPS);
    }

    #[test]
    fn prefers_the_library_files_of_a_record_already_downloaded() {
        let mut deps = fake(1, |_| vec![track("stream")]);
        deps.local.insert(1, vec![track("file")]);
        let batch = block_on(build_fan_batch(&cur(None, FanOrder::Seq, 0), &deps)).unwrap();
        assert_eq!(titles(&batch), ["file"]);
        assert!(!deps.calls.borrow().contains(&"remote:https://a.bandcamp.com/album/r1".to_string()));
    }

    // ---- shuffled ----

    #[test]
    fn takes_one_random_track_from_each_of_a_few_records() {
        // rng = 0.99 -> the last track of each record.
        let mut deps = fake(10, |i| vec![track(&format!("{i}a")), track(&format!("{i}b"))]);
        deps.rng = 0.99;
        let batch = block_on(build_fan_batch(&cur(None, FanOrder::Shuffle, 7), &deps)).unwrap();
        assert_eq!(titles(&batch), ["1b", "2b", "3b"]);
        assert_eq!(batch.as_ref().unwrap().tracks.len(), FAN_BATCH);
        assert_eq!(batch.unwrap().last_item_id, 3);
    }

    #[test]
    fn fills_the_batch_past_records_with_nothing_to_play() {
        let deps = fake(6, |i| if i % 2 == 0 { vec![track(&i.to_string())] } else { vec![] });
        let batch = block_on(build_fan_batch(&cur(None, FanOrder::Shuffle, 1), &deps)).unwrap();
        assert_eq!(titles(&batch), ["2", "4", "6"]);
    }

    #[test]
    fn returns_a_short_last_batch_at_the_end_of_the_list_rather_than_nothing() {
        let deps = fake(4, |i| vec![track(&i.to_string())]);
        let batch = block_on(build_fan_batch(&cur(Some(3), FanOrder::Shuffle, 1), &deps)).unwrap();
        assert_eq!(titles(&batch), ["4"]);
    }

    // ---- warming ----

    #[test]
    fn take_reuses_a_batch_warmed_for_the_same_cursor_and_only_once() {
        let deps = fake(3, |i| vec![track(&i.to_string())]);
        let cache = RefCell::new(FanWarmCache::new());
        let c = cur(None, FanOrder::Seq, 0);
        block_on(warm_fan_batch(&cache, &c, &deps));
        block_on(warm_fan_batch(&cache, &c, &deps));
        let first = block_on(take_fan_batch(&cache, &c, &deps)).unwrap();
        assert_eq!(first.unwrap().last_item_id, 1);
        assert_eq!(deps.next_calls(), 1);

        block_on(take_fan_batch(&cache, &c, &deps)).unwrap();
        assert_eq!(deps.next_calls(), 2);
    }

    #[test]
    fn a_warmed_batch_for_another_cursor_is_not_served() {
        let deps = fake(3, |i| vec![track(&i.to_string())]);
        let cache = RefCell::new(FanWarmCache::new());
        block_on(warm_fan_batch(&cache, &cur(None, FanOrder::Seq, 0), &deps));
        let other = block_on(take_fan_batch(&cache, &cur(Some(1), FanOrder::Seq, 0), &deps)).unwrap();
        assert_eq!(other.unwrap().last_item_id, 2);
    }

    #[test]
    fn reset_forgets_the_warmed_step_and_new_seed_is_31_bits() {
        let deps = fake(3, |i| vec![track(&i.to_string())]);
        let cache = RefCell::new(FanWarmCache::new());
        let c = cur(None, FanOrder::Seq, 0);
        block_on(warm_fan_batch(&cache, &c, &deps));
        cache.borrow_mut().reset();
        assert_eq!(cache.borrow_mut().take(&c), Taken::Miss);
        assert_eq!(new_seed(0.0), 0);
        assert_eq!(new_seed(0.999999999), 2_147_483_645);
    }

    #[test]
    fn pick_one_clamps_the_index() {
        assert_eq!(pick_one(&[1, 2, 3], 0.99), [3]);
        assert_eq!(pick_one(&[1, 2, 3], 1.0), [3]);
        assert_eq!(pick_one::<i32>(&[], 0.5), Vec::<i32>::new());
    }
}
