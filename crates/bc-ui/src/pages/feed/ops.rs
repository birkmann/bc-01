//! Inbox operations shared by Feed and Fans: batch ignore, batch queue, play via a server-built source.
use bc_types::bandcamp::{HarvestItemOut, QueueRequest, QueueResult};
use bc_types::player::{ExploreCard, PlayerCommand, QueueSource};
use futures::StreamExt;

use crate::api;
use crate::player::PlayerCtx;

/// Concurrent requests for the one-by-one fallback of a batch ignore.
const IGNORE_CONCURRENCY: usize = 6;

/// Toggle `ignored` on each item. Returns `(id, new_state)` for those that succeeded and the
/// first error message. The server toggles per item (`POST /harvest/items/{id}/ignore`); a
/// batch route would collapse this into one request (see Requests in the report).
pub async fn toggle_ignore(ids: Vec<i64>) -> (Vec<(i64, String)>, Option<String>) {
    // One request (server toggles in one transaction); fall back to per-item on an older server.
    match api::post::<_, Vec<HarvestItemOut>>("/harvest/items/ignore", &serde_json::json!({ "ids": ids })).await {
        Ok(items) => return (items.into_iter().map(|i| (i.id, i.state)).collect(), None),
        Err(e) if e.status != 404 && e.status != 405 => return (vec![], Some(e.message())),
        Err(_) => {}
    }
    let results: Vec<(i64, Result<HarvestItemOut, api::ApiErr>)> = futures::stream::iter(ids)
        .map(|id| async move { (id, api::send::<(), HarvestItemOut>("POST", &format!("/harvest/items/{id}/ignore"), &()).await) })
        .buffer_unordered(IGNORE_CONCURRENCY)
        .collect()
        .await;
    let mut ok = vec![];
    let mut err = None;
    for (id, r) in results {
        match r {
            Ok(item) => ok.push((id, item.state)),
            Err(e) => err = err.or(Some(e.message())),
        }
    }
    (ok, err)
}

pub async fn queue(req: &QueueRequest) -> Result<QueueResult, api::ApiErr> {
    api::post("/harvest/items/queue", req).await
}

/// Play Bandcamp cards: the server sweeps them (library files when the release is owned).
pub fn play_cards(player: PlayerCtx, cards: Vec<ExploreCard>, shuffle: bool) {
    if cards.is_empty() {
        return;
    }
    player.cmd(PlayerCommand::StartSource { source: QueueSource::Explore { cards, shuffle, next: 0 }, shuffle });
}

pub fn card_of(i: &HarvestItemOut) -> ExploreCard {
    ExploreCard { url: i.url.clone(), library_release_id: i.release_id }
}

/// `Queued 12 for download. Skipped 3 already in library.` style summary of a queue answer.
pub fn queue_notice(r: &QueueResult, to: &str) -> String {
    let mut s = format!("Queued {} {to}.", r.queued);
    if r.skipped_in_library > 0 {
        s.push_str(&format!(" Skipped {} already in library.", r.skipped_in_library));
    }
    if r.adopted > 0 {
        s.push_str(&format!(" Moved {} into your library.", r.adopted));
    }
    s
}

/// Relative age of an RFC 3339 timestamp ("5m ago").
pub fn ago_of(iso: &str) -> Option<String> {
    let ms = js_sys::Date::parse(iso);
    if ms.is_nan() {
        return None;
    }
    let secs = ((crate::util::unix_ms() - ms) / 1000.0).round() as i64;
    Some(crate::logic::format::format_ago(secs.max(0)))
}
