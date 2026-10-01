//! Album / track pages: `data-tralbum` -> JSON-LD -> CSS ladder.

use scraper::Html;
use serde_json::{Map, Value};

use crate::urls::{artist_root, build_art_url, normalise};
use super::util::{
    as_float, as_int, attr, attr_json, css_first, css_all, node_first, obj, opt_string, parse_bc_date,
    pystr, str_or_empty, text_strip, truthy,
};
use super::{CANARY_KEY, HarvestedRelease, HarvestedTrack, Tier};

/// Port of `parse_tralbum`: extract a release, degrading through the ladder rather than failing.
pub fn parse_tralbum(html_text: &str, url: &str) -> HarvestedRelease {
    let doc = Html::parse_document(html_text);
    let mut release = HarvestedRelease { url: normalise(url), ..Default::default() };

    let tralbum = attr_json(&doc, "script[data-tralbum]", "data-tralbum");
    let band = attr_json(&doc, "script[data-band]", "data-band");
    let ld = jsonld(&doc);
    let ld_truthy = ld.as_ref().filter(|m| !m.is_empty());

    if let Some(Value::Object(tr)) = &tralbum {
        release.tier = Tier::Blob;
        from_tralbum(&mut release, tr);
        if !tr.contains_key(CANARY_KEY) {
            release.missing.push("canary".into());
            tracing::warn!("data-tralbum canary missing for {url} -- blob format may have changed");
        }
    } else if ld_truthy.is_some() {
        release.tier = Tier::JsonLd;
        release.missing.push("data-tralbum".into());
    } else {
        release.tier = Tier::Css;
        release.missing.extend(["data-tralbum".to_string(), "jsonld".to_string()]);
    }

    if let Some(Value::Object(band)) = &band {
        if truthy(band.get("id")) {
            release.band_id = as_int(band.get("id"));
        }
        if release.artist_name.is_empty() {
            release.artist_name = str_or_empty(band.get("name"));
        }
    }

    if let Some(ld) = ld_truthy {
        from_jsonld(&mut release, ld);
    }

    from_css(&mut release, &doc);

    if release.item_type.is_empty() || release.item_type == "album" {
        release.item_type = if url.contains("/track/") { "track" } else { "album" }.into();
    }
    if release.art_url.is_none() {
        release.art_url = build_art_url(release.art_id);
    }
    release
}

/// Port of the canary check in `parse_tralbum`: `Some(true)` when the page's `data-tralbum`
/// blob carries the `"for the curious"` key, `Some(false)` when the blob is there without
/// it (format drift), `None` when the page has no parsable `data-tralbum` at all.
pub fn tralbum_has_canary(html_text: &str) -> Option<bool> {
    let doc = Html::parse_document(html_text);
    match attr_json(&doc, "script[data-tralbum]", "data-tralbum") {
        Some(Value::Object(m)) => Some(m.contains_key(CANARY_KEY)),
        _ => None,
    }
}

/// Port of `_jsonld`: the first `application/ld+json` script that parses to an object.
fn jsonld(doc: &Html) -> Option<Map<String, Value>> {
    for node in css_all(doc, r#"script[type="application/ld+json"]"#) {
        let text: String = node.text().collect::<String>();
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        if let Ok(Value::Object(m)) = serde_json::from_str::<Value>(text) {
            return Some(m);
        }
    }
    None
}

fn from_tralbum(release: &mut HarvestedRelease, data: &Map<String, Value>) {
    let current = obj(data.get("current"));

    release.item_type = match data.get("item_type") {
        v if truthy(v) => pystr(v.unwrap_or(&Value::Null)),
        _ => "album".into(),
    };
    release.bc_item_id = as_int(data.get("id"));
    release.art_id = as_int(data.get("art_id"));
    release.artist_name = str_or_empty(data.get("artist"));
    release.title = str_or_empty(current.get("title"));
    release.band_id = as_int(current.get("band_id"));
    release.about = opt_string(current.get("about"));
    release.credits = opt_string(current.get("credits"));
    let date_val = if truthy(data.get("album_release_date")) {
        data.get("album_release_date")
    } else {
        current.get("release_date")
    };
    release.release_date = match date_val {
        Some(v) if truthy(Some(v)) => parse_bc_date(Some(&pystr(v))),
        _ => None,
    };
    release.is_preorder = truthy(data.get("is_preorder")) || truthy(data.get("album_is_preorder"));
    release.is_private = truthy(current.get("private")) || truthy(data.get("is_private_stream"));

    let minimum = current.get("minimum_price").filter(|v| !v.is_null());
    release.price = match data.get("defaultPrice") {
        Some(v) if !v.is_null() => as_float(Some(v)),
        _ => as_float(minimum),
    };
    release.currency = opt_string(current.get("currency"));
    // A free download page, or a name-your-price release with no floor, is freely offered by
    // the artist -- the queue treats that as in-scope.
    release.is_free_download = truthy(data.get("freeDownloadPage"))
        || minimum.is_some_and(|m| as_float(Some(m)).is_some_and(|f| f == 0.0));
    release.is_purchasable =
        truthy(current.get("purchase_url")) || truthy(data.get("PAID")) || truthy(minimum);

    if let Some(Value::Array(trackinfo)) = data.get("trackinfo") {
        for entry in trackinfo {
            let Value::Object(entry) = entry else { continue };
            let track_url = if truthy(entry.get("title_link")) {
                let base = release.url.split("/album/").next().unwrap_or("");
                let base = base.split("/track/").next().unwrap_or("");
                Some(format!("{base}{}", str_or_empty(entry.get("title_link"))))
            } else {
                None
            };
            let track_id = if truthy(entry.get("track_id")) { entry.get("track_id") } else { entry.get("id") };
            release.tracks.push(HarvestedTrack {
                title: str_or_empty(entry.get("title")),
                track_num: as_int(entry.get("track_num")),
                duration_sec: as_float(entry.get("duration")),
                url: track_url,
                artist: opt_string(entry.get("artist")),
                bc_track_id: as_int(track_id),
                has_lyrics: truthy(entry.get("has_lyrics")),
                stream_url: stream_url(entry.get("file")),
            });
        }
    }
}

/// Port of `_stream_url`: pick the playable URL out of a `trackinfo` entry's `file` map.
///
/// Keyed by encoding (`mp3-128` today). Preferring the known key over "whatever is first"
/// means a future added format cannot silently downgrade playback to something the browser
/// can't decode. Protocol-relative URLs get `https:`.
pub fn stream_url(file_map: Option<&Value>) -> Option<String> {
    let Some(Value::Object(map)) = file_map else { return None };
    for key in ["mp3-128", "mp3-v0"] {
        if let Some(Value::String(v)) = map.get(key) {
            if !v.is_empty() {
                return Some(if v.starts_with("//") { format!("https:{v}") } else { v.clone() });
            }
        }
    }
    None
}

/// Port of `artist_parts`: the individually credited artists in a byline, case-folded.
///
/// Bandcamp lists the *artist* as publisher when they self-publish, so a bare
/// "publisher != artist" check invents a label. Comparing against the folded parts also
/// rejects a publisher that is merely one credited artist of several ("Kode9" against
/// "kode9, burial").
pub fn artist_parts(artist_name: &str) -> std::collections::HashSet<String> {
    use regex::Regex;
    use std::sync::OnceLock;
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r",|&|\bfeat\.?\b|\bwith\b").expect("static regex"));
    re.split(artist_name)
        .map(|p| p.trim().to_lowercase())
        .filter(|p| !p.is_empty())
        .collect()
}

fn from_jsonld(release: &mut HarvestedRelease, ld: &Map<String, Value>) {
    if release.title.is_empty() {
        release.title = str_or_empty(ld.get("name"));
    }

    if let Some(Value::Object(by)) = ld.get("byArtist") {
        if release.artist_name.is_empty() {
            release.artist_name = str_or_empty(by.get("name"));
        }
    }

    if let Some(Value::Object(publisher)) = ld.get("publisher") {
        let name = str_or_empty(publisher.get("name")).trim().to_string();
        if !name.is_empty() && !artist_parts(&release.artist_name).contains(&name.to_lowercase()) {
            release.label_name = Some(name);
        }
    }

    if release.release_date.is_none() {
        release.release_date = match ld.get("datePublished") {
            Some(v) if truthy(Some(v)) => parse_bc_date(Some(&pystr(v))),
            _ => None,
        };
    }

    if let Some(Value::Array(keywords)) = ld.get("keywords") {
        if release.tags.is_empty() {
            release.tags = keywords
                .iter()
                .map(|k| pystr(k).trim().to_string())
                .filter(|k| !k.is_empty())
                .collect();
        }
    }

    if release.about.as_deref().is_none_or(str::is_empty) && truthy(ld.get("description")) {
        release.about = Some(str_or_empty(ld.get("description")));
    }
}

/// Port of `_from_css`: the gen-2 selectors, as the floor of the ladder.
fn from_css(release: &mut HarvestedRelease, doc: &Html) {
    if release.title.is_empty() {
        if let Some(node) = css_first(doc, "h2.trackTitle").or_else(|| css_first(doc, "title")) {
            release.title = text_strip(node, true).replace(" | ", " - ");
        }
    }

    if release.artist_name.is_empty() {
        if let Some(node) =
            css_first(doc, "span[itemprop=byArtist]").or_else(|| css_first(doc, "p#band-name-location"))
        {
            let target = node_first(node, "a").unwrap_or(node);
            release.artist_name = text_strip(target, true);
        }
    }

    if release.tags.is_empty() {
        // a.tag is more reliable than the div.tralbum-tags container, which is themeable and
        // absent on some layouts.
        release.tags =
            css_all(doc, "a.tag").into_iter().map(|n| text_strip(n, true)).filter(|t| !t.is_empty()).collect();
    }

    if release.art_url.as_deref().is_none_or(str::is_empty) {
        if let Some(node) = css_first(doc, "div#tralbumArt img").or_else(|| css_first(doc, "a.popupImage img")) {
            if let Some(src) = attr(node, "src").filter(|s| !s.is_empty()) {
                release.art_url = Some(src.to_string());
            }
        }
    }
}

/// Port of `parse_track_album`: the album a `/track/` page belongs to, or `None` if it stands alone.
///
/// Needed because "download the whole album" is meaningless for a URL naming one track. The
/// blob carries `album_url` (site-relative) whenever the track is part of a release and omits
/// it for a standalone single -- a real answer, not a failure. The CSS floor is
/// `h3.albumTitle`; deliberately narrow, since a track page also lists the artist's whole
/// discography in a sidebar and taking any `/album/` href would misattribute the track.
pub fn parse_track_album(html_text: &str, url: &str) -> Option<String> {
    let doc = Html::parse_document(html_text);
    let base = artist_root(url);

    if let Some(Value::Object(tr)) = attr_json(&doc, "script[data-tralbum]", "data-tralbum") {
        if str_or_empty(tr.get("item_type")) == "album" {
            return Some(normalise(url));
        }
        if let Some(Value::String(album_url)) = tr.get("album_url") {
            if !album_url.trim().is_empty() {
                return Some(normalise(&if album_url.contains("://") {
                    album_url.clone()
                } else {
                    format!("{base}{album_url}")
                }));
            }
        }
    }

    let node = css_first(&doc, "h3.albumTitle a[href]")?;
    let href = attr(node, "href")?;
    if !href.is_empty() && href.contains("/album/") {
        return Some(normalise(&if href.contains("://") { href.to_string() } else { format!("{base}{href}") }));
    }
    None
}
