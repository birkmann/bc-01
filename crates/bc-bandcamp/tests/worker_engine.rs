//! Worker-level cases of `test_download_engine.py`: per-item staging, the merge, the label / shelf
//! stamping, partial / salvage / `-f` handling -- driven by the fake bandcamp-dl through the real
//! `BandcampDl` adapter, with a fake `LibraryPort` (raw-SQL artist/release/track/file rows).

mod worker_common;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use bc_bandcamp::download::bcdl::snapshot_tree;
use bc_bandcamp::download::worker::staging::{merge_staging, purge_stale_staging};
use bc_bandcamp::download::worker::url_plausibly_names;
use bc_db::rusqlite::{OptionalExtension, params};
use bc_jobs::{HandlerOutcome, NewItem, NewJob};
use worker_common::*;

fn write(p: &Path, bytes: &[u8]) {
    std::fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
    std::fs::write(p, bytes).expect("write");
}

// -- merge_staging ------------------------------------------------------------------------------

#[test]
fn merge_staging_moves_the_tree_and_reports_audio() {
    let tmp = tempfile::tempdir().expect("tmp");
    let staging = tmp.path().join(".staging").join("item-1");
    let album = staging.join("Artist").join("Album");
    write(&album.join("01 - a.mp3"), b"\xff\xfb");
    write(&album.join("cover.jpg"), b"\xff\xd8\xff");
    write(&album.join("02 - b.mp3.tmp"), b"partial");
    write(&album.join("03 - c.mp3.part"), b"partial");
    let target = tmp.path().join("downloads");

    let audio = merge_staging(&staging, &target);

    assert_eq!(audio, vec![target.join("Artist").join("Album").join("01 - a.mp3")]);
    assert!(target.join("Artist/Album/cover.jpg").exists(), "art travels with the music");
    assert_eq!(count_files(&target, ".tmp") + count_files(&target, ".part"), 0, "partials must never reach the library");
    assert!(!staging.exists());
}

#[test]
fn merge_staging_discards_a_tree_with_no_audio() {
    let tmp = tempfile::tempdir().expect("tmp");
    let staging = tmp.path().join(".staging").join("item-2");
    write(&staging.join("Artist/Album/cover.jpg"), b"\xff\xd8\xff");
    let target = tmp.path().join("downloads");

    assert!(merge_staging(&staging, &target).is_empty());
    assert!(!staging.exists());
    assert!(!target.exists());
}

// -- the worker end to end ----------------------------------------------------------------------

#[tokio::test]
async fn worker_downloads_via_staging_and_merges_on_success() {
    let env = Env::new();
    let handler = env.handler(bcdl("success"));
    let item_id = env.queue_claimed(GRID);

    env.run_item(&handler, item_id).await;

    let item = env.item(item_id);
    assert_eq!(item.status, "done");
    let release_id = item.release_id.expect("a release was ingested");
    let (url, folder): (Option<String>, Option<String>) = env
        .db
        .read(move |c| Ok(c.query_row("SELECT bandcamp_url, folder_path FROM releases WHERE id = ?1", [release_id], |r| Ok((r.get(0)?, r.get(1)?)))?))
        .expect("release");
    assert_eq!(url.as_deref(), Some(GRID));
    assert!(!folder.unwrap_or_default().contains(".staging"));
    let paths: Vec<String> = env
        .db
        .read(|c| {
            let mut st = c.prepare("SELECT path FROM files")?;
            Ok(st.query_map([], |r| r.get::<_, String>(0))?.collect::<Result<_, _>>()?)
        })
        .expect("paths");
    assert!(!paths.is_empty() && paths.iter().all(|p| !p.contains(".staging")), "no .staging path may reach the database: {paths:?}");

    let root = env.downloads();
    assert_eq!(count_files(&root.join("Somatic").join("Grid Failure"), ".mp3"), 4);
    assert!(!root.join(".staging").join(format!("item-{item_id}")).exists());
    // One `analyze` job for the new tracks.
    assert_eq!(env.count("SELECT count(*) FROM jobs WHERE kind = 'analyze'"), 1);
    assert_eq!(env.count("SELECT count(*) FROM job_items ji JOIN jobs j ON j.id = ji.job_id WHERE j.kind = 'analyze'"), 4);
    let (label, prio): (String, i64) =
        env.db.read(|c| Ok(c.query_row("SELECT label, priority FROM jobs WHERE kind = 'analyze'", [], |r| Ok((r.get(0)?, r.get(1)?)))?)).expect("analyze job");
    assert_eq!((label.as_str(), prio), ("Analyze new downloads", 50));
}

#[tokio::test]
async fn label_page_job_files_its_releases_under_the_label() {
    let env = Env::new();
    let handler = env.handler(bcdl("success"));
    let item_id = env.queue_claimed_with(
        GRID,
        serde_json::json!({"label_name": "Player", "label_url": "https://player.bandcamp.com"}),
        None,
    );

    env.run_item(&handler, item_id).await;

    let item = env.item(item_id);
    assert_eq!(item.status, "done");
    let rid = item.release_id.expect("release");
    let (label_id, name, url): (Option<i64>, Option<String>, Option<String>) = env
        .db
        .read(move |c| {
            let label_id: Option<i64> = c.query_row("SELECT label_id FROM releases WHERE id = ?1", [rid], |r| r.get(0))?;
            let row: Option<(String, Option<String>)> = c
                .query_row("SELECT name, bandcamp_url FROM labels WHERE id = ?1", [label_id], |r| Ok((r.get(0)?, r.get(1)?)))
                .optional()?;
            Ok((label_id, row.clone().map(|r| r.0), row.and_then(|r| r.1)))
        })
        .expect("label");
    assert!(label_id.is_some());
    assert_eq!(name.as_deref(), Some("Player"));
    assert_eq!(url.as_deref(), Some("https://player.bandcamp.com"));
}

#[tokio::test]
async fn job_without_label_params_leaves_the_release_unlabelled() {
    let env = Env::new();
    let handler = env.handler(bcdl("success"));
    let item_id = env.queue_claimed(GRID);

    env.run_item(&handler, item_id).await;

    let item = env.item(item_id);
    assert_eq!(item.status, "done");
    let rid = item.release_id.expect("release");
    let label: Option<i64> = env.db.read(move |c| Ok(c.query_row("SELECT label_id FROM releases WHERE id = ?1", [rid], |r| r.get(0))?)).expect("label");
    assert_eq!(label, None);
}

#[test]
fn concurrent_items_cannot_see_each_others_files() {
    // While item A is between its snapshots, item B's finished album must not enter A's diff --
    // which is what stamped A's URL (and through it A's label) onto B's release. With per-item
    // staging A's diff stays empty of it.
    let env = Env::new();
    let staging_a = env.downloads().join(".staging").join("item-1");
    std::fs::create_dir_all(&staging_a).expect("mkdir");

    let before = snapshot_tree(&staging_a);
    let other = env.downloads().join("Other Artist").join("Other Album");
    write(&other.join("01 - theirs.mp3"), b"\xff\xfb");
    let after = snapshot_tree(&staging_a);

    assert_eq!(after, before, "a sibling's files must be invisible to this item's diff");
}

#[tokio::test]
async fn partial_leaves_staging_for_the_retry_and_ingests_nothing() {
    let env = Env::new();
    let item_id = env.queue_claimed(GRID);
    let staging = env.downloads().join(".staging").join(format!("item-{item_id}"));

    env.run_item(&env.handler(bcdl("partial")), item_id).await;

    assert_eq!(env.item(item_id).status, "pending", "a partial with attempts left must be retried");
    assert_eq!(env.count("SELECT count(*) FROM tracks"), 0);
    assert!(count_files(&staging, ".mp3") > 0, "the partial download must survive for the resume");
    assert_eq!(count_files(&env.downloads().join("Somatic"), ".mp3"), 0);

    // The retry succeeds and the whole album -- including the first attempt's tracks, never
    // ingested from staging -- lands in the library.
    env.clear_backoff(item_id);
    assert_eq!(env.claim(), Some(item_id));
    env.run_item(&env.handler(bcdl("success")), item_id).await;

    assert_eq!(env.item(item_id).status, "done");
    assert_eq!(env.count("SELECT count(*) FROM tracks"), 4);
    assert!(!staging.exists());
    assert_eq!(count_files(&env.downloads().join("Somatic").join("Grid Failure"), ".mp3"), 4);
}

#[tokio::test]
async fn terminal_failure_salvages_what_landed() {
    let env = Env::new();
    let item_id = env.queue_claimed(GRID);
    env.set_max_attempts(item_id, 1); // this attempt is the last

    env.run_item(&env.handler(bcdl("partial")), item_id).await;

    let item = env.item(item_id);
    assert_eq!(item.status, "failed");
    assert_eq!(env.count("SELECT count(*) FROM tracks"), 2);
    assert!(!env.downloads().join(".staging").join(format!("item-{item_id}")).exists());
    assert_eq!(count_files(&env.downloads().join("Somatic").join("Grid Failure"), ".mp3"), 2);
    // The expected count is recorded so the grid can say "2/4".
    assert_eq!(env.count("SELECT expected_track_count FROM releases LIMIT 1"), 4);
    // And the failure is mirrored to error.log.
    let log = std::fs::read_to_string(env.downloads().join("error.log")).expect("error.log");
    assert!(log.contains(&format!("Failed: {GRID}")), "{log}");
}

#[tokio::test]
async fn a_part_streamable_release_downloads_what_it_can() {
    // The Loved-shelf failure: every album behind a loved stream is one whose tracks are not all
    // public, so `-f` skipped all of them and the job ended "0 ok, 1 failed" after three identical
    // retries. One run without the flag gets the music the heart was actually about.
    let env = Env::new();
    let item_id = env.queue_claimed(GRID);

    env.run_item(&env.handler(bcdl("part_stream")), item_id).await;

    let item = env.item(item_id);
    assert_eq!(item.status, "done");
    assert_eq!(env.count("SELECT count(*) FROM tracks"), 1);
    assert!(item.message.unwrap_or_default().contains("streamable"));
    assert_eq!(count_files(&env.downloads().join("Somatic").join("Grid Failure"), ".mp3"), 1);
}

#[tokio::test]
async fn a_release_that_streams_nothing_is_not_retried() {
    // Availability belongs to the release, not to the attempt. Once the run without `-f` has also
    // come back empty, two more of the same command are two more minutes spent proving it again.
    let env = Env::new();
    let item_id = env.queue_claimed(GRID);
    let argv_log = env.dir.path().join("argv.log");
    // The first run reports the -f skip; the fallback finds nothing to fetch.
    let script = env.dir.path().join("bandcamp-dl");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nfor a in \"$@\"; do if [ \"$a\" = \"-f\" ]; then FAKE_BCDL_MODE=part_stream exec \"{FAKE}\" \"$@\"; fi; done\nFAKE_BCDL_MODE=silent_fail exec \"{FAKE}\" \"$@\"\n"
        ),
    )
    .expect("script");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let dl = std::sync::Arc::new(bc_bandcamp::download::worker::BcdlDownloader::new(
        bc_bandcamp::download::bcdl::BandcampDl::new(script.to_string_lossy())
            .with_env("FAKE_BCDL_ARGV_LOG", argv_log.to_string_lossy())
            .with_sweep(bc_bandcamp::download::bcdl::SweepConfig { probe: std::time::Duration::from_millis(50), ..Default::default() }),
    ));

    env.run_item(&env.handler(dl), item_id).await;

    let argv: Vec<String> = std::fs::read_to_string(&argv_log).expect("argv log").lines().map(str::to_string).collect();
    assert_eq!(argv.len(), 2, "the flag is dropped exactly once: {argv:?}");
    assert!(argv[0].contains(" -f ") && !argv[1].contains(" -f "), "{argv:?}");
    let item = env.item(item_id);
    assert_eq!(item.status, "failed", "a retry would run the identical command");
    assert_eq!(item.error_class.as_deref(), Some("no_output"));
    assert_eq!(item.last_error.as_deref(), Some("None of this release is publicly streamable."));
}

// -- staging purge / URL guard -----------------------------------------------------------------

#[test]
fn stale_staging_dirs_are_purged_except_live_items() {
    let env = Env::new();
    let base = env.dir.path().join("downloads");
    let job = env.create_download(&["https://x.bandcamp.com/album/y"]);
    let pending_id: i64 = env.db.read(|c| Ok(c.query_row("SELECT id FROM job_items", [], |r| r.get(0))?)).expect("id");
    let _ = job;

    let live = base.join(".staging").join(format!("item-{pending_id}"));
    let orphan = base.join(".staging").join("item-99999");
    let stray = base.join(".staging").join("not-an-item");
    for d in [&live, &orphan, &stray] {
        std::fs::create_dir_all(d).expect("mkdir");
    }

    assert_eq!(purge_stale_staging(&env.db, &base), 1);

    assert!(live.exists(), "a pending item resumes into its staging");
    assert!(!orphan.exists());
    assert!(stray.exists(), "only item-* dirs belong to the worker");
}

#[test]
fn url_guard_rejects_a_slug_that_names_another_record() {
    let folder = Some("music/heiko-laux/klockworks-24");
    assert!(url_plausibly_names("https://heikolaux.bandcamp.com/album/klockworks-24", "Klockworks 24", folder));
    assert!(!url_plausibly_names("https://mehen.bandcamp.com/album/hoxa-8-harmony-ep", "Klockworks 24", folder));
}

// -- whose shelf a download lands on -------------------------------------------------------------

fn source_fan(env: &Env, release_id: i64) -> Option<i64> {
    env.db.read(move |c| Ok(c.query_row("SELECT source_fan_id FROM releases WHERE id = ?1", [release_id], |r| r.get(0))?)).expect("source_fan_id")
}

#[tokio::test]
async fn a_shelf_job_files_the_release_it_creates_under_the_fan() {
    let env = Env::new();
    let handler = env.handler(bcdl("success"));
    let fan_id = env.fan("alice");
    let item_id = env.queue_claimed_with(GRID, serde_json::json!({"source_fan_id": fan_id}), Some("fan-alice"));

    env.run_item(&handler, item_id).await;

    let item = env.item(item_id);
    assert_eq!(item.status, "done");
    assert_eq!(source_fan(&env, item.release_id.expect("release")), Some(fan_id));
    assert!(env.downloads().join("fan-alice").join("Somatic").join("Grid Failure").is_dir());
}

#[tokio::test]
async fn a_personal_job_adopts_a_release_that_sat_on_a_shelf() {
    // Force-downloading for myself what is already on alice's shelf merges into the same release
    // row (same artist, title) -- which must then be mine, not stay hidden behind hers.
    let env = Env::new();
    let handler = env.handler(bcdl("success"));
    let fan_id = env.fan("alice");
    let first = env.queue_claimed_with(GRID, serde_json::json!({"source_fan_id": fan_id}), Some("fan-alice"));
    env.run_item(&handler, first).await;

    let second = env.queue_claimed_with(GRID, serde_json::json!({"force": true}), Some("fan-alice"));
    env.run_item(&handler, second).await;

    assert_eq!(source_fan(&env, env.item(first).release_id.expect("release")), None);
    assert_eq!(env.count("SELECT count(*) FROM releases"), 1);
}

#[tokio::test]
async fn a_personal_job_skipping_a_shelved_record_adopts_it() {
    let env = Env::new();
    let handler = env.handler(bcdl("success"));
    let fan_id = env.fan("alice");
    let first = env.queue_claimed_with(GRID, serde_json::json!({"source_fan_id": fan_id}), Some("fan-alice"));
    env.run_item(&handler, first).await;

    let again = env.queue_claimed_with(GRID, serde_json::json!({}), Some("fan-alice"));
    env.run_item(&handler, again).await;

    let skipped = env.item(again);
    assert_eq!(skipped.status, "skipped");
    assert!(skipped.message.unwrap_or_default().contains("moved into your library"));
    assert_eq!(source_fan(&env, env.item(first).release_id.expect("release")), None);
}

#[tokio::test]
async fn a_second_shelf_job_leaves_the_first_shelf_alone() {
    // Two people wishing for the same record: it stays on whoever's shelf got it first.
    let env = Env::new();
    let handler = env.handler(bcdl("success"));
    let alice = env.fan("alice");
    let bob = env.fan("bob");
    let first = env.queue_claimed_with(GRID, serde_json::json!({"source_fan_id": alice}), Some("fan-alice"));
    env.run_item(&handler, first).await;
    let second = env.queue_claimed_with(GRID, serde_json::json!({"source_fan_id": bob}), Some("fan-bob"));
    env.run_item(&handler, second).await;

    assert_eq!(env.item(second).status, "skipped");
    assert_eq!(source_fan(&env, env.item(first).release_id.expect("release")), Some(alice));
}

// -- other worker behaviour ------------------------------------------------------------------------

#[tokio::test]
async fn an_unsupported_url_is_explained_once_and_not_retried() {
    let env = Env::new();
    // `/artists` is neither expandable nor downloadable.
    let item_id = {
        env.store().create_job(NewJob::new("download", vec![NewItem::url("https://lemos.bandcamp.com/artists", "artist")])).expect("job");
        env.claim().expect("claim")
    };
    env.run_item(&env.handler(bcdl("success")), item_id).await;

    let item = env.item(item_id);
    assert_eq!(item.status, "failed", "no amount of retrying changes the answer");
    assert_eq!(item.error_class.as_deref(), Some("unsupported_url"));
    assert!(item.last_error.unwrap_or_default().contains("/album/ and /track/"));
}

#[tokio::test]
async fn a_missing_binary_fails_the_item_for_good() {
    let env = Env::new();
    let item_id = env.queue_claimed(GRID);
    let outcome = env.run_item(&env.handler(bcdl_with("/nonexistent/bandcamp-dl", "success")), item_id).await;

    assert!(matches!(outcome, HandlerOutcome::Failed { ref class, retryable: false, .. } if class == "missing_binary"), "{outcome:?}");
    assert_eq!(env.item(item_id).status, "failed");
}

#[tokio::test]
async fn the_downloader_is_chosen_by_the_setting() {
    // `downloads.downloader`: native (default) | bandcamp-dl, read per item.
    let env = Env::new();
    let deps = env.deps();
    assert_eq!(deps.pick_downloader().name(), "native");
    env.db.write(|t| bc_db::settings::set(t, "downloads.downloader", "\"bandcamp-dl\"")).expect("set");
    assert_eq!(deps.pick_downloader().name(), "bandcamp-dl");
    env.db.write(|t| bc_db::settings::set(t, "downloads.downloader", "native")).expect("set");
    assert_eq!(deps.pick_downloader().name(), "native");
    let _ = params![];
}
