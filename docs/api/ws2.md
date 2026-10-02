# WS2 API: Bandcamp, downloads, jobs

Paths are **without** the `/api` prefix (`bc-server` nests the routers under `/api`).
DTOs live in `bc_types::jobs` and `bc_types::bandcamp` (wasm-safe). Timestamps are RFC 3339 UTC strings.
Errors are RFC 9457 `problem+json` (`bc_types::Problem`); `IdentityExpired` -> 401, busy -> 503 + `Retry-After`.
Everything long returns **`202 Accepted` + `{"job_id"}`** (`bc_types::Accepted`) and runs as a job; progress arrives over WS.

Services (see COORDINATION "Service shape"):
```rust
bc_jobs::JobsService::new(db: Db, bus: Arc<EventBus>)   // .start().await  (crash recovery + 30 s lease reaper), .router(), .store()
bc_bandcamp::BandcampService::new(db, bus, cfg, jobs: JobsService) // .start().await (download worker, sweepers), .router()
```
Start order: `jobs.start().await` **before** any worker (own `start`s call `jobs.recover()`, which is idempotent) so that
crash recovery runs before anything claims.

## 1. Job store (for WS1 scan/move/metadata, WS3 analyze)

`bc_jobs::JobStore` (clone-cheap; blocking API, use `store.run(|s| ...).await` from async code). The store publishes the
legacy `job.*` / `job.item.*` events itself; workers never publish them.

```rust
let store = jobs.store().clone();
// producer
let job = store.create_job(NewJob::new("scan", vec![NewItem::url(..), ..]).label("Scan /music").priority(100).params(json!({..})))?;
// worker loop (one per kind)
let mut wake = store.subscribe_wake();
loop {
    while let Some(Claimed { item, job }) = store.claim_item("scan", std::process::id() as i64, LEASE_SECONDS)? {
        // heartbeat at least every 30 s while working: returns false if the lease was lost (stop!)
        store.heartbeat(item.id, LEASE_SECONDS)?;
        let mut rep = ProgressReporter::new(&store, &job.id, item.id);   // <= 4 events/s
        rep.report(0.4, "reading tags");
        if store.is_cancel_requested(&job.id)? { store.cancel_item(item.id, "Cancelled")?; continue; }
        match work() {
            Ok(r)  => { store.complete_item(item.id, Complete { message: Some("ok".into()), result: Some(json), release_id: None })?; }
            Err(e) => { store.fail_item(item.id, &e.to_string(), "network", /*retryable*/ true)?; }
        }
    }
    tokio::select! { _ = wake.changed() => {}, _ = tokio::time::sleep(Duration::from_secs(2)) => {} }
}
```
Other calls: `expand_item` (replace an item by N appended items), `skip_item(s)`, `release_item` (pause/shutdown: refund
the attempt), `update_progress`, `set_job_error`, `settle_job`, `cancel_job`, `pause_job`, `resume_job`, `retry_failed`,
`remove_items`, `move_items`, `list_jobs/items/groups`, `select_item_ids`, `reconcile`, `reap_expired`.
Claim order: `ORDER BY job.priority, job.created_at, item.seq` among `pending` items of `queued|running` jobs of that kind
with `next_attempt_at <= now` and `cancel_requested = 0` (same SQL as the legacy `store.py`).
Leases: `claim_item(kind, pid, lease_secs)` sets `lease_expires_at`; the reaper requeues expired `running` items
(`error_class = lease_expired`, or fails them when attempts are exhausted). `complete/fail/skip` are no-ops
(return `false`/`applied=false`) when the item is no longer `running`.
Interrupting in-flight work on cancel/pause: implement `bc_jobs::JobHooks` (`interrupt_job(job_id, Interrupt::{Cancel|Pause})`)
and register it with `JobsService::add_hooks`.

**Job kinds.** Legacy CHECK allows `download, analyze, scan, metadata_bulk, harvest_import`. New kinds (`move`, `harvest`,
`walk`, `sweep`, `enrich`, ...) need `crates/bc-db/migrations/req_ws2_jobs_kinds.sql` (table rebuild that drops the kind CHECK):
**WS1 must wire it in `bc-db/src/migrate.rs`** as the next migration version. Constants: `bc_types::jobs::KIND_*`.

## 2. Routes: `JobsService::router()` (generic job control)

| Method | Path | Body / query | Response |
| --- | --- | --- | --- |
| GET | `/jobs` | `?status&kind&offset&limit(<=200)` | `Page<JobOut>` |
| GET | `/jobs/{id}` | | `JobOut` |
| GET | `/jobs/{id}/items` | `?group&status=a,b&offset&limit` | `Vec<JobItemOut>` |
| GET | `/jobs/{id}/groups` | `?status&offset&limit(<=500)` | `Page<JobItemGroupOut>` |
| POST | `/jobs/{id}/items/move` | `MoveItemsRequest` | `MovedOut` |
| POST | `/jobs/{id}/items/remove` | `ItemSelection` | `RemovedOut` |
| POST | `/jobs/{id}/cancel` | | `JobOut` (already `cancelled`) |
| POST | `/jobs/{id}/pause` | | `JobOut` (400 if finished) |
| POST | `/jobs/{id}/resume` | | `JobOut` |
| POST | `/jobs/{id}/retry` | | `JobOut` |
| POST | `/jobs/clear` | | `ClearedOut` |
| DELETE | `/jobs/{id}` | | 204 |

## 3. Routes: `BandcampService::router()`

### Downloads
| Method | Path | Body / query | Response |
| --- | --- | --- | --- |
| POST | `/downloads/parse` | `ParseUrlsRequest` | `ParsedUrls` |
| POST | `/downloads` | `DownloadRequest` | `JobOut` (idempotent on `job_id`) |
| GET / PUT | `/downloads/disk` | `DiskIn` | `DiskOut` |
| GET / PUT | `/downloads/format` | `DownloadFormatIn` (`format`: a key from `formats`, or `null` for the public stream) | `DownloadFormatOut` |

### Harvest / inbox
| Method | Path | Body / query | Response |
| --- | --- | --- | --- |
| POST | `/harvest/resolve` | `ResolveRequest` | `ResolveResult` (one probe fetch at most) |
| POST | `/harvest/run` | `RunRequest` | **202** `Accepted` (job kind `harvest`; result via `harvest.completed`) |
| GET | `/harvest/runs/{job_id}` | | `RunResult` (once finished; 404 before) |
| GET | `/harvest/items` | `HarvestItemsQuery` | `Page<HarvestItemOut>` |
| GET | `/harvest/stats` | | `{state: count}` |
| GET | `/harvest/tags` | `?state&source_kind&source_label&fan_id&tab&limit` | `Vec<TagCount>` |
| POST | `/harvest/items/queue` | `QueueRequest` | `QueueResult` |
| POST | `/harvest/items/{id}/ignore` | | `HarvestItemOut` (toggles `ignored`/`new`) |
| POST | `/harvest/items/ignore` | `{ids: [..]}` | `Vec<HarvestItemOut>` (batch toggle, one transaction) |
| POST / GET / DELETE | `/harvest/enrich` | `EnrichRequest` | `EnrichState` (202 on POST; job kind `enrich`) |
| GET | `/harvest/labels` | | `LabelResolveStatus` |
| POST | `/harvest/labels/resolve` | | 202 `LabelResolveStatus` |
| GET / POST / DELETE | `/harvest/labels/sweep` | `LabelSweepRequest` | `SweepStatus` (POST = 202) |
| GET / POST / DELETE | `/harvest/favorites/sweep` | | `SweepStatus` (POST = 202) |
| GET / PUT / DELETE | `/harvest/identity` | `CookieRequest` | `IdentityStatus` / 204 (cookie never returned) |
| GET | `/desktop` | | `DesktopInfo` (`bandcamp_login`: the desktop app can open a sign-in window for this caller) |
| POST | `/desktop/bandcamp-login` | | 202; the desktop app opens the window and reports on `bandcamp.login` (`BandcampLoginEvent`); 404 outside the desktop app or from another device |
| GET | `/harvest/health` | | `HarvestHealth` |

### Fans
| Method | Path | Body / query | Response |
| --- | --- | --- | --- |
| GET | `/fans` | | `Vec<FanOut>` |
| POST | `/fans` | `AddFanRequest` | `FanOut` (walk starts as a job) |
| GET | `/fans/peek` | `?url` | `FanPeekOut` |
| GET | `/fans/peek/items` | `?url&which&cursor&fan_id&count` | `FanPeekPageOut` |
| GET / DELETE | `/fans/{id}` | `?releases=adopt` (DELETE) | `FanOut` / 204 |
| POST | `/fans/{id}/walk` | `WalkRequest` | 202 `FanOut` |
| DELETE | `/fans/{id}/walk` | | `FanOut` |
| GET | `/fans/{id}/next` | `?after&order=seq\|shuffle&seed&state&tab&limit` | `FanNextOut` |

### Follows (Feed)
| Method | Path | Body | Response |
| --- | --- | --- | --- |
| GET / POST | `/follows` | `FollowCreate` | `FollowsOut` / `FollowOut` |
| PATCH / DELETE | `/follows/{id}` | `FollowPatch` | `FollowOut` / 204 |
| PUT | `/follows/settings` | `FollowSettingsIn` | `FollowsOut` |
| GET / POST / DELETE | `/follows/sweep` | `FeedSweepRequest` | `FeedSweepStatus` (POST = 202) |

### Explore (prefix `/explore`)
| Method | Path | Query | Response |
| --- | --- | --- | --- |
| GET | `/explore/search` | `q&kind=all\|artist\|album\|track\|fan&limit` | `Vec<SearchHitOut>` |
| GET | `/explore/genres` | `genre` | `GenresOut` (Bandcamp's filter vocabulary) |
| GET | `/explore/discover` | `genre&tags&slice&category_id&geoname_id&time_facet_id&cursor&size` | `DiscoverOut` |
| GET | `/explore/collectors` | `url&limit` | `CollectorsOut` |
| GET | `/explore/band` | `url` | `BandOut` |
| GET | `/explore/release` | `url` | `ExploreReleaseOut` |
| GET | `/explore/related` | `url&tags&size&tag_limit&slice&include_band` | `RelatedOut` |
| GET | `/explore/stream` | `release&track` (`track` = `bc_track_id` or `i<index>`) | audio bytes; **Range passthrough** (206/416), 403 -> re-resolve once; `Cache-Control: private, max-age=600` |
| POST | `/explore/download` | `DownloadReleasesRequest` | `JobOut` |
| POST | `/explore/download/catalog` | `DownloadCatalogRequest` | `CatalogResult` (queueing is a job; the page fetch happens in the request, one `/music` page) |

### Rust functions for WS4's engine (plain async fns on `bc_bandcamp::BandcampService`, no HTTP hop)
```rust
svc.resolve_stream(release_url: &str, track_key: &str, refresh: bool) -> Result<ResolvedStream { url: String /*signed CDN*/, expires_in: Duration }>
    // track_key = bc_track_id or "i<index>"; cached tralbum, pre-resolved on a reserved token lane and refreshed at 80 % of the TTL;
    // refresh=true after a 403 re-resolves once. The signed URL is never stored.
svc.release_tracks(release_url: &str) -> Result<ExploreReleaseOut>        // QueueSource::Explore (tracks carry bc_track_id + proxy stream_url)
svc.fan_next(fan_id, after: Option<i64>, order: FanOrder::{Seq,Shuffle}, seed: u32, states: &[String], tab: FanTab, limit: usize) -> Result<FanNextOut>  // QueueSource::Fan
svc.notify_downloads()                                                    // wake the download worker (WS1's JobHost::notify_download_queue should call this / jobs.store().notify())
```

### Tracklists / locate
| Method | Path | Body | Response |
| --- | --- | --- | --- |
| POST | `/tracklists/parse` | multipart `files` (CSV) | `ParsedTracklists` |
| POST | `/tracklists/match` | `MatchRequest` | `MatchOut` |
| POST | `/artists/{id}/locate`, `/labels/{id}/locate` | | search Bandcamp for the artist/label page and pin it (`LocateOut`) |

`/loved-streams*` is **not** served by WS2: WS1's `bc_maint::router` owns it (DTOs `bc_types::library::LovedStream*`).
WS2 implements `bc_maint::BandcampLookup` for its client (`bc_bandcamp::BcLookup`).

### Wiring for `bc-server` (WS5)
```rust
let jobs = bc_jobs::JobsService::new(db.clone(), bus.clone());
jobs.start().await;                                   // crash recovery first, then the 30 s lease reaper
let bc = bc_bandcamp::service::BandcampService::new(db.clone(), bus.clone(), config.clone(), jobs.clone());
bc.start().await;                                     // download worker + disk guard, harvest/walk/sweep/enrich workers, feed scheduler, stream refresher
let api = Router::new().nest("/api", jobs.router().merge(bc.router()));   // no overlap with bc_maint / bc_library routes
library_service.with_bandcamp(bc.lookup());           // bc_maint::BandcampLookup for stray merge / loved download
// WS1's JobHost::notify_download_queue() -> bc.notify_downloads() (or jobs.store().notify())
```
Downloader: `settings.downloads.downloader` = `"native"` (default) | `"bandcamp-dl"`. After an ingest the worker creates an `analyze`
job (priority 50, `NewItem::track(id)` per track). `bc_bandcamp::replay::replay(&cache, PageKind::Album)` re-runs the extractors over
`cache.db` offline (for `bc bandcamp replay`; returns tier counts + canary-missing count).

## 4. WebSocket topics (payload types in `bc_types::{jobs,bandcamp}`)

| Topic | Payload |
| --- | --- |
| `job.created` | `JobCreated` |
| `job.progress` | `JobProgress` (also when a job settles: status completed/failed/cancelled) |
| `job.paused` `job.resumed` `job.cancelled` `job.deleted` | `JobRef` |
| `job.reordered` / `job.retried` | `JobReordered` / `JobRetried` |
| `job.item.started` / `job.item.skipped` | `JobItemStarted` |
| `job.item.progress` (<= 4/s per item) | `JobItemProgress` |
| `job.item.completed` / `job.item.failed` | `JobItemCompleted` / `JobItemFailed` |
| `downloads.disk` | `DiskOut` |
| `harvest.completed` / `harvest.progress` | `HarvestCompleted` / `HarvestProgress` |
| `harvest.enrich` / `harvest.labels` | `EnrichState` / `LabelResolveStatus` |
| `labels.sweep` / `favorites.sweep` | `SweepStatus` |
| `feed.sweep` | `FeedSweepStatus` |
| `fans.walk` | `WalkState` |
| `loved.reconciled` | (WS1's `bc_types::library` payload; published by the download worker via `bc_maint::loved`) |
| `library.changed` | (WS1's payload; WS2 sends `{ "adopted": n }` / `{ "tracks_added": n }` / `{ "fan_deleted": id }`) |
| `invalidate` | stable entity names (empty ids = all of that kind): `job` (job created/deleted), `inbox_item` (any mutation under `/harvest/items*`), `fan` (any mutation under `/fans*`), `follow` (under `/follows*`), `release` (queue adopted releases; ingest also sends WS1's `track`/`release`). `loved_stream`: sent by WS1's loved routes, not WS2. |

## 5. Pure logic for the UI (wasm-safe): `bc_types::bandcamp::logic`
Ports of `feedGroups`, `bandcampSearch`, `tracklist`, `gate`, `fanQueue`, `exploreSweep` from the legacy frontend.
