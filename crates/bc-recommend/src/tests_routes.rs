//! Route-level tests: ports of `test_suggest_route.py`, `test_set_planning.py` and the
//! `/playlists/{id}/similar` cases of `test_playlist_make.py`. Libraries are seeded with raw SQL
//! (set and playlist CRUD belong to WS1) and the router is driven with `oneshot`.

use std::collections::HashSet;

use axum::http::StatusCode;
use serde_json::{Value, json};

use crate::testutil::{Lib, T, get, ids, post};

fn set(v: &[i64]) -> HashSet<i64> {
    v.iter().copied().collect()
}

async fn suggest(lib: &Lib, payload: Value) -> Value {
    let (st, body) = post(lib, "/suggest/next", payload).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    body
}

async fn similar(lib: &Lib, payload: Value) -> Value {
    let (st, body) = post(lib, "/suggest/similar", payload).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    body
}

async fn loved(lib: &Lib, query: &str) -> Value {
    let (st, body) = get(lib, &format!("/suggest/loved{query}")).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    body
}

fn t(title: &str) -> T {
    T::new(title)
}

// --- /suggest/next --------------------------------------------------------------------

#[tokio::test]
async fn strict_harmonic_filters_to_compatible_keys() {
    let mut lib = Lib::new();
    let seed = lib.add(t("seed").bpm(128.0).key("8A"));
    let same = lib.add(t("same").bpm(128.0).key("8A"));
    let up = lib.add(t("up").bpm(128.0).key("9A"));
    let down = lib.add(t("down").bpm(128.0).key("8B"));
    let energy = lib.add(t("energy").bpm(128.0).key("3A"));
    let risky = lib.add(t("risky").bpm(128.0).key("10A"));
    let clash = lib.add(t("clash").bpm(128.0).key("2B"));

    let strict = ids(&suggest(&lib, json!({"seed_track_id": seed})).await);
    assert_eq!(set(&strict), set(&[same, up, down, energy]));
    assert_eq!(strict[0], same);

    let loose = ids(&suggest(&lib, json!({"seed_track_id": seed, "harmonic": "loose"})).await);
    assert!(loose.contains(&risky));
    assert!(!loose.contains(&clash));

    let off = ids(&suggest(&lib, json!({"seed_track_id": seed, "harmonic": "off"})).await);
    assert!(off.contains(&clash));
}

#[tokio::test]
async fn the_seed_itself_and_excludes_never_come_back() {
    let mut lib = Lib::new();
    let seed = lib.add(t("seed").bpm(128.0).key("8A"));
    let a = lib.add(t("a").bpm(128.0).key("8A"));
    let b = lib.add(t("b").bpm(128.0).key("8A"));
    let got = ids(&suggest(&lib, json!({"seed_track_id": seed, "exclude_track_ids": [a]})).await);
    assert_eq!(got, [b]);
    assert!(!got.contains(&seed));
}

#[tokio::test]
async fn raise_moves_the_tempo_window_up() {
    let mut lib = Lib::new();
    let seed = lib.add(t("seed").bpm(128.0).key("8A"));
    let slow = lib.add(t("slow").bpm(118.0).key("8A"));
    let same = lib.add(t("same").bpm(128.0).key("8A"));
    let fast = lib.add(t("fast").bpm(134.0).key("8A"));

    let keep = ids(&suggest(&lib, json!({"seed_track_id": seed, "tempo": "keep"})).await);
    assert_eq!(set(&keep), set(&[same, fast])); // 118 is 7.8% off: out of the window
    assert_eq!(keep[0], same);

    let raised = suggest(&lib, json!({"seed_track_id": seed, "tempo": "raise"})).await;
    assert!((raised["target_bpm"].as_f64().unwrap() - 133.12).abs() < 1e-9);
    assert_eq!(ids(&raised)[0], fast);
    assert!(!ids(&raised).contains(&slow));

    let lowered = ids(&suggest(&lib, json!({"seed_track_id": seed, "tempo": "lower"})).await);
    assert!(lowered.contains(&slow));
    assert!(!lowered.contains(&fast));
}

#[tokio::test]
async fn missing_files_are_never_suggested() {
    let mut lib = Lib::new();
    let seed = lib.add(t("seed").bpm(128.0).key("8A"));
    let gone = lib.add(t("gone").bpm(128.0).key("8A").missing());
    let here = lib.add(t("here").bpm(128.0).key("8A"));
    assert_eq!(ids(&suggest(&lib, json!({"seed_track_id": seed})).await), [here]);
    assert!(!ids(&suggest(&lib, json!({"seed_track_id": seed, "harmonic": "off"})).await).contains(&gone));
}

#[tokio::test]
async fn switch_filters_to_the_target_tags_and_bridges_lead() {
    let mut lib = Lib::new();
    let seed = lib.add(t("seed").bpm(128.0).key("8A").tags(&["techno"]));
    let techno = lib.add(t("techno").bpm(128.0).key("8A").tags(&["techno"]));
    let breaks = lib.add(t("breaks").bpm(128.0).key("8A").tags(&["breaks"]));
    let bridge = lib.add(t("bridge").bpm(128.0).key("8A").tags(&["techno", "breaks"]));

    let got = suggest(&lib, json!({"seed_track_id": seed, "tag_mode": "switch", "tags": ["Breaks"]})).await;
    let got_ids = ids(&got);
    assert!(!got_ids.contains(&techno));
    assert_eq!(got_ids[0], bridge);
    assert!(got_ids.contains(&breaks));
    assert!(got["items"].as_array().unwrap().iter().any(|i| i["why"].as_array().unwrap().contains(&json!("towards: breaks"))));
}

#[tokio::test]
async fn stick_keeps_to_the_seed_tags() {
    let mut lib = Lib::new();
    let seed = lib.add(t("seed").bpm(128.0).key("8A").tags(&["dub techno"]));
    let dub = lib.add(t("dub").bpm(128.0).key("8A").tags(&["dub techno"]));
    let other = lib.add(t("other").bpm(128.0).key("8A").tags(&["trance"]));
    let got = ids(&suggest(&lib, json!({"seed_track_id": seed, "tag_mode": "stick"})).await);
    assert!(got.contains(&dub));
    assert!(!got.contains(&other));
}

#[tokio::test]
async fn allow_tags_keep_only_tracks_carrying_one_of_them() {
    let mut lib = Lib::new();
    let seed = lib.add(t("seed").bpm(128.0).key("8A").tags(&["techno"]));
    let house = lib.add(t("house").bpm(128.0).key("8A").tags(&["house"]));
    let breaks = lib.add(t("breaks").bpm(128.0).key("8A").tags(&["breaks"]));
    let trance = lib.add(t("trance").bpm(128.0).key("8A").tags(&["trance"]));
    let got = ids(&suggest(&lib, json!({"seed_track_id": seed, "allow_tags": ["House", "breaks"]})).await);
    assert_eq!(set(&got), set(&[house, breaks]));
    assert!(!got.contains(&trance));
}

#[tokio::test]
async fn deny_tags_never_come_back_whatever_the_mode() {
    let mut lib = Lib::new();
    let seed = lib.add(t("seed").bpm(128.0).key("8A").tags(&["techno"]));
    let ok = lib.add(t("ok").bpm(128.0).key("8A").tags(&["techno"]));
    let bad = lib.add(t("bad").bpm(128.0).key("8A").tags(&["techno", "hardstyle"]));
    for mode in ["stick", "drift"] {
        let got = ids(&suggest(&lib, json!({"seed_track_id": seed, "tag_mode": mode, "deny_tags": ["Hardstyle"]})).await);
        assert!(got.contains(&ok));
        assert!(!got.contains(&bad));
    }
    // A denied tag beats an allowed one on the same track.
    let got = ids(&suggest(&lib, json!({"seed_track_id": seed, "allow_tags": ["techno"], "deny_tags": ["hardstyle"]})).await);
    assert_eq!(got, [ok]);
}

#[tokio::test]
async fn wishes_float_up_and_a_wished_track_is_shown_even_when_it_clashes() {
    let mut lib = Lib::new();
    let seed = lib.add(t("seed").bpm(128.0).key("8A"));
    let plain = lib.add(t("plain").bpm(128.0).key("8A").label("Nowhere"));
    let on_label = lib.add(t("on label").bpm(128.0).key("8A").label("Hyperdub"));
    let by_artist = lib.add(t("by artist").bpm(128.0).key("8A").artist("Burial"));
    let clashing_wish = lib.add(t("wish").bpm(100.0).key("2B"));

    let got = suggest(
        &lib,
        json!({
            "seed_track_id": seed,
            "wish_label_ids": [lib.label_id("Hyperdub")],
            "wish_artist_ids": [lib.artist_id("Burial")],
            "wish_track_ids": [clashing_wish],
        }),
    )
    .await;
    let got_ids = ids(&got);
    assert_eq!(set(&got_ids[..3]), set(&[on_label, by_artist, clashing_wish]));
    assert_eq!(*got_ids.last().unwrap(), plain);
    let wish_row = got["items"].as_array().unwrap().iter().find(|i| i["track"]["id"] == clashing_wish).unwrap();
    assert_eq!(wish_row["key_verdict"], "clash");
    assert!(wish_row["why"].as_array().unwrap().contains(&json!("wished track")));
}

#[tokio::test]
async fn seed_override_stands_in_for_a_track_the_server_does_not_have() {
    // A Bandcamp stream has a negative id and no row; the client sends what it knows.
    let mut lib = Lib::new();
    let a = lib.add(t("a").bpm(128.0).key("8A"));
    lib.add(t("b").bpm(128.0).key("2B"));
    let got = suggest(&lib, json!({"seed_track_id": -5, "seed": {"bpm": 128, "camelot": "8A"}})).await;
    assert!(got["seed_track_id"].is_null());
    assert_eq!(got["seed_camelot"], "8A");
    assert_eq!(ids(&got), [a]);
}

#[tokio::test]
async fn an_unanalysed_seed_still_gets_a_crate() {
    let mut lib = Lib::new();
    let seed = lib.add(t("seed").tags(&["dub techno"]).unanalysed());
    let m = lib.add(t("match").bpm(140.0).key("1B").tags(&["dub techno"]));
    let stranger = lib.add(t("stranger").bpm(128.0).key("8A").tags(&["pop"]));
    let got = suggest(&lib, json!({"seed_track_id": seed, "tag_mode": "stick"})).await;
    assert!(got["seed_bpm"].is_null());
    assert_eq!(ids(&got), [m]);
    assert!(!ids(&got).contains(&stranger));
}

#[tokio::test]
async fn a_pool_keeps_the_crate_inside_a_playlist_or_the_loved_tracks() {
    let mut lib = Lib::new();
    let seed = lib.add(t("seed").bpm(128.0).key("8A"));
    let inside = lib.add(t("inside").bpm(128.0).key("8A"));
    let outside = lib.add(t("outside").bpm(128.0).key("8A"));
    let lv = lib.add(t("loved").bpm(128.0).key("8A"));
    lib.love(lv);
    let playlist = lib.playlist("Warm-up", &[inside]);

    assert_eq!(ids(&suggest(&lib, json!({"seed_track_id": seed, "playlist_id": playlist})).await), [inside]);
    assert_eq!(ids(&suggest(&lib, json!({"seed_track_id": seed, "loved": true})).await), [lv]);
    let everything = ids(&suggest(&lib, json!({"seed_track_id": seed})).await);
    assert!(set(&[inside, outside, lv]).is_subset(&set(&everything)));
}

#[tokio::test]
async fn response_carries_what_a_row_displays() {
    let mut lib = Lib::new();
    let seed = lib.add(t("seed").bpm(128.0).key("8A").energy(0.5));
    lib.add(t("next").bpm(130.0).key("9A").energy(0.6).tags(&["techno"]));
    let got = suggest(&lib, json!({"seed_track_id": seed, "limit": 5})).await;
    assert_eq!(got["seed_track_id"], seed);
    let item = &got["items"][0];
    assert_eq!(item["track"]["bpm"], 130.0);
    assert_eq!(item["track"]["camelot"], "9A");
    assert_eq!(item["key_verdict"], "good");
    assert!(["good", "perfect"].contains(&item["bpm_verdict"].as_str().unwrap()));
    assert!(item["score"].as_f64().unwrap() > 0.0);
    assert!(!item["why"].as_array().unwrap().is_empty());
    assert_eq!(item["track"]["tags"], json!(["techno"]));
    assert!(item["track"]["stream_url"].as_str().unwrap().contains("/stream/"));
}

// --- /suggest/loved -----------------------------------------------------------------

#[tokio::test]
async fn loved_suggestions_are_empty_without_loved_tracks() {
    let mut lib = Lib::new();
    lib.add(t("A").tags(&["techno"]));
    let body = loved(&lib, "").await;
    assert_eq!(body["profile"]["loved_count"], 0);
    assert_eq!(body["items"], json!([]));
}

#[tokio::test]
async fn loved_suggestions_match_tags_artists_and_labels() {
    let mut lib = Lib::new();
    let a = lib.add(t("Loved one").bpm(143.0).tags(&["hardgroove", "techno"]).artist("Kaipe").label("Trax"));
    let b = lib.add(t("Loved two").bpm(145.0).tags(&["hardgroove"]).artist("Genex").label("Trax"));
    let same_artist = lib.add(t("More Kaipe").bpm(144.0).tags(&["techno"]).artist("Kaipe"));
    let same_label = lib.add(t("On Trax").bpm(142.0).tags(&["house"]).artist("Other").label("Trax"));
    let same_tags = lib.add(t("Groover").bpm(143.0).tags(&["hardgroove", "techno"]).artist("Nobody"));
    let stranger = lib.add(t("Ambient").bpm(80.0).tags(&["ambient"]).artist("Else"));
    lib.add(t("Gone").bpm(143.0).tags(&["hardgroove"]).artist("Kaipe").missing());
    lib.love(a);
    lib.love(b);

    let body = loved(&lib, "").await;
    let got = ids(&body);
    assert!(!got.contains(&a) && !got.contains(&b), "loved tracks are the seed, never the answer");
    assert!(got.contains(&same_tags) && got.contains(&same_artist) && got.contains(&same_label));
    assert!(!got.contains(&stranger), "nothing in common: not in the pool");
    assert!(!body["items"].as_array().unwrap().iter().any(|i| i["track"]["title"] == "Gone"));

    let profile = &body["profile"];
    assert_eq!(profile["loved_count"], 2);
    assert_eq!(profile["tags"][0]["name"], "hardgroove");
    assert_eq!(profile["bpm"], 144.0);
    assert!(profile["artists"] == 2 && profile["labels"] == 1);

    let why = |id: i64| -> Vec<String> {
        body["items"].as_array().unwrap().iter().find(|i| i["track"]["id"] == id).unwrap()["why"]
            .as_array()
            .unwrap()
            .iter()
            .map(|w| w.as_str().unwrap().to_string())
            .collect()
    };
    assert!(why(same_artist).iter().any(|w| w.contains("artist")));
    assert!(why(same_label).iter().any(|w| w.contains("label")));
    assert!(why(same_tags).iter().any(|w| w.starts_with("tags:")));
}

#[tokio::test]
async fn loved_suggestions_can_be_narrowed_to_tags() {
    let mut lib = Lib::new();
    let a = lib.add(t("Loved").tags(&["hardgroove", "house"]).artist("Kaipe"));
    let groove = lib.add(t("Groove").tags(&["hardgroove"]).artist("X"));
    let house = lib.add(t("House").tags(&["house"]).artist("Y"));
    lib.love(a);
    let got = ids(&loved(&lib, "?tags=house").await);
    assert!(got.contains(&house) && !got.contains(&groove));
}

#[tokio::test]
async fn loved_suggestions_are_stable_per_seed() {
    let mut lib = Lib::new();
    let a = lib.add(t("Loved").tags(&["techno"]).artist("Kaipe"));
    for i in 0..6 {
        lib.add(t(&format!("T{i}")).tags(&["techno"]).artist(&format!("A{i}")));
    }
    lib.love(a);
    let one = ids(&loved(&lib, "?seed=1&limit=3").await);
    let again = ids(&loved(&lib, "?seed=1&limit=3").await);
    assert!(one == again && one.len() == 3);
}

// --- /suggest/similar ------------------------------------------------------------------

#[tokio::test]
async fn similar_lifts_the_same_artist_rather_than_penalising_it() {
    let mut lib = Lib::new();
    let seed = lib.add(t("seed").tags(&["techno"]).artist("Vril").bpm(128.0).key("8A"));
    let mine = lib.add(t("mine").tags(&["techno"]).artist("Vril").bpm(128.0).key("8A"));
    let stranger = lib.add(t("stranger").tags(&["techno"]).artist("Other").bpm(128.0).key("8A"));
    let body = similar(&lib, json!({"seed_track_id": seed})).await;
    assert_eq!(ids(&body)[0], mine);
    assert!(ids(&body).contains(&stranger));
    assert!(body["items"][0]["why"].as_array().unwrap().contains(&json!("same artist")));
}

#[tokio::test]
async fn similar_lifts_labelmates() {
    let mut lib = Lib::new();
    let seed = lib.add(t("seed").tags(&["techno"]).artist("A").label("Ostgut"));
    let mate = lib.add(t("mate").tags(&["techno"]).artist("B").label("Ostgut"));
    let outsider = lib.add(t("outsider").tags(&["techno"]).artist("C").label("Other"));
    let order = ids(&similar(&lib, json!({"seed_track_id": seed})).await);
    let pos = |id| order.iter().position(|x| *x == id).unwrap();
    assert!(pos(mate) < pos(outsider));
}

#[tokio::test]
async fn similar_prefers_a_rare_shared_tag_over_a_common_one() {
    let mut lib = Lib::new();
    for i in 0..6 {
        lib.add(t(&format!("filler{i}")).tags(&["electronic"]).artist(&format!("F{i}")));
    }
    let seed = lib.add(t("seed").tags(&["electronic", "dub techno"]).artist("A"));
    let rare = lib.add(t("rare").tags(&["dub techno"]).artist("B"));
    assert_eq!(ids(&similar(&lib, json!({"seed_track_id": seed})).await)[0], rare);
}

#[tokio::test]
async fn the_similar_seed_and_excludes_never_come_back() {
    let mut lib = Lib::new();
    let seed = lib.add(t("seed").tags(&["techno"]).artist("A"));
    let skip = lib.add(t("skip").tags(&["techno"]).artist("B"));
    let keep = lib.add(t("keep").tags(&["techno"]).artist("C"));
    let got = ids(&similar(&lib, json!({"seed_track_id": seed, "exclude_track_ids": [skip]})).await);
    assert_eq!(got, [keep]);
}

#[tokio::test]
async fn missing_files_are_never_similar() {
    let mut lib = Lib::new();
    let seed = lib.add(t("seed").tags(&["techno"]).artist("A"));
    let gone = lib.add(t("gone").tags(&["techno"]).artist("B").missing());
    assert!(!ids(&similar(&lib, json!({"seed_track_id": seed})).await).contains(&gone));
}

#[tokio::test]
async fn a_similar_seed_with_no_label_still_returns_rows_with_label_on() {
    let mut lib = Lib::new();
    let seed = lib.add(t("seed").tags(&["techno"]).artist("A"));
    let other = lib.add(t("other").tags(&["techno"]).artist("B").label("Ostgut"));
    let body = similar(&lib, json!({"seed_track_id": seed, "signals": {"label": true}})).await;
    assert_eq!(ids(&body), [other]);
    assert!(body["seed_label"].is_null());
}

#[tokio::test]
async fn an_unanalysed_seed_still_gets_similar_tracks() {
    let mut lib = Lib::new();
    let seed = lib.add(t("seed").tags(&["techno"]).artist("A").unanalysed());
    let other = lib.add(t("other").tags(&["techno"]).artist("B").bpm(128.0).key("8A"));
    assert_eq!(ids(&similar(&lib, json!({"seed_track_id": seed})).await), [other]);
}

#[tokio::test]
async fn label_alone_keeps_only_labelmates() {
    let mut lib = Lib::new();
    let seed = lib.add(t("seed").tags(&["techno"]).artist("A").label("Ostgut").bpm(128.0).key("8A"));
    let mate = lib.add(t("mate").tags(&["ambient"]).artist("B").label("Ostgut").bpm(90.0).key("3B"));
    let twin = lib.add(t("twin").tags(&["techno"]).artist("A").label("Other").bpm(128.0).key("8A"));
    let only_label = json!({"tags": false, "artist": false, "label": true, "tempo": false, "key": false, "loved": false});
    let got = ids(&similar(&lib, json!({"seed_track_id": seed, "signals": only_label})).await);
    assert_eq!(got, [mate]);
    assert!(!got.contains(&twin));
}

#[tokio::test]
async fn turning_tempo_off_stops_it_ordering_the_list() {
    let mut lib = Lib::new();
    let seed = lib.add(t("seed").tags(&["techno"]).artist("A").bpm(128.0));
    let near = lib.add(t("near").tags(&["techno"]).artist("B").bpm(128.0));
    let far = lib.add(t("far").tags(&["techno"]).artist("C").bpm(175.0));
    assert_eq!(ids(&similar(&lib, json!({"seed_track_id": seed})).await)[0], near);
    let off = similar(&lib, json!({"seed_track_id": seed, "signals": {"tempo": false}})).await;
    let score = |id: i64| off["items"].as_array().unwrap().iter().find(|i| i["track"]["id"] == id).unwrap()["score"].as_f64().unwrap();
    assert_eq!(score(near), score(far));
}

#[tokio::test]
async fn loved_tracks_get_a_nudge() {
    let mut lib = Lib::new();
    let seed = lib.add(t("seed").tags(&["techno"]).artist("A"));
    let plain = lib.add(t("plain").tags(&["techno"]).artist("B"));
    let lv = lib.add(t("loved").tags(&["techno"]).artist("C"));
    lib.love(lv);
    assert_eq!(ids(&similar(&lib, json!({"seed_track_id": seed})).await)[0], lv);
    let off = ids(&similar(&lib, json!({"seed_track_id": seed, "signals": {"loved": false}})).await);
    assert_eq!(set(&off), set(&[plain, lv]));
}

#[tokio::test]
async fn seed_override_stands_in_for_a_bandcamp_stream() {
    let mut lib = Lib::new();
    let m = lib.add(t("match").tags(&["dub techno"]).artist("A"));
    lib.add(t("miss").tags(&["ambient"]).artist("B"));
    let body = similar(&lib, json!({"seed_track_id": null, "seed": {"tags": ["dub techno"], "bpm": 128}})).await;
    assert_eq!(ids(&body), [m]);
    assert!(body["seed_track_id"].is_null());
    assert_eq!(body["seed_tags"], json!(["dub techno"]));
}

#[tokio::test]
async fn similar_pages_without_overlap() {
    let mut lib = Lib::new();
    let seed = lib.add(t("seed").tags(&["techno"]).artist("A"));
    for i in 0..6 {
        lib.add(t(&format!("c{i}")).tags(&["techno"]).artist(&format!("Artist{i}")));
    }
    let first = ids(&similar(&lib, json!({"seed_track_id": seed, "limit": 3})).await);
    let second = ids(&similar(&lib, json!({"seed_track_id": seed, "limit": 3, "offset": 3})).await);
    assert!(first.len() == 3 && second.len() == 3);
    assert!(set(&first).is_disjoint(&set(&second)));
}

#[tokio::test]
async fn the_same_shuffle_seed_gives_the_same_page() {
    let mut lib = Lib::new();
    let seed = lib.add(t("seed").tags(&["techno"]).artist("A"));
    for i in 0..5 {
        lib.add(t(&format!("c{i}")).tags(&["techno"]).artist(&format!("Artist{i}")));
    }
    let once = ids(&similar(&lib, json!({"seed_track_id": seed, "shuffle_seed": 7})).await);
    let again = ids(&similar(&lib, json!({"seed_track_id": seed, "shuffle_seed": 7})).await);
    assert_eq!(once, again);
}

#[tokio::test]
async fn similar_response_carries_the_seed_facts() {
    let mut lib = Lib::new();
    let seed = lib.add(t("seed").tags(&["techno", "dub techno"]).artist("Vril").label("Ostgut").bpm(128.0).key("8A"));
    lib.add(t("other").tags(&["techno"]).artist("B"));
    let body = similar(&lib, json!({"seed_track_id": seed})).await;
    assert_eq!(body["seed_track_id"], seed);
    assert_eq!(body["seed_bpm"], 128.0);
    assert_eq!(body["seed_camelot"], "8A");
    assert_eq!(body["seed_tags"], json!(["dub techno", "techno"]));
    assert_eq!(body["seed_artist"], "Vril");
    assert_eq!(body["seed_label"], "Ostgut");
    assert!(body["pool_size"].as_i64().unwrap() >= 1);
    assert!(!body["items"][0]["why"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn similar_is_empty_for_a_seed_with_nothing_to_go_on() {
    let mut lib = Lib::new();
    lib.add(t("other").tags(&["techno"]).artist("B"));
    let body = similar(&lib, json!({})).await;
    assert_eq!(body["items"], json!([]));
}

#[tokio::test]
async fn similar_reports_the_rare_tags_the_pool_was_drawn_from() {
    let mut lib = Lib::new();
    for i in 0..8 {
        lib.add(t(&format!("filler{i}")).tags(&["electronic"]).artist(&format!("F{i}")));
    }
    let seed = lib.add(t("seed").tags(&["electronic", "dub techno"]).artist("A"));
    lib.add(t("rare").tags(&["dub techno"]).artist("B"));
    let body = similar(&lib, json!({"seed_track_id": seed})).await;
    // Rarest first: which tag the pool leans on is the whole point.
    assert_eq!(body["pool_tags"][0], "dub techno");
    assert_eq!(body["seed_tags"], json!(["dub techno", "electronic"]));
}

#[tokio::test]
async fn a_track_that_only_mixes_well_is_not_similar() {
    let mut lib = Lib::new();
    let seed = lib.add(t("seed").tags(&["techno"]).artist("A").label("Ostgut").bpm(128.0).key("8A"));
    let mixable = lib.add(t("mixable").tags(&["polka"]).artist("B").label("Other").bpm(128.0).key("8A"));
    let kin = lib.add(t("kin").tags(&["techno"]).artist("C").label("Other").bpm(175.0).key("2B"));
    let got = ids(&similar(&lib, json!({"seed_track_id": seed})).await);
    assert_eq!(got, [kin]);
    assert!(!got.contains(&mixable));
}

// --- sets: pool ----------------------------------------------------------------------

async fn pool(lib: &Lib, set_id: i64, query: &str) -> Value {
    let (st, body) = get(lib, &format!("/sets/{set_id}/pool{query}")).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    body
}

fn pool_ids(body: &Value) -> Vec<i64> {
    body["items"].as_array().unwrap().iter().map(|t| t["id"].as_i64().unwrap()).collect()
}

#[tokio::test]
async fn pool_unions_sources_dedups_and_excludes_the_set() {
    let mut lib = Lib::new();
    let both = lib.add(t("both").bpm(128.0).key("8A").tags(&["techno"]));
    let tagged = lib.add(t("tagged").bpm(126.0).key("9A").tags(&["techno"]));
    let listed = lib.add(t("listed").bpm(124.0).key("7A"));
    lib.add(t("stranger").bpm(90.0).key("2B"));
    let playlist = lib.playlist("Warm", &[both, listed]);
    let sources = json!([{"kind": "tag", "tag": "techno"}, {"kind": "playlist", "playlist_id": playlist, "name": "Warm"}]);
    let set_id = lib.dj_set("Test", None, &sources.to_string());

    let body = pool(&lib, set_id, "").await;
    assert_eq!(set(&pool_ids(&body)), set(&[both, tagged, listed]));
    assert_eq!(body["total"], 3);
    assert_eq!(body["excluded_in_set"], 0);
    assert_eq!(body["automix_max"], 200);
    let counts: Vec<i64> = body["sources"].as_array().unwrap().iter().map(|s| s["track_count"].as_i64().unwrap()).collect();
    assert_eq!(counts, [2, 2]);
    assert_eq!(body["sources"][0]["label"], "tag: techno");
    assert_eq!(body["sources"][1]["label"], "playlist: Warm");

    lib.set_add(set_id, &[both]);
    let body = pool(&lib, set_id, "").await;
    assert_eq!(set(&pool_ids(&body)), set(&[tagged, listed]));
    assert_eq!(body["excluded_in_set"], 1);
}

#[tokio::test]
async fn pool_with_no_sources_is_empty() {
    let mut lib = Lib::new();
    lib.add(t("a"));
    let set_id = lib.dj_set("Test", None, "[]");
    assert_eq!(pool(&lib, set_id, "").await["total"], 0);
}

#[tokio::test]
async fn the_pool_of_a_missing_set_is_a_404() {
    let lib = Lib::new();
    let (st, body) = get(&lib, "/sets/99/pool").await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert_eq!(body["status"], 404);
}

#[tokio::test]
async fn dead_refs_resolve_to_nothing_not_an_error() {
    let mut lib = Lib::new();
    lib.add(t("a").bpm(128.0).key("8A"));
    let sources = json!([{"kind": "playlist", "playlist_id": 9999}, {"kind": "tag", "tag": "no such tag"}]);
    let set_id = lib.dj_set("Test", None, &sources.to_string());
    let body = pool(&lib, set_id, "").await;
    assert_eq!(body["total"], 0);
    let counts: Vec<i64> = body["sources"].as_array().unwrap().iter().map(|s| s["track_count"].as_i64().unwrap()).collect();
    assert_eq!(counts, [0, 0]);
}

#[tokio::test]
async fn a_corrupt_pool_column_is_an_empty_pool_not_a_500() {
    let mut lib = Lib::new();
    lib.add(t("a"));
    let set_id = lib.dj_set("Test", None, "{not json");
    assert_eq!(pool(&lib, set_id, "").await["total"], 0);
}

#[tokio::test]
async fn pool_sorts_and_search() {
    let mut lib = Lib::new();
    let fast = lib.add(t("Fast one").bpm(140.0).key("8A").tags(&["techno"]));
    let slow = lib.add(t("Slow one").bpm(100.0).key("8A").tags(&["techno"]));
    let silent = lib.add(t("Unanalysed").unanalysed().tags(&["techno"]));
    lib.reindex();
    let set_id = lib.dj_set("Test", None, &json!([{"kind": "tag", "tag": "techno"}]).to_string());

    assert_eq!(pool_ids(&pool(&lib, set_id, "?sort=bpm").await), [slow, fast, silent], "nulls last");
    assert_eq!(pool_ids(&pool(&lib, set_id, "?sort=bpm&order=desc").await), [fast, slow, silent]);

    let one = pool_ids(&pool(&lib, set_id, "?sort=random&seed=3").await);
    let again = pool_ids(&pool(&lib, set_id, "?sort=random&seed=3").await);
    assert_eq!(one, again);

    let found = pool(&lib, set_id, "?q=slow").await;
    assert_eq!(pool_ids(&found), [slow]);
    assert_eq!(found["total"], 1);
}

#[tokio::test]
async fn missing_files_and_explicit_tracks() {
    let mut lib = Lib::new();
    let here = lib.add(t("here").bpm(128.0).key("8A"));
    let gone = lib.add(t("gone").bpm(128.0).key("8A").missing());
    let set_id = lib.dj_set("Test", None, &json!([{"kind": "tracks", "track_ids": [here, gone]}]).to_string());
    assert_eq!(pool_ids(&pool(&lib, set_id, "").await), [here]);
}

#[tokio::test]
async fn label_and_artist_sources_resolve_through_the_release() {
    let mut lib = Lib::new();
    let a = lib.add(t("a").artist("Burial").label("Hyperdub"));
    let b = lib.add(t("b").artist("Other").label("Hyperdub"));
    let c = lib.add(t("c").artist("Burial"));
    lib.add(t("d").artist("Else"));
    let sources = json!([
        {"kind": "label", "label_id": lib.label_id("Hyperdub")},
        {"kind": "artist", "artist_id": lib.artist_id("Burial")},
    ]);
    let set_id = lib.dj_set("Test", None, &sources.to_string());
    let body = pool(&lib, set_id, "").await;
    assert_eq!(set(&pool_ids(&body)), set(&[a, b, c]));
    let counts: Vec<i64> = body["sources"].as_array().unwrap().iter().map(|s| s["track_count"].as_i64().unwrap()).collect();
    assert_eq!(counts, [2, 2]);
}

// --- sets: suggest -------------------------------------------------------------------

async fn set_suggest(lib: &Lib, set_id: i64, payload: Value) -> Value {
    let (st, body) = post(lib, &format!("/sets/{set_id}/suggest"), payload).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    body
}

#[tokio::test]
async fn suggestions_seed_from_the_tail_at_effective_values() {
    let mut lib = Lib::new();
    let tail = lib.add(t("tail").bpm(125.0).key("8A"));
    let at_pitch = lib.add(t("at pitch").bpm(131.4).key("8A"));
    let at_rest = lib.add(t("at rest").bpm(125.0).key("8A"));
    let set_id = lib.dj_set("Test", None, "[]");
    let item_id = lib.set_add(set_id, &[tail])[0];

    let got = set_suggest(&lib, set_id, json!({})).await;
    assert_eq!(got["seed_bpm"], 125.0);
    assert_eq!(got["after_item_id"], item_id);
    assert_eq!(set(&ids(&got)), set(&[at_pitch, at_rest]));

    // Pitch the tail +5%: the effective seed moves to 131.25.
    lib.exec("UPDATE dj_set_items SET tempo_adjust_pct = 5.0 WHERE id = ?1", [item_id]);
    let got = set_suggest(&lib, set_id, json!({})).await;
    assert!((got["seed_bpm"].as_f64().unwrap() - 131.25).abs() < 1e-9);
    assert_eq!(ids(&got)[0], at_pitch);
}

#[tokio::test]
async fn a_keyless_tail_degrades_instead_of_erroring() {
    let mut lib = Lib::new();
    let tail = lib.add(t("tail").bpm(128.0).tags(&["dub"]));
    let m = lib.add(t("match").bpm(128.0).key("8A").tags(&["dub"]));
    let set_id = lib.dj_set("Test", None, "[]");
    lib.set_add(set_id, &[tail]);
    let got = set_suggest(&lib, set_id, json!({})).await;
    assert!(got["seed_camelot"].is_null());
    assert!(ids(&got).contains(&m));
}

#[tokio::test]
async fn an_empty_set_ranks_the_pool_cold() {
    let mut lib = Lib::new();
    let inside = lib.add(t("inside").bpm(128.0).key("8A").tags(&["techno"]));
    lib.add(t("outside").bpm(128.0).key("8A"));
    let set_id = lib.dj_set("Test", None, &json!([{"kind": "tag", "tag": "techno"}]).to_string());
    let got = set_suggest(&lib, set_id, json!({})).await;
    assert!(got["after_index"].is_null());
    assert_eq!(got["pool_restricted"], true);
    assert_eq!(ids(&got), [inside]);
}

#[tokio::test]
async fn pool_restriction_and_the_library_fallback() {
    let mut lib = Lib::new();
    let tail = lib.add(t("tail").bpm(128.0).key("8A"));
    let inside = lib.add(t("inside").bpm(128.0).key("8A").loved());
    let outside = lib.add(t("outside").bpm(128.0).key("8A"));

    let pooled = lib.dj_set("Pooled", None, &json!([{"kind": "loved"}]).to_string());
    lib.set_add(pooled, &[tail]);
    let got = set_suggest(&lib, pooled, json!({})).await;
    assert_eq!(got["pool_restricted"], true);
    assert_eq!(set(&ids(&got)), set(&[inside]));

    let whole = set_suggest(&lib, pooled, json!({"use_pool": false})).await;
    assert_eq!(whole["pool_restricted"], false);
    assert_eq!(set(&ids(&whole)), set(&[inside, outside]));

    let bare = lib.dj_set("Bare", None, "[]");
    lib.set_add(bare, &[tail]);
    let got = set_suggest(&lib, bare, json!({})).await;
    assert_eq!(got["pool_restricted"], false);
    assert_eq!(set(&ids(&got)), set(&[inside, outside]));
}

#[tokio::test]
async fn set_tracks_are_never_suggested_back() {
    let mut lib = Lib::new();
    let a = lib.add(t("a").bpm(128.0).key("8A"));
    let b = lib.add(t("b").bpm(128.0).key("8A"));
    let c = lib.add(t("c").bpm(128.0).key("8A"));
    let set_id = lib.dj_set("Test", None, "[]");
    lib.set_add(set_id, &[a, b]);
    assert_eq!(set(&ids(&set_suggest(&lib, set_id, json!({})).await)), set(&[c]));
}

#[tokio::test]
async fn suggesting_after_an_unknown_item_is_a_404() {
    let mut lib = Lib::new();
    let a = lib.add(t("a"));
    let set_id = lib.dj_set("Test", None, "[]");
    lib.set_add(set_id, &[a]);
    let (st, _) = post(&lib, &format!("/sets/{set_id}/suggest"), json!({"after_item_id": 9999})).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

// --- sets: automix ---------------------------------------------------------------------

async fn automix(lib: &Lib, set_id: i64, payload: Value) -> (StatusCode, Value) {
    post(lib, &format!("/sets/{set_id}/automix"), payload).await
}

#[tokio::test]
async fn automix_fills_a_set_from_its_pool() {
    let mut lib = Lib::new();
    for i in 0..6 {
        lib.add(t(&format!("t{i}")).bpm(124.0 + i as f64 * 2.0).key(&format!("{}A", 7 + i)).tags(&["techno"]));
    }
    let set_id = lib.dj_set("Test", None, &json!([{"kind": "tag", "tag": "techno"}]).to_string());

    let (st, detail) = automix(&lib, set_id, json!({})).await;
    assert_eq!(st, StatusCode::OK, "{detail}");
    assert_eq!(detail["items"].as_array().unwrap().len(), 6);
    // Deterministic: the same pool arranges the same way.
    automix(&lib, set_id, json!({"keep_existing": false})).await;
    let (_, again) = automix(&lib, set_id, json!({"keep_existing": false})).await;
    let order = |d: &Value| d["items"].as_array().unwrap().iter().map(|i| i["track_id"].as_i64().unwrap()).collect::<Vec<_>>();
    assert_eq!(order(&again), order(&detail));
    // Transition defaults landed on the incoming items.
    for it in &detail["items"].as_array().unwrap()[1..] {
        assert!(["blend", "cut"].contains(&it["transition_type"].as_str().unwrap()));
    }
    // Cue points from the duration heuristic (no waveforms in this library).
    for it in detail["items"].as_array().unwrap() {
        assert_eq!(it["cue_out_ms"], 290_000);
    }
    // The snapshot makes the slot readable without the track.
    assert_eq!(lib.count("SELECT count(*) FROM dj_set_items WHERE snapshot LIKE '%\"title\"%'"), 6);
}

#[tokio::test]
async fn automix_keeps_the_existing_head_and_mixes_out_of_it() {
    let mut lib = Lib::new();
    let head = lib.add(t("head").bpm(128.0).key("8A"));
    let close = lib.add(t("close").bpm(128.0).key("9A").tags(&["pool"]));
    lib.add(t("far").bpm(128.0).key("3B").tags(&["pool"]));
    let set_id = lib.dj_set("Test", None, &json!([{"kind": "tag", "tag": "pool"}]).to_string());
    lib.set_add(set_id, &[head]);

    let (_, detail) = automix(&lib, set_id, json!({})).await;
    assert_eq!(detail["items"][0]["track_id"], head, "the head stays");
    assert_eq!(detail["items"][1]["track_id"], close, "the first addition mixes out of it");
    assert_eq!(detail["items"].as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn automix_respects_the_target_and_refuses_an_empty_pool() {
    let mut lib = Lib::new();
    for i in 0..5 {
        lib.add(t(&format!("t{i}")).bpm(128.0).key("8A").tags(&["x"]).duration(300_000));
    }
    let set_id = lib.dj_set("Test", Some(9), &json!([{"kind": "tag", "tag": "x"}]).to_string());
    // 300s per slot, 15s blend overlap: the second slot crosses 9 minutes.
    let (_, detail) = automix(&lib, set_id, json!({"write_cues": false})).await;
    assert_eq!(detail["items"].as_array().unwrap().len(), 2);

    let empty = lib.dj_set("Empty", None, "[]");
    let (st, body) = automix(&lib, empty, json!({})).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(body["status"], 400);
}

#[tokio::test]
async fn automix_arranges_explicit_picks_and_caps_at_200() {
    let mut lib = Lib::new();
    let a = lib.add(t("a").bpm(128.0).key("8A"));
    let b = lib.add(t("b").bpm(128.0).key("8A"));
    let set_id = lib.dj_set("Test", None, "[]");
    let (st, detail) = automix(&lib, set_id, json!({"track_ids": [a, b], "start_track_id": b})).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(detail["items"][0]["track_id"], b);

    let many: Vec<i64> = (1000..=1200).collect();
    let (st, _) = automix(&lib, set_id, json!({"track_ids": many})).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
}

// --- playlists/{id}/similar ----------------------------------------------------------

async fn similar_playlist(lib: &Lib, id: i64, body: Value) -> (StatusCode, Value) {
    post(lib, &format!("/playlists/{id}/similar"), body).await
}

fn playlist_tracks(lib: &Lib, id: i64) -> Vec<i64> {
    lib.db
        .read(move |c| {
            let mut st = c.prepare("SELECT track_id FROM playlist_items WHERE playlist_id = ?1 ORDER BY position")?;
            Ok(st.query_map([id], |r| r.get::<_, i64>(0))?.collect::<Result<Vec<_>, _>>()?)
        })
        .unwrap()
}

#[tokio::test]
async fn a_similar_playlist_is_the_same_size_and_the_same_kind() {
    let mut lib = Lib::new();
    let source: Vec<i64> =
        (0..4).map(|i| lib.add(t(&format!("src{i}")).artist(&format!("Src{i}")).tags(&["dub techno"]).bpm(125.0).label("Echocord"))).collect();
    let kin: Vec<i64> = (0..8).map(|i| lib.add(t(&format!("kin{i}")).artist(&format!("Kin{i}")).tags(&["dub techno"]).bpm(126.0))).collect();
    let alien: Vec<i64> = (0..8).map(|i| lib.add(t(&format!("alien{i}")).artist(&format!("Alien{i}")).tags(&["polka"]).bpm(180.0))).collect();
    let pid = lib.playlist("Source", &source);

    let (st, body) = similar_playlist(&lib, pid, json!({})).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["name"], "Source (similar)");
    assert_eq!(body["track_count"], source.len());
    assert_eq!(body["kind"], "manual");
    let picked = set(&playlist_tracks(&lib, body["id"].as_i64().unwrap()));
    assert!(picked.is_disjoint(&set(&source)));
    assert!(picked.is_subset(&set(&kin)));
    assert!(picked.is_disjoint(&set(&alien)));
}

#[tokio::test]
async fn a_similar_playlist_can_be_asked_for_a_different_length() {
    let mut lib = Lib::new();
    let source: Vec<i64> = (0..3).map(|i| lib.add(t(&format!("src{i}")).artist(&format!("Src{i}")).tags(&["techno"]))).collect();
    for i in 0..10 {
        lib.add(t(&format!("kin{i}")).artist(&format!("Kin{i}")).tags(&["techno"]));
    }
    let pid = lib.playlist("Source", &source);
    let (_, body) = similar_playlist(&lib, pid, json!({"limit": 2})).await;
    assert_eq!(body["track_count"], 2);
}

#[tokio::test]
async fn similar_playlists_stack_up_rather_than_collide() {
    let mut lib = Lib::new();
    let source: Vec<i64> = (0..2).map(|i| lib.add(t(&format!("src{i}")).artist(&format!("Src{i}")).tags(&["techno"]))).collect();
    for i in 0..10 {
        lib.add(t(&format!("kin{i}")).artist(&format!("Kin{i}")).tags(&["techno"]));
    }
    let pid = lib.playlist("Source", &source);
    let (_, first) = similar_playlist(&lib, pid, json!({})).await;
    let (_, second) = similar_playlist(&lib, pid, json!({"shuffle_seed": 7})).await;
    assert_eq!(first["name"], "Source (similar)");
    assert_eq!(second["name"], "Source (similar) 2");
    assert_ne!(first["id"], second["id"]);
}

#[tokio::test]
async fn a_similar_playlist_never_holds_one_recording_twice() {
    // An album and the single off it are two ids for one track.
    let mut lib = Lib::new();
    let source = vec![lib.add(t("Emerald Coast").artist("Steaw").tags(&["house"]))];
    lib.add(t("Emerald Coast").artist("Steaw").tags(&["house"]));
    lib.add(t("Bassline").artist("Steaw").tags(&["house"]));
    lib.add(t("Bassline").artist("Steaw").tags(&["house"]));
    let pid = lib.playlist("Source", &source);
    let (_, body) = similar_playlist(&lib, pid, json!({})).await;
    let picked = playlist_tracks(&lib, body["id"].as_i64().unwrap());
    let titles: Vec<String> = lib
        .db
        .read(move |c| {
            let mut st = c.prepare("SELECT t.title FROM playlist_items pi JOIN tracks t ON t.id = pi.track_id WHERE pi.playlist_id = ?1")?;
            Ok(st.query_map([body["id"].as_i64().unwrap()], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?)
        })
        .unwrap();
    assert_eq!(titles, ["Bassline"]);
    assert_eq!(picked.len(), 1);
}

#[tokio::test]
async fn a_similar_playlist_of_an_empty_playlist_is_a_400() {
    let lib = Lib::new();
    let pid = lib.playlist("Source", &[]);
    assert_eq!(similar_playlist(&lib, pid, json!({})).await.0, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_similar_playlist_with_no_neighbours_is_a_400() {
    let mut lib = Lib::new();
    let alone = lib.add(t("alone").tags(&["very rare tag"]));
    let pid = lib.playlist("Source", &[alone]);
    assert_eq!(similar_playlist(&lib, pid, json!({})).await.0, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_similar_playlist_of_a_playlist_that_is_not_there_is_a_404() {
    let lib = Lib::new();
    assert_eq!(similar_playlist(&lib, 999, json!({})).await.0, StatusCode::NOT_FOUND);
}

// --- scope ---------------------------------------------------------------------------------

#[tokio::test]
async fn the_library_scope_hides_other_peoples_shelves_unless_asked() {
    let mut lib = Lib::new();
    let seed = lib.add(t("seed").bpm(128.0).key("8A"));
    let mine = lib.add(t("mine").bpm(128.0).key("8A"));
    let theirs = lib.add(t("theirs").bpm(128.0).key("8A"));
    lib.exec("INSERT INTO fans(id, username, url, is_self, created_at) VALUES (1, 'x', 'https://bandcamp.com/x', 0, '2026-01-01')", []);
    lib.exec("UPDATE releases SET source_fan_id = 1 WHERE id = (SELECT release_id FROM tracks WHERE id = ?1)", [theirs]);

    // The default (the saved switch is off) is "mine".
    assert_eq!(ids(&suggest(&lib, json!({"seed_track_id": seed})).await), [mine]);
    let (st, all) = post(&lib, "/suggest/next?scope=all", json!({"seed_track_id": seed})).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(set(&ids(&all)), set(&[mine, theirs]));
    let (_, fan) = post(&lib, "/suggest/next?source_fan_id=1", json!({"seed_track_id": seed})).await;
    assert_eq!(ids(&fan), [theirs]);
}
