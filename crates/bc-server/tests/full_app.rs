//! Builds the whole application with every service mounted. Router merges panic on
//! duplicate paths, so this test is the guard against two workstreams claiming one route.
#![cfg(all(feature = "library", feature = "jobs", feature = "analysis", feature = "recommend", feature = "player"))]
mod common;
use tower::ServiceExt;

#[tokio::test]
async fn full_app_builds_without_route_overlap_and_serves_every_mount() {
    unsafe { std::env::set_var("BC_AUDIO", "null") };
    let dir = tempfile::tempdir().unwrap();
    let app = bc_server::build_app_with(
        common::config(dir.path()),
        bc_server::ServerOptions { no_ui: true, ..Default::default() },
    )
    .unwrap();
    for (path, ok) in [
        ("/api/health", 200),
        ("/api/tracks?limit=1", 200),
        ("/api/library/stats", 200),
        ("/api/jobs?limit=1", 200),
        ("/api/analysis/status", 200),
        ("/api/player/state", 200),
        ("/api/ui-state/none", 200),
        ("/api/not-a-route", 404),
    ] {
        let resp = app.clone().oneshot(common::get(path)).await.unwrap();
        assert_eq!(resp.status(), ok, "{path}");
    }
}

/// Known gap (reported to the orchestrator): nobody mounts `/loved-streams*` yet (WS1 says WS2,
/// WS2 says WS1's bc_maint). Run with `cargo test -- --ignored` to see whether it is fixed.
#[tokio::test]
async fn loved_streams_route_is_served_exactly_once() {
    unsafe { std::env::set_var("BC_AUDIO", "null") };
    let dir = tempfile::tempdir().unwrap();
    let app = bc_server::build_app_with(common::config(dir.path()), bc_server::ServerOptions { no_ui: true, ..Default::default() }).unwrap();
    let resp = app.oneshot(common::get("/api/loved-streams")).await.unwrap();
    assert_eq!(resp.status(), 200);
}
