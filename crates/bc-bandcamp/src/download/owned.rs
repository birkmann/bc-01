//! Downloads in the quality you bought (FLAC, MP3 320, ...): the files a purchase's download page
//! offers, instead of the public 128 kbps stream.
//!
//! Bandcamp hands them out in the same steps its download page runs in the browser:
//! 1. The owner's collection lists a signed *redownload* link per purchase
//!    (`collection_items` -> `redownload_urls`, keyed `<sale_item_type><sale_item_id>`).
//! 2. That page's `pagedata` blob lists the formats on offer
//!    (`digital_items[0].downloads` = `{"flac": {"url": ..., "size_mb": ...}, ...}`).
//! 3. The format URL is prepared through `/statdownload/`, which answers (wrapped in a JS
//!    callback) with the CDN `download_url`; when it does not, the format URL itself is fetched.
//! 4. An album arrives as a zip of tagged tracks (plus cover and extras), a track as one file.
//!
//! Each track is unpacked to `<final>.part`, synced and renamed into the layout the stream
//! downloader uses (only the extension differs), so dedup and completeness see the same album
//! either way. Bandcamp's own tags and embedded art are kept as they are; extras (cover.jpg,
//! PDFs) are left out, like the stream downloader leaves no cover file.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use bc_db::Db;
use scraper::Html;
use serde_json::{Value, json};

use super::slug;
use crate::error::HarvestError;
use crate::extract::{HarvestedRelease, attr_json};
use crate::net::{BandcampClient, GetOpts, PageKind};
use crate::{sources, urls};

/// Setting holding the preferred format key (see [`FORMATS`]); absent or `stream` = the stream.
pub const FORMAT_KEY: &str = "downloads.format";

/// The formats Bandcamp sells, best first: (key, label, file extension).
pub const FORMATS: [(&str, &str, &str); 8] = [
    ("flac", "FLAC", "flac"),
    ("alac", "ALAC", "m4a"),
    ("wav", "WAV", "wav"),
    ("aiff-lossless", "AIFF", "aiff"),
    ("mp3-320", "MP3 320", "mp3"),
    ("mp3-v0", "MP3 V0", "mp3"),
    ("aac-hi", "AAC", "m4a"),
    ("vorbis", "Ogg Vorbis", "ogg"),
];

/// How long the purchase index is trusted; a miss re-reads one older than [`RECHECK`] (a
/// purchase made a minute ago is not in an index read an hour ago).
const FRESH: Duration = Duration::from_secs(30 * 60);
const RECHECK: Duration = Duration::from_secs(60);
const PAGE: usize = 500;
const MAX_PAGES: usize = 200;

/// The format key for a stored or submitted value; `None` = the public stream.
pub fn parse_format(raw: &str) -> Option<&'static str> {
    let v = raw.trim().trim_matches('"').to_ascii_lowercase();
    FORMATS.iter().map(|f| f.0).find(|k| *k == v)
}

pub fn read_format(db: &Db) -> Option<&'static str> {
    db.read(|c| bc_db::settings::get(c, FORMAT_KEY)).ok().flatten().as_deref().and_then(parse_format)
}

pub async fn write_format_async(db: &Db, format: Option<&'static str>) -> bc_db::Result<()> {
    db.write_async(move |t| bc_db::settings::set(t, FORMAT_KEY, format.unwrap_or("stream"))).await
}

pub fn extension_of(format: &str) -> &'static str {
    FORMATS.iter().find(|f| f.0 == format).map(|f| f.2).unwrap_or("bin")
}

pub fn label_of(format: &str) -> &'static str {
    FORMATS.iter().find(|f| f.0 == format).map(|f| f.1).unwrap_or("?")
}

// -- the purchase index ----------------------------------------------------------------------

#[derive(Default)]
struct Index {
    /// Normalised item URL -> redownload link.
    links: HashMap<String, String>,
    at: Option<Instant>,
}

/// The cookie owner's purchases, read from their collection (and hidden items) on demand.
#[derive(Default)]
pub struct Purchases(tokio::sync::Mutex<Index>);

impl Purchases {
    /// The redownload link for the release at `url`, `None` when it is not a purchase.
    pub async fn link(&self, client: &BandcampClient, url: &str) -> Result<Option<String>, HarvestError> {
        let key = urls::normalise(url);
        // Held across the reload so concurrent items wait for one read instead of each paging.
        let mut idx = self.0.lock().await;
        let older = |idx: &Index, age: Duration| idx.at.is_none_or(|t| t.elapsed() > age);
        if older(&idx, FRESH) {
            *idx = load(client).await?;
        }
        if let Some(l) = idx.links.get(&key) {
            return Ok(Some(l.clone()));
        }
        if older(&idx, RECHECK) {
            *idx = load(client).await?;
        }
        Ok(idx.links.get(&key).cloned())
    }
}

async fn load(client: &BandcampClient) -> Result<Index, HarvestError> {
    let me = sources::whoami(client).await?;
    let fan_id = me.fan_id.ok_or_else(|| HarvestError::IdentityExpired("Bandcamp did not say whose cookie this is".into()))?;
    let mut links = HashMap::new();
    for path in [sources::COLLECTION_PATH, sources::HIDDEN_PATH] {
        let mut token = sources::newest_token();
        for _ in 0..MAX_PAGES {
            let payload = json!({ "fan_id": fan_id, "older_than_token": token, "count": PAGE });
            let data = match client.post_api(path, &payload, true, Some("https://bandcamp.com/")).await {
                Ok(d) => d,
                // Bandcamp answers a list it will not show with a bare error; hidden items are optional.
                Err(HarvestError::Api { unspecified: true, .. }) if path == sources::HIDDEN_PATH => break,
                Err(e) => return Err(e),
            };
            let n = collect_links(&data, &mut links);
            let next = data.get("last_token").and_then(Value::as_str).unwrap_or_default().to_string();
            let more = data.get("more_available").and_then(Value::as_bool).unwrap_or(false);
            if n == 0 || !more || next.is_empty() || next == token {
                break;
            }
            token = next;
        }
    }
    tracing::info!("owned downloads: {} purchases with a download link", links.len());
    Ok(Index { links, at: Some(Instant::now()) })
}

fn id_text(v: Option<&Value>) -> String {
    match v {
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    }
}

/// Pair each item of one `collection_items` page with its redownload link (into `out`, keyed by
/// the normalised item URL). Returns how many items the page held.
pub fn collect_links(data: &Value, out: &mut HashMap<String, String>) -> usize {
    let links = data.get("redownload_urls").and_then(Value::as_object);
    let items = data.get("items").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
    for it in items {
        let key = format!("{}{}", id_text(it.get("sale_item_type")), id_text(it.get("sale_item_id")));
        let Some(link) = links.and_then(|l| l.get(&key)).and_then(Value::as_str) else { continue };
        let Some(url) = sources::release_from_collection_item(it).map(|r| r.url) else { continue };
        out.insert(urls::normalise(&url), link.to_string());
    }
    items.len()
}

// -- the download page -----------------------------------------------------------------------

/// One format a purchase's download page offers.
#[derive(Debug, Clone, PartialEq)]
pub struct Offer {
    pub format: String,
    pub url: String,
}

/// The formats on a download page (`#pagedata` blob, `digital_items` or `download_items`).
pub fn parse_download_page(html: &str) -> Result<Vec<Offer>, HarvestError> {
    let doc = Html::parse_document(html);
    let blob = ["#pagedata[data-blob]", "div#pagedata", "[data-blob]"]
        .iter()
        .find_map(|sel| attr_json(&doc, sel, "data-blob").filter(Value::is_object))
        .ok_or_else(|| HarvestError::Extraction("download page carries no pagedata blob".into()))?;
    let item = ["digital_items", "download_items"]
        .iter()
        .find_map(|k| blob.get(*k).and_then(Value::as_array).and_then(|a| a.first()))
        .ok_or_else(|| HarvestError::Extraction("download page lists no purchase".into()))?;
    let downloads = item.get("downloads").and_then(Value::as_object).filter(|d| !d.is_empty()).ok_or_else(|| {
        HarvestError::Extraction("Bandcamp offers no files for this purchase yet (a pre-order not out yet?)".into())
    })?;
    Ok(downloads
        .iter()
        .filter_map(|(k, v)| {
            let url = v.get("url").and_then(Value::as_str).filter(|u| !u.is_empty())?;
            Some(Offer { format: k.clone(), url: url.to_string() })
        })
        .collect())
}

/// The offer in `want`, else the best format on offer.
pub fn pick_offer<'a>(offers: &'a [Offer], want: &str) -> Option<&'a Offer> {
    offers
        .iter()
        .find(|o| o.format == want)
        .or_else(|| FORMATS.iter().find_map(|f| offers.iter().find(|o| o.format == f.0)))
}

/// The `/statdownload/` twin of a format URL (`/download/album?enc=flac&...`).
pub fn stat_url(format_url: &str, rand: u32) -> Option<String> {
    let mut u = url::Url::parse(format_url).ok()?;
    let path = u.path().strip_prefix("/download/")?.to_string();
    u.set_path(&format!("/statdownload/{path}"));
    u.query_pairs_mut().append_pair(".vrs", "1").append_pair(".rand", &rand.to_string());
    Some(u.to_string())
}

/// The CDN URL in a statdownload answer (plain JSON or wrapped in `Downloads.statResult(...)`).
pub fn parse_stat(body: &str) -> Option<String> {
    // The first `{` that starts a JSON object with a `result`: the JS wrapper has braces of its own.
    let v = body.match_indices('{').find_map(|(i, _)| {
        let v = serde_json::Deserializer::from_str(&body[i..]).into_iter::<Value>().next()?.ok()?;
        v.get("result").is_some().then_some(v)
    })?;
    if v.get("result").and_then(Value::as_str) != Some("ok") {
        return None;
    }
    v.get("download_url").and_then(Value::as_str).filter(|u| !u.is_empty()).map(str::to_string)
}

/// Where a purchase's file comes from: the redownload page -> format -> CDN URL.
pub struct Resolved {
    pub format: String,
    pub url: String,
}

/// Resolve the file URL for a purchase's redownload `link` in `want` (or the best offered).
pub async fn resolve(client: &BandcampClient, link: &str, want: &str) -> Result<Resolved, HarvestError> {
    let page = client.get_html(link, GetOpts::kind(PageKind::Discover).authed(true)).await?;
    let offers = parse_download_page(&page)?;
    let offer = pick_offer(&offers, want).ok_or_else(|| HarvestError::Extraction("the download page offers no known format".into()))?;
    let mut url = offer.url.clone();
    if let Some(stat) = stat_url(&offer.url, rand::random()) {
        // Large formats are prepared on demand; ask a few times before taking the plain URL.
        for attempt in 0..3 {
            match client.get_html(&stat, GetOpts::kind(PageKind::Discover).authed(true)).await {
                Ok(body) => {
                    if let Some(u) = parse_stat(&body) {
                        url = u;
                        break;
                    }
                }
                Err(e) => tracing::debug!("statdownload failed: {e}"),
            }
            if attempt < 2 {
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
    Ok(Resolved { format: offer.format.clone(), url })
}

// -- unpacking -------------------------------------------------------------------------------

/// Where the tracks of one release go: per track number the template path (without extension)
/// and the title as a match key, and the album folder for anything that names no known track.
#[derive(Debug, Clone)]
pub struct Layout {
    tracks: HashMap<u32, (PathBuf, String)>,
    album_dir: PathBuf,
    /// The only track's path, for a single file (a track purchase or a one-track release).
    single: Option<PathBuf>,
}

/// Letters and digits only, lowercased: how a title is compared with a file name.
fn match_key(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

impl Layout {
    pub fn new(release: &HarvestedRelease, template: &str, base: &Path) -> Result<Self, String> {
        let mut tracks = HashMap::new();
        let mut first = None;
        for t in &release.tracks {
            let meta = super::native::track_meta(release, t);
            let rel = slug::expand_template(template, &meta);
            let dest = bc_core::paths::safe_join(base, &rel).map_err(|e| e.to_string())?;
            first.get_or_insert_with(|| dest.clone());
            tracks.insert(t.track_num.filter(|n| *n > 0).unwrap_or(1) as u32, (dest, match_key(&meta.title)));
        }
        let album_dir = first.as_ref().and_then(|p| p.parent()).map(Path::to_path_buf).unwrap_or_else(|| base.to_path_buf());
        let single = if release.tracks.len() == 1 { first } else { None };
        Ok(Self { tracks, album_dir, single })
    }

    /// The final path of a zip entry named `name` (Bandcamp: `Artist - Album - 03 Title.flac`).
    /// A number whose following text matches that track's title wins (an album called
    /// `20 Years` must not claim track 20); else the last number naming a known track.
    fn dest_for(&self, name: &str, ext: &str) -> PathBuf {
        let stem = Path::new(name).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let found = track_numbers(&stem);
        let titled = found.iter().find(|(n, rest)| {
            self.tracks.get(n).is_some_and(|(_, title)| {
                let rest = match_key(rest);
                !title.is_empty() && !rest.is_empty() && (rest.starts_with(title.as_str()) || title.starts_with(rest.as_str()))
            })
        });
        let base = titled
            .or_else(|| found.iter().rev().find(|(n, _)| self.tracks.contains_key(n)))
            .and_then(|(n, _)| self.tracks.get(n))
            .map(|(p, _)| p.clone())
            .unwrap_or_else(|| self.album_dir.join(slug::slugify(&stem)));
        with_ext(&base, ext)
    }
}

fn with_ext(stem_path: &Path, ext: &str) -> PathBuf {
    let mut s = stem_path.as_os_str().to_os_string();
    s.push(".");
    s.push(ext);
    PathBuf::from(s)
}

/// Track-number candidates in a Bandcamp file name with the text after each: a number of at
/// most three digits at the start or right after a ` - `, followed by a space or `.`.
pub fn track_numbers(stem: &str) -> Vec<(u32, String)> {
    let starts = std::iter::once(0).chain(stem.match_indices(" - ").map(|(i, _)| i + 3));
    let mut out = Vec::new();
    for at in starts {
        let part = &stem[at..];
        let digits = part.chars().take_while(char::is_ascii_digit).count();
        if (1..=3).contains(&digits) && part[digits..].starts_with([' ', '.']) {
            if let Ok(n) = part[..digits].parse::<u32>() {
                out.push((n, part[digits..].trim_start_matches(['.', ' ']).to_string()));
            }
        }
    }
    out
}

/// What unpacking produced.
#[derive(Debug, Default)]
pub struct Unpacked {
    pub new_files: Vec<PathBuf>,
    pub present: u32,
}

/// Write `reader` to `dest` crash-safely: `<dest>.part`, fsync, rename, fsync the folder.
fn place(mut reader: impl Read, dest: &Path) -> std::io::Result<()> {
    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut part = dest.as_os_str().to_os_string();
    part.push(".part");
    let part = PathBuf::from(part);
    let res = (|| {
        let mut f = std::fs::File::create(&part)?;
        std::io::copy(&mut reader, &mut f)?;
        f.flush()?;
        f.sync_all()?;
        std::fs::rename(&part, dest)?;
        if let Some(dir) = dest.parent().and_then(|d| std::fs::File::open(d).ok()) {
            let _ = dir.sync_all();
        }
        Ok(())
    })();
    if res.is_err() {
        let _ = std::fs::remove_file(&part);
    }
    res
}

fn audio_ext(name: &str) -> Option<String> {
    let ext = Path::new(name).extension()?.to_string_lossy().to_ascii_lowercase();
    bc_core::audio::is_audio_ext(&ext).then_some(ext)
}

/// Unpack a downloaded purchase (a zip of tracks, or one audio file in `format`) into `layout`.
/// Tracks already present and readable are kept. Blocking.
pub fn unpack(archive: &Path, format: &str, layout: &Layout, complete: impl Fn(&Path) -> bool) -> std::io::Result<Unpacked> {
    let mut out = Unpacked::default();
    let mut magic = [0u8; 4];
    let is_zip = std::fs::File::open(archive)?.read(&mut magic)? == 4 && magic == *b"PK\x03\x04";
    if !is_zip {
        let ext = extension_of(format);
        let dest = match &layout.single {
            Some(p) => with_ext(p, ext),
            None => layout.tracks.get(&1).map(|(p, _)| with_ext(p, ext)).unwrap_or_else(|| layout.album_dir.join(format!("track.{ext}"))),
        };
        if complete(&dest) {
            out.present += 1;
        } else {
            place(std::fs::File::open(archive)?, &dest)?;
            out.new_files.push(dest);
        }
        return Ok(out);
    }
    let mut zip = zip::ZipArchive::new(std::fs::File::open(archive)?).map_err(std::io::Error::other)?;
    for i in 0..zip.len() {
        let entry = zip.by_index(i).map_err(std::io::Error::other)?;
        if entry.is_dir() {
            continue;
        }
        // Only the file name counts: a zip path can never steer a write outside the layout.
        let Some(name) = entry.enclosed_name().and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned())) else { continue };
        let Some(ext) = audio_ext(&name) else { continue };
        let dest = layout.dest_for(&name, &ext);
        if out.new_files.contains(&dest) || complete(&dest) {
            out.present += 1;
            continue;
        }
        place(entry, &dest)?;
        out.new_files.push(dest);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_setting_values() {
        assert_eq!(parse_format("flac"), Some("flac"));
        assert_eq!(parse_format("\"MP3-320\""), Some("mp3-320"));
        assert_eq!(parse_format("stream"), None);
        assert_eq!(parse_format(""), None);
        assert_eq!(extension_of("aac-hi"), "m4a");
    }

    #[test]
    fn links_pair_items_with_their_sale_key() {
        let page = json!({
            "items": [
                {"sale_item_type": "p", "sale_item_id": 111, "item_url": "https://a.bandcamp.com/album/one", "tralbum_type": "a", "item_title": "One", "band_name": "A"},
                {"sale_item_type": "r", "sale_item_id": 222, "item_url": "https://b.bandcamp.com/track/two?from=fanpub", "tralbum_type": "t", "item_title": "Two", "band_name": "B"},
                {"sale_item_type": "p", "sale_item_id": 333, "item_url": "https://c.bandcamp.com/album/gift", "tralbum_type": "a", "item_title": "Gift", "band_name": "C"}
            ],
            "redownload_urls": {"p111": "https://bandcamp.com/download?from=collection&payment_id=111&sig=x&sitem_id=1", "r222": "https://bandcamp.com/download?r=222"}
        });
        let mut m = HashMap::new();
        assert_eq!(collect_links(&page, &mut m), 3);
        assert_eq!(m.len(), 2);
        assert_eq!(m.get(&urls::normalise("https://a.bandcamp.com/album/one")).map(String::as_str), Some("https://bandcamp.com/download?from=collection&payment_id=111&sig=x&sitem_id=1"));
        assert!(m.contains_key(&urls::normalise("https://b.bandcamp.com/track/two")));
    }

    fn page(blob: Value) -> String {
        let esc = blob.to_string().replace('&', "&amp;").replace('"', "&quot;");
        format!(r#"<html><body><div id="pagedata" data-blob="{esc}"></div></body></html>"#)
    }

    #[test]
    fn download_page_offers_and_choice() {
        let html = page(json!({"digital_items": [{"title": "X", "downloads": {
            "mp3-320": {"size_mb": "90MB", "url": "https://popplers5.bandcamp.com/download/album?enc=mp3-320&fsig=a&id=1&ts=2"},
            "flac": {"size_mb": "300MB", "url": "https://popplers5.bandcamp.com/download/album?enc=flac&fsig=b&id=1&ts=2"}
        }}]}));
        let offers = parse_download_page(&html).unwrap();
        assert_eq!(offers.len(), 2);
        assert_eq!(pick_offer(&offers, "flac").unwrap().format, "flac");
        assert_eq!(pick_offer(&offers, "mp3-320").unwrap().format, "mp3-320");
        // not offered: the best that is
        assert_eq!(pick_offer(&offers, "wav").unwrap().format, "flac");
        let none = page(json!({"download_items": [{"title": "Pre"}]}));
        assert!(matches!(parse_download_page(&none), Err(HarvestError::Extraction(_))));
    }

    #[test]
    fn statdownload_round_trip() {
        let s = stat_url("https://popplers5.bandcamp.com/download/album?enc=flac&fsig=b&id=1&ts=2", 7).unwrap();
        assert_eq!(s, "https://popplers5.bandcamp.com/statdownload/album?enc=flac&fsig=b&id=1&ts=2&.vrs=1&.rand=7");
        assert_eq!(stat_url("https://example.com/other", 1), None);
        let wrapped = r#"if ( window.Downloads ) { Downloads.statResult ( {"result":"ok","download_url":"https://p4.bcbits.com/download/album/x/flac?token=t"} ) };"#;
        assert_eq!(parse_stat(wrapped).as_deref(), Some("https://p4.bcbits.com/download/album/x/flac?token=t"));
        assert_eq!(parse_stat(r#"{"result":"err","errortype":"ExpiredFreebieError"}"#), None);
    }

    #[test]
    fn track_numbers_from_bandcamp_names() {
        let nums = |s: &str| track_numbers(s).into_iter().map(|(n, _)| n).collect::<Vec<_>>();
        assert_eq!(nums("Artist - Album - 03 Title"), [3]);
        assert_eq!(nums("Artist - 20 Years - 03 Ti - tle"), [20, 3]);
        assert_eq!(nums("07 Title"), [7]);
        assert_eq!(nums("Artist - Album - Title"), Vec::<u32>::new());
        assert_eq!(nums("cover"), Vec::<u32>::new());
    }

    #[test]
    fn the_title_decides_between_numbers() {
        let base = Path::new("/m");
        let mut r = release(25);
        r.title = "20 Years".into();
        r.tracks[2].title = "Ti - tle".into();
        let layout = Layout::new(&r, "%{track} - %{title}", base).unwrap();
        assert_eq!(layout.dest_for("Artist - 20 Years - 03 Ti - tle.flac", "flac"), base.join("03 - ti-tle.flac"));
        assert_eq!(layout.dest_for("Artist - 20 Years - 20 Song 20.flac", "flac"), base.join("20 - song-20.flac"));
    }

    fn release(n: usize) -> HarvestedRelease {
        HarvestedRelease {
            url: "https://a.bandcamp.com/album/x".into(),
            title: "Album".into(),
            artist_name: "Artist".into(),
            tracks: (1..=n)
                .map(|i| crate::extract::HarvestedTrack { title: format!("Song {i}"), track_num: Some(i as i64), ..Default::default() })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn unpacks_a_zip_into_the_stream_layout() {
        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("dl.zip");
        {
            let mut w = zip::ZipWriter::new(std::fs::File::create(&zip_path).unwrap());
            let o = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
            for (name, body) in [
                ("Artist - Album - 01 Song 1.flac", &b"fLaC one"[..]),
                ("Artist - Album - 02 Song 2.flac", b"fLaC two"),
                ("cover.jpg", b"jpeg"),
                ("../../evil.flac", b"nope"),
            ] {
                w.start_file(name, o).unwrap();
                w.write_all(body).unwrap();
            }
            w.finish().unwrap();
        }
        let base = dir.path().join("out");
        let layout = Layout::new(&release(2), "%{artist}/%{album}/%{track} - %{title}", &base).unwrap();
        let got = unpack(&zip_path, "flac", &layout, |p| p.exists()).unwrap();
        let mut names: Vec<String> = got.new_files.iter().map(|p| p.strip_prefix(&base).unwrap().to_string_lossy().into_owned()).collect();
        names.sort();
        // a path climbing out of the zip is skipped, not written anywhere
        assert_eq!(names, ["artist/album/01 - song-1.flac", "artist/album/02 - song-2.flac"]);
        assert_eq!(std::fs::read(base.join("artist/album/01 - song-1.flac")).unwrap(), b"fLaC one");
        assert!(!dir.path().join("evil.flac").exists());
        // a second run finds everything in place
        let again = unpack(&zip_path, "flac", &layout, |p| p.exists()).unwrap();
        assert!(again.new_files.is_empty());
        assert_eq!(again.present, 2);
    }

    #[test]
    fn unpacks_a_single_file() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("dl.bin");
        std::fs::write(&f, b"ID3 mp3 bytes").unwrap();
        let base = dir.path().join("out");
        let layout = Layout::new(&release(1), "%{artist}/%{album}/%{track} - %{title}", &base).unwrap();
        let got = unpack(&f, "mp3-320", &layout, |p| p.exists()).unwrap();
        assert_eq!(got.new_files, vec![base.join("artist/album/01 - song-1.mp3")]);
    }
}
