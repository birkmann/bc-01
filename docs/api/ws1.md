# WS1 API: Data & Library

Paths are **without** the `/api` prefix (`bc-server` nests the router under `/api`). DTOs: `bc_types::library` (wasm-safe, serde
only; split over `library.rs`, `library/{maint,metadata,playlists,scan}.rs`). DJ-set DTOs are WS3's `bc_types::sets`.
Errors are RFC 9457 `problem+json` (`bc_types::Problem`): 400/404/409, 503 + `Retry-After` when the DB is busy.
Timestamps are ISO-8601 UTC strings (`2026-07-28T07:06:39.490752Z`). **No filesystem path is ever returned to a client**
except `RootOut.path` and the folder picker's `BrowseOut` (folders only, never file names); the only request bodies that carry a path
are `POST /library/roots` and the move target.

```rust
// bc-server wiring (WS5)
let lib = bc_library::LibraryService::new(db.clone(), bus.clone(), config.clone());
lib.start().await;                       // watcher on hot roots, lazy art conversion, nothing blocks startup
let api = Router::new().nest("/api", lib.router() /* + other services */);
// optional integrations:
let lib = lib.with_jobs(jobs_host)       // bc_libcore::JobHost (e.g. an adapter over bc-jobs); default = in-memory LocalJobs
              .with_bandcamp(lookup);    // bc_maint::BandcampLookup (strays merge, loved download); default = none (409)
```

## Conventions
* **Paging:** offset based and unlimited. `limit` defaults: tracks 100 (max 5000), releases 60 (max 1000), artists 100 (500), labels 200 (500).
  Every list answers `Page<T> { items, total, offset, limit }` (`TrackPage` adds `total_duration_ms`). The sort is always a total order
  (id tiebreak) so offset paging never duplicates/skips rows.
* **Shuffle:** `sort=random&seed=N` is a seeded-hash order `((id+seed)*2654435761) % 2^32`: stable and pageable for a given seed; no seed = a fresh seed.
* **Search (`q`)** on tracks: FTS5 over title/artist/album/label/tags (unicode61, prefix on the last term). Default order with `q` is bm25
  relevance (capped at the 20 000 best matches); any explicit `sort` turns the match into a filter. Releases/artists/labels: trigram substring match.
* **Tags** filter on the indexed `name_key` (case/accent-insensitive), repeated `tags=` are ANDed.
* **Scope:** every listing accepts `scope=mine|all` and `source_fan_id=`; absent both, the saved `library.unified` setting decides
  (`GET/PUT /library/scope`); Bandcamp teaser clips are hidden when `GET/PUT /library/snippets` says so.
* **Art URLs** carry the immutable version: `/api/art/release/{id}?size=thumb|medium|full&v=<hash>`; `Cache-Control: public, max-age=31536000, immutable`.
* **Long work** returns `202 { "job_id" }` (`bc_types::Accepted`), progress as WS topics below, status via `GET /library/tasks/{id}`.

## Tracks (`bc_types::library::{TrackQuery, TrackOut, TrackPage}`)
| Method | Path | Request | Response |
| --- | --- | --- | --- |
| GET | `/tracks` | `TrackQuery` (q, artist_id, label_id, release_id, release_ids, tags, year_min/max, loved, added_after/before, played, last_played_before, favorites, missing, bpm_min/max, camelot, sort, order, seed, offset, limit, scope, source_fan_id) | `TrackPage` |
| GET | `/tracks/ids` | same filters (no paging) | `Vec<i64>` all ids in listing order (select-all, play-all) |
| GET | `/tracks/export` | filters + `format=m3u8\|csv\|zip` | file (streaming zip) |
| GET | `/tracks/{id}` | | `TrackOut` |
| POST | `/tracks/{id}/love` | | `TrackOut` (toggle; loving adopts the release into my library) |
| POST | `/tracks/love` | `SetLoved { track_ids, loved }` | `ChangedOut { changed }` (assign, not toggle) |
| PUT | `/tracks/{id}/rating` | `SetRating { rating: 0..5 \| null }` | `TrackOut` |
| DELETE | `/tracks/{id}` | | `DeletedOut` (removes file + row) |
| POST | `/tracks/remove` | `RemoveTracksRequest` | `RemovedOut`; rows only, files stay on disk and their paths go on the excluded list so scans skip them (an emptied release goes too) |

`TrackSort`: added, title, artist, album, duration, bpm, play_count, last_played, year, random, relevance, key, energy, rating.
`order` = asc\|desc (default desc; `album` always plays disc/track ascending inside an album).

## Releases
| GET | `/releases` | `ReleaseQuery` (q, artist_id, label_id, tags, loved, missing, added_after/before, sort=added\|title\|year\|artist\|random, order, seed, offset, limit, scope) | `Page<ReleaseOut>` |
| GET | `/releases/ids` | same | `Vec<ReleaseStub>` (whole listing, for select-all) |
| GET | `/releases/{id}` | | `ReleaseOut` |
| GET | `/releases/{id}/next` | listing filters | `ReleaseOut \| null` (the row after this one in that listing) |
| GET | `/releases/{id}/related?limit=` | | `Vec<RelatedGroup>` |
| POST | `/releases/adopt` | `AdoptRequest` | `AdoptResult` |
| POST | `/releases/delete` | `DeleteReleasesRequest` | `DeleteReleasesResult` |
| DELETE | `/releases/{id}` | | `DeletedOut` |
| POST | `/releases/{id}/fill` | | `FillResult` (queues a re-download through the shared download queue) |
| POST | `/releases/fill` | | `FillAllResult` |
| GET | `/releases/strays?label_id=&limit=` | | `StraysOut` |
| POST/GET/DELETE | `/releases/strays/merge` | `StrayMergeRequest` | `StraySweepStatus` (202 on POST) |

## Artists, labels, tags, facets, favorites
| GET | `/artists` | `ArtistQuery` (q, sort=name\|plays\|releases\|tracks\|added, order, offset, limit, scope) | `Page<ArtistOut>` |
| GET | `/artists/{id}` | | `ArtistDetailOut` |
| GET | `/artists/{id}/related?limit=` | | `ArtistRelatedOut` |
| PATCH | `/artists/{id}` | `ArtistPatch { name?, bandcamp_url? }` (a `name_key` collision is 409) | `ArtistDetailOut` |
| GET | `/labels` | `LabelQuery` (q, sort=releases\|tracks\|name\|added, order, offset, limit, scope) | `Page<LabelOut>` |
| GET | `/labels/shuffle?q=&limit=` | | `Page<TrackOut>` (equal share per label, shuffled) |
| GET | `/labels/random?q=&exclude_id=` | | `LabelOut \| null` |
| GET | `/labels/{id}` | | `LabelOut` |
| GET | `/labels/{id}/next` | `q, sort, order` | `LabelOut \| null` |
| PATCH | `/labels/{id}` | `LabelPatch` | `LabelOut` (a rename onto an existing name merges) |
| DELETE | `/labels/{id}` | | `DeleteLabelResult` |
| GET | `/tags` | `TagsQuery` | `Vec<TagOut>` |
| GET | `/facets?limit=` | | `Facets` |
| GET | `/favorites` | | `FavoritesOut` |
| PUT/DELETE | `/favorites/{artist\|label}/{id}` | | 204 (idempotent) |
| PUT/DELETE | `/favorites/tag?name=` | | 204 |

(`POST /labels/{id}/locate`, `/artists/{id}/locate` and all `/loved-streams*` routes belong to WS2. Offline set render is WS4's `GET /sets/{id}/render?format=mp3|wav`; `/sets/{id}/export` serves m3u8/csv/zip only.)

## Library, roots, scanning, stats, history
| GET | `/library/stats` | scope | `LibraryStats` |
| GET | `/library/home?seed=` | scope | `HomeShelves` (one request for the whole Home page) |
| GET/PUT | `/library/scope` | `LibraryScopeIn` | `LibraryScopeOut` |
| GET/PUT | `/library/snippets` | `SnippetSettingIn` | `SnippetSettingOut` |
| GET | `/library/roots` | | `Vec<RootOut>` |
| POST | `/library/roots` | `AddRootRequest` (path!) | `RootOut` |
| GET | `/library/browse?path=&hidden=` | | `BrowseOut`: the subfolders of `path` (`~` expands; empty = the music folder, `XDG_MUSIC_DIR` or `~/Music`, else home), its audio file count, whether it is a library root, and places (home, music folder, drives under `/run/media/$USER`, `/media`, `/mnt`, `/Volumes`, `/`); 404 when missing, 400 for a file or an unreadable folder |
| PATCH | `/library/roots/{id}` | `RootPatch` | `RootOut` |
| DELETE | `/library/roots/{id}` | | 204 |
| POST | `/library/scan?root_id=` | | `202 Accepted` -> `library.scan.*` events, `GET /library/scan/{job_id}` -> `ScanStatus` |
| POST | `/library/scan/{job_id}/cancel` | | `202 Accepted`; stops the scan between files, keeps what was written, marks nothing missing; `library.scan.done` carries `"cancelled": true` and `ScanStatus.state` becomes `cancelled` |
| GET | `/library/excluded` | | `Vec<ExcludedOut>` (files removed from the library, newest first) |
| POST | `/library/excluded/restore` | `RestoreExcludedRequest` | `RestoredOut`; lifts the exclusions and ingests the files still on disk |
| POST | `/library/roots/{id}/move/plan` | `MoveRequest` (path!) | `MovePlanOut` |
| POST | `/library/roots/{id}/move` | `MoveRequest` | `202 Accepted` -> `library.move.progress`; result `MoveResult` |
| POST | `/library/import` | `ImportRequest { from?, force, skip_repairs }` (`from` = legacy db/dir, read-only; a third path-accepting route) | `202 Accepted`; only when the library is empty or `force=true` (409 otherwise); progress on `library.import.progress`, report in the task result |
| GET | `/library/tasks`, `/library/tasks/{id}` | | `TaskInfo` (running/finished tracked tasks) |
| POST | `/history/play` | `PlayEvent` | 204 |
| GET | `/history/top?days=&limit=` | | `HistoryTop` |
| GET | `/history/recent?limit=` | | `Vec<HistoryEntry>` |
| DELETE | `/history?days=` | | `HistoryReset` |

## Media
| GET/HEAD | `/stream/{track_id}` | `Range: bytes=a-b \| a- \| -n` | 200/206/304/416, `ETag`, `Accept-Ranges`; the file is resolved server-side (never a client path) |
| GET | `/art/release/{id}?size=thumb\|medium\|full&v=` | | WebP (or the legacy JPEG until converted), immutable caching |
| GET | `/art/track/{id}?size=` | | falls back to the track's release |

## Playlists and DJ sets (CRUD storage; suggest/automix/pool = WS3, render = WS4)
| GET/POST | `/playlists` | `PlaylistCreate` | `Vec<PlaylistOut>` / `PlaylistOut` |
| PATCH/DELETE | `/playlists/{id}` | `PlaylistPatch` | `PlaylistOut` / 204 |
| POST | `/playlists/from-tracks` | `PlaylistFromTracks` (the `/tracks` filters) | `PlaylistOut` |
| GET | `/playlists/{id}/tracks` | | `Page<TrackOut>` (`item_id` set; smart playlists evaluate their saved filter) |
| POST | `/playlists/{id}/tracks` | `PlaylistAddTracks` | `PlaylistAdded` |
| DELETE | `/playlists/{id}/tracks/{item_id}` | | 204 |
| POST | `/playlists/{id}/tracks/{item_id}/move` | `PlaylistMove` | 204 |
| GET | `/playlists/{id}/export?format=m3u8\|csv\|zip` | | file |
| GET/POST | `/sets` | `DjSetCreate` | `Vec<DjSetListOut>` / `DjSetDetail` |
| GET/PATCH/DELETE | `/sets/{id}` | `DjSetUpdate` | `DjSetDetail` / 204 |
| POST | `/sets/{id}/items` | `AddTracks` | `DjSetDetail` |
| PATCH/DELETE | `/sets/{id}/items/{item_id}` | `SetItemUpdate` | `DjSetDetail` |
| POST | `/sets/{id}/items/{item_id}/move` | `MoveItem` | `DjSetDetail` |
| GET | `/sets/{id}/tracks` | | `Page<TrackOut>` |
| GET | `/sets/{id}/export?format=m3u8\|csv\|zip` | | file |

## Maintenance and metadata
| GET | `/cleanup/candidates?max_track_s=&include_titles=` | | `CleanupOut` |
| GET/POST | `/blacklist` | `BlacklistAdd` | `Page<BlacklistOut>` / `BlacklistOut` |
| DELETE | `/blacklist/{id}` | | 204 |
| GET | `/metadata/status` | | `MetadataStatus` |
| POST | `/metadata/preview` | `PreviewRequest` | `PreviewOut` |
| POST | `/metadata/write` | `WriteRequest` (`dry_run` default true; a real write needs `dry_run=false` **and** `confirm=true`) | `202 { job_id }` + `WriteQueued` |
| POST | `/metadata/tracks/{id}?dry_run=` | | `TrackPlanOut` |
| GET | `/metadata/jobs/{job_id}/snapshot` | | JSONL undo journal |
| POST | `/metadata/jobs/{job_id}/undo` | | `UndoOut` |

## WebSocket topics (payloads in `bc_types::library`)
`library.changed` (`LibraryChanged`), `library.scan.progress` (`ScanProgress`), `library.scan.done`, `library.move.progress` (`MoveProgress`),
`library.strays`, `library.art.progress`, `library.import.progress`, `loved.reconciled`, `metadata.progress` (`MetadataProgress`),
`library.task.started|progress|done` (`TaskInfo`), plus entity-level `invalidate` events (`track|release|artist|label|tag|playlist|set` + ids).

## Rust API for other workstreams
```rust
// WS4 (engine): record a play (history row + play_count/last_played_at), plain blocking fn
bc_library::history::record_play_db(&db, track_id, ms_played, completed, skipped) -> ApiResult<()>
// ui_state key/value (themes, prefs, player queue): bc_db::ui_state::{get(&Connection,key), set(&Transaction,key,value), delete}
// secrets (Bandcamp cookie): bc_libcore::secrets::Secrets::new(&data_dir).{get,set,delete}(secrets::BANDCAMP_COOKIE)
// WS4 (engine): resolve a track id to a file; the path is validated to live under an enabled root.
bc_libcore::resolve_track_file(&conn, track_id) // also re-exported as bc_library::media::resolve_track_file -> Result<ResolvedFile { path, file_id, ext, codec, size_bytes, mtime_ns }, MediaError>
// WS2 (download worker): after a download lands, index it. Idempotent. Publishes library.changed.
bc_library::ingest::ingest_paths(&ctx, root_id, &paths) -> Result<IngestReport>
bc_library::ingest::ingest_dir(&ctx, dir)               -> Result<IngestReport>   // finds the root by ancestry
// Everyone: filters / hydration
bc_libcore::{Ctx, Scope, hydrate::{tracks_out, releases_out}, ApiError}
bc_library::tracks::{page_ids, all_ids}                  // the /tracks filter engine
```
