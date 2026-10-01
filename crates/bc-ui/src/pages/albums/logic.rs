//! Pure logic of the library pages (DOM-free, unit-tested natively): the "added"
//! filter (port of `AddedFilter.tsx`), release sort specs (`ReleaseSort.tsx`), the
//! grid place maths, shelf ordering and tag helpers.
use bc_types::library::{ReleaseOut, ReleaseSort, SortDir};

// ---- dates --------------------------------------------------------------------------

const HOUR: f64 = 3_600_000.0;
const DAY: f64 = 24.0 * HOUR;

/// Days since 1970-01-01 of a civil date (Howard Hinnant's algorithm).
pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

pub fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `2026-07-28T07:29:28.000Z` for a unix-ms timestamp.
pub fn iso_from_ms(ms: f64) -> String {
    let total = ms.floor() as i64;
    let days = total.div_euclid(86_400_000);
    let rem = total.rem_euclid(86_400_000);
    let (y, m, d) = civil_from_days(days);
    let (h, mi, s, milli) = (rem / 3_600_000, rem / 60_000 % 60, rem / 1000 % 60, rem % 1000);
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{milli:03}Z")
}

/// `2026-07-28T07:29:28.210257Z` (or a bare day) -> unix ms.
pub fn parse_iso_ms(s: &str) -> Option<f64> {
    let day = parse_day(s.get(0..10)?)?;
    let mut ms = days_from_civil(day.0, day.1, day.2) as f64 * DAY;
    if let Some(t) = s.get(11..) {
        let t = t.trim_end_matches('Z');
        let t = t.split(['+', '.']).next().unwrap_or(t);
        let mut it = t.split(':');
        let (h, m, sec) = (it.next()?.parse::<f64>().ok()?, it.next().and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0), it.next().and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0));
        ms += h * HOUR + m * 60_000.0 + sec * 1000.0;
    }
    Some(ms)
}

/// `28 Jul` (with the year when it is not `this_year`) for an ISO timestamp.
pub fn short_date(iso: &str, this_year: i64) -> String {
    match iso.get(0..10).and_then(parse_day) {
        Some(d) => short_day(d, this_year),
        None => String::new(),
    }
}

/// `YYYY-MM-DD` -> (y, m, d) when it is a real calendar day.
pub fn parse_day(s: &str) -> Option<(i64, i64, i64)> {
    let b = s.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let y: i64 = s[0..4].parse().ok()?;
    let m: i64 = s[5..7].parse().ok()?;
    let d: i64 = s[8..10].parse().ok()?;
    if !(1..=12).contains(&m) || d < 1 {
        return None;
    }
    let (y2, m2, d2) = civil_from_days(days_from_civil(y, m, d));
    ((y2, m2, d2) == (y, m, d)).then_some((y, m, d))
}

/// Unix ms of local midnight of a day. `tz_offset_min` = `Date.getTimezoneOffset()`.
pub fn start_of_day_ms(day: (i64, i64, i64), tz_offset_min: f64) -> f64 {
    days_from_civil(day.0, day.1, day.2) as f64 * DAY + tz_offset_min * 60_000.0
}

pub struct Preset {
    pub value: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    pub ms: f64,
}

pub const PRESETS: [Preset; 4] = [
    Preset { value: "1h", label: "Last hour", description: "in the last hour", ms: HOUR },
    Preset { value: "24h", label: "Last 24 hours", description: "in the last 24 hours", ms: DAY },
    Preset { value: "7d", label: "Last 7 days", description: "in the last 7 days", ms: 7.0 * DAY },
    Preset { value: "30d", label: "Last 30 days", description: "in the last 30 days", ms: 30.0 * DAY },
];

#[derive(Debug, Clone, PartialEq, Default)]
pub struct AddedBounds {
    pub after: Option<String>,
    pub before: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AddedResolved {
    pub bounds: AddedBounds,
    pub label: String,
    pub description: String,
    pub from: String,
    pub to: String,
}

fn short_day(d: (i64, i64, i64), this_year: i64) -> String {
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let base = format!("{} {}", d.2, MONTHS[(d.1 - 1) as usize]);
    if d.0 == this_year { base } else { format!("{base} {}", d.0) }
}

fn day_string(d: (i64, i64, i64)) -> String {
    format!("{:04}-{:02}-{:02}", d.0, d.1, d.2)
}

/// Resolve the URL spelling (`24h`, `2026-07-01..2026-07-09`, `..2026-07-09`) into
/// bounds. `now_ms` anchors the relative presets.
pub fn resolve_added(value: &str, now_ms: f64, tz_offset_min: f64) -> Option<AddedResolved> {
    if let Some(p) = PRESETS.iter().find(|p| p.value == value) {
        return Some(AddedResolved {
            bounds: AddedBounds { after: Some(iso_from_ms(now_ms - p.ms)), before: None },
            label: p.label.into(),
            description: p.description.into(),
            from: String::new(),
            to: String::new(),
        });
    }
    let at = value.find("..")?;
    let (from_s, to_s) = (&value[..at], &value[at + 2..]);
    let mut from = if from_s.is_empty() { None } else { Some(parse_day(from_s)?) };
    let mut to = if to_s.is_empty() { None } else { Some(parse_day(to_s)?) };
    if from.is_none() && to.is_none() {
        return None;
    }
    if let (Some(a), Some(b)) = (from, to) {
        if a > b {
            from = Some(b);
            to = Some(a);
        }
    }
    let (year, _, _) = civil_from_days((now_ms / DAY).floor() as i64);
    let bounds = AddedBounds {
        after: from.map(|d| iso_from_ms(start_of_day_ms(d, tz_offset_min))),
        before: to.map(|d| iso_from_ms(start_of_day_ms(d, tz_offset_min) + DAY)),
    };
    let (f, t) = (from.map(day_string).unwrap_or_default(), to.map(day_string).unwrap_or_default());
    let (label, description) = match (from, to) {
        (Some(a), Some(b)) => {
            let (x, y) = (short_day(a, year), short_day(b, year));
            if a == b { (x.clone(), format!("on {x}")) } else { (format!("{x} – {y}"), format!("between {x} and {y}")) }
        }
        (Some(a), None) => {
            let x = short_day(a, year);
            (format!("Since {x}"), format!("since {x}"))
        }
        (None, Some(b)) => {
            let y = short_day(b, year);
            (format!("Until {y}"), format!("up to {y}"))
        }
        (None, None) => return None,
    };
    Some(AddedResolved { bounds, label, description, from: f, to: t })
}

// ---- release sort ----------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SortSpec {
    pub value: &'static str,
    pub label: &'static str,
    /// The direction this field opens in.
    pub default_order: SortDir,
    pub alpha: bool,
}

pub const RELEASE_SORTS: [SortSpec; 4] = [
    SortSpec { value: "added", label: "Date added", default_order: SortDir::Desc, alpha: false },
    SortSpec { value: "year", label: "Release year", default_order: SortDir::Desc, alpha: false },
    SortSpec { value: "title", label: "Title", default_order: SortDir::Asc, alpha: true },
    SortSpec { value: "artist", label: "Artist", default_order: SortDir::Asc, alpha: true },
];

pub fn sort_spec(value: &str) -> SortSpec {
    RELEASE_SORTS.iter().copied().find(|s| s.value == value).unwrap_or(RELEASE_SORTS[0])
}

pub fn describe_order(spec: SortSpec, order: SortDir) -> &'static str {
    match (spec.alpha, order) {
        (true, SortDir::Asc) => "A–Z",
        (true, SortDir::Desc) => "Z–A",
        (false, SortDir::Desc) => "Newest first",
        (false, SortDir::Asc) => "Oldest first",
    }
}

pub fn parse_order(s: Option<&str>) -> Option<SortDir> {
    match s {
        Some("asc") => Some(SortDir::Asc),
        Some("desc") => Some(SortDir::Desc),
        _ => None,
    }
}

pub fn order_name(o: SortDir) -> &'static str {
    match o {
        SortDir::Asc => "asc",
        SortDir::Desc => "desc",
    }
}

pub fn release_sort_of(value: &str) -> ReleaseSort {
    match value {
        "title" => ReleaseSort::Title,
        "year" => ReleaseSort::Year,
        "artist" => ReleaseSort::Artist,
        "random" => ReleaseSort::Random,
        _ => ReleaseSort::Added,
    }
}

/// `sort=`/`order=` URL words -> the effective pair and whether it differs from the default view.
pub fn effective_sort(sort: Option<&str>, order: Option<&str>) -> (SortSpec, SortDir, bool) {
    let spec = sort.map(sort_spec).unwrap_or(RELEASE_SORTS[0]);
    let ord = parse_order(order).unwrap_or(spec.default_order);
    let active = spec.value != "added" || ord != spec.default_order;
    (spec, ord, active)
}

// ---- releases ------------------------------------------------------------------------------

/// Tracks the record is short of (Bandcamp's count or the files' own numbering).
pub fn missing_tracks(r: &ReleaseOut) -> i64 {
    r.expected_track_count.map(|e| (e - r.track_count).max(0)).unwrap_or(0)
}

/// Tracks a fill can actually fetch: the gap minus what Bandcamp has not released yet. Zero hides
/// every "Fill" affordance.
pub fn fillable_tracks(r: &ReleaseOut) -> i64 {
    r.fillable_missing
}

/// Is the shortfall a problem worth the warning colour? Not when every missing track is simply
/// not out yet.
pub fn shortfall_is_warning(r: &ReleaseOut) -> bool {
    missing_tracks(r) > 0 && !(r.unreleased_count > 0 && r.fillable_missing == 0)
}

const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// `2026-10-09` -> `9 Oct 2026` (or `9 Oct` without the year). `None` for anything else.
pub fn format_release_date(iso: &str, with_year: bool) -> Option<String> {
    let mut it = iso.get(..10)?.split('-');
    let (y, m, d): (i64, usize, i64) = (it.next()?.parse().ok()?, it.next()?.parse().ok()?, it.next()?.parse().ok()?);
    let month = MONTHS.get(m.checked_sub(1)?)?;
    Some(if with_year { format!("{d} {month} {y}") } else { format!("{d} {month}") })
}

/// "Pre-order · 3 tracks out 9 Oct 2026": the calm header pill. `None` unless a pre-order with
/// tracks still to come.
pub fn preorder_label(r: &ReleaseOut) -> Option<String> {
    if !r.is_preorder {
        return None;
    }
    let n = r.unreleased_count;
    let when = r.release_date.as_deref().and_then(|d| format_release_date(d, true));
    Some(match (n > 0, when) {
        (true, Some(w)) => format!("Pre-order \u{b7} {n} track{} out {w}", if n == 1 { "" } else { "s" }),
        (true, None) => format!("Pre-order \u{b7} {n} track{} not out yet", if n == 1 { "" } else { "s" }),
        (false, Some(w)) => format!("Pre-order \u{b7} out {w}"),
        (false, None) => "Pre-order".to_string(),
    })
}

/// "Out 9 Oct" on an unreleased track row.
pub fn out_badge(release_date: Option<&str>) -> String {
    match release_date.and_then(|d| format_release_date(d, false)) {
        Some(d) => format!("Out {d}"),
        None => "Not out yet".to_string(),
    }
}

/// Re-deal tracks into shelf order: `rank` maps release id -> shelf position; stable.
pub fn order_by_release_rank<T: Clone>(items: &[T], release_of: impl Fn(&T) -> Option<i64>, order: &[i64]) -> Vec<T> {
    let rank = |t: &T| release_of(t).and_then(|id| order.iter().position(|o| *o == id)).unwrap_or(0);
    let mut v = items.to_vec();
    v.sort_by_key(|t| rank(t));
    v
}

// ---- grid place --------------------------------------------------------------------------------

/// Scroll offset that puts `index` back where it stood: `offset` is the card's top
/// edge relative to the scroller's top when saved.
pub fn scroll_for_place(index: u64, offset: f64, cols: usize, row_h: f64) -> f64 {
    let row = (index as usize / cols.max(1)) as f64;
    (row * row_h - offset).max(0.0)
}

/// The place for a scroll position: index of the first card of the top visible row
/// and its top edge relative to the scroller.
pub fn place_of(scroll_top: f64, cols: usize, row_h: f64) -> (u64, f64) {
    let row = (scroll_top / row_h).floor().max(0.0);
    ((row as u64) * cols.max(1) as u64, row * row_h - scroll_top)
}

/// Cards show a medium cover, not the full-size art the DTO carries.
pub fn art_size(url: &str, size: &str) -> String {
    url.replace("size=full", &format!("size={size}"))
}

// ---- tags ---------------------------------------------------------------------------------------

/// Log-scaled bar length (percent) of a tag count between the min and max.
pub fn bar_width(count: i64, min: i64, max: i64) -> f64 {
    if max <= min || count <= 0 || min <= 0 {
        return 100.0;
    }
    let t = ((count as f64).ln() - (min as f64).ln()) / ((max as f64).ln() - (min as f64).ln());
    4.0 + t.clamp(0.0, 1.0) * 96.0
}

/// FNV-1a hue of a tag, the same everywhere a tag is coloured.
pub fn tag_hue(tag: &str) -> u32 {
    let norm = tag.trim().to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ");
    let mut h: u32 = 0x811c_9dc5;
    for ch in norm.chars() {
        h ^= ch as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    ((h as f64 * 137.508) % 360.0).round() as u32
}

/// Font-size step (0..=4) of a tag in the cloud, log scaled.
pub fn cloud_step(count: i64, min: i64, max: i64) -> usize {
    if max <= min || min <= 0 {
        return 2;
    }
    let t = ((count.max(1) as f64).ln() - (min as f64).ln()) / ((max as f64).ln() - (min as f64).ln());
    (t.clamp(0.0, 1.0) * 4.0).round() as usize
}

pub fn tracks_href_for_tag(tag: &str) -> String {
    format!("/tracks?tag={}", crate::util::enc(tag))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: f64 = 1_785_000_000_000.0;

    #[test]
    fn civil_roundtrip_and_iso() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(civil_from_days(days_from_civil(2026, 7, 28)), (2026, 7, 28));
        assert_eq!(iso_from_ms(0.0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso_from_ms(86_400_000.0 * 365.0 + 3_661_001.0), "1971-01-01T01:01:01.001Z");
    }

    #[test]
    fn parses_timestamps() {
        assert_eq!(parse_iso_ms("1970-01-02T00:00:01.5Z"), Some(86_401_000.0));
        assert_eq!(parse_iso_ms("1970-01-01"), Some(0.0));
        assert_eq!(parse_iso_ms("junk"), None);
        assert_eq!(short_date("2026-07-28T07:29:28.210257Z", 2026), "28 Jul");
        assert_eq!(short_date("2025-07-28T07:29:28Z", 2026), "28 Jul 2025");
    }

    #[test]
    fn parses_only_real_days() {
        assert_eq!(parse_day("2026-02-28"), Some((2026, 2, 28)));
        assert_eq!(parse_day("2026-02-30"), None);
        assert_eq!(parse_day("2026-13-01"), None);
        assert_eq!(parse_day("26-02-01"), None);
    }

    #[test]
    fn presets_resolve_relative_to_now() {
        let r = resolve_added("24h", NOW, 0.0).unwrap();
        assert_eq!(r.label, "Last 24 hours");
        assert_eq!(r.bounds.after.unwrap(), iso_from_ms(NOW - DAY));
        assert!(r.bounds.before.is_none());
    }

    #[test]
    fn ranges_are_whole_days_and_either_end_may_be_open() {
        let r = resolve_added("2026-07-01..2026-07-03", NOW, 0.0).unwrap();
        assert_eq!(r.bounds.after.as_deref(), Some("2026-07-01T00:00:00.000Z"));
        // the end day is included: the window closes at the next midnight
        assert_eq!(r.bounds.before.as_deref(), Some("2026-07-04T00:00:00.000Z"));
        assert_eq!((r.from.as_str(), r.to.as_str()), ("2026-07-01", "2026-07-03"));
        let open = resolve_added("2026-07-01..", NOW, 0.0).unwrap();
        assert!(open.bounds.before.is_none());
        assert!(open.label.starts_with("Since"));
        let until = resolve_added("..2026-07-01", NOW, 0.0).unwrap();
        assert!(until.bounds.after.is_none());
    }

    #[test]
    fn reversed_range_is_swapped_and_garbage_is_none() {
        let r = resolve_added("2026-07-09..2026-07-01", NOW, 0.0).unwrap();
        assert_eq!(r.from, "2026-07-01");
        assert!(resolve_added("..", NOW, 0.0).is_none());
        assert!(resolve_added("nonsense", NOW, 0.0).is_none());
        assert!(resolve_added("2026-02-30..", NOW, 0.0).is_none());
    }

    #[test]
    fn local_midnight_follows_the_timezone() {
        // UTC+2 reports -120
        let r = resolve_added("2026-07-01..2026-07-01", NOW, -120.0).unwrap();
        assert_eq!(r.bounds.after.as_deref(), Some("2026-06-30T22:00:00.000Z"));
        assert_eq!(r.label, "1 Jul");
    }

    #[test]
    fn sort_defaults_and_describe() {
        let (s, o, active) = effective_sort(None, None);
        assert_eq!((s.value, o, active), ("added", SortDir::Desc, false));
        let (s, o, active) = effective_sort(Some("title"), None);
        assert_eq!((s.value, o, active), ("title", SortDir::Asc, true));
        assert_eq!(describe_order(s, o), "A–Z");
        let (s, o, active) = effective_sort(Some("added"), Some("asc"));
        assert!(active);
        assert_eq!(describe_order(s, o), "Oldest first");
        assert_eq!(sort_spec("bogus").value, "added");
    }

    #[test]
    fn missing_counts_only_when_a_total_is_known() {
        let mut r = ReleaseOut { track_count: 4, expected_track_count: Some(12), ..Default::default() };
        assert_eq!(missing_tracks(&r), 8);
        r.expected_track_count = None;
        assert_eq!(missing_tracks(&r), 0);
        r.expected_track_count = Some(3);
        assert_eq!(missing_tracks(&r), 0);
    }

    #[test]
    fn shelf_order_is_stable_inside_a_release() {
        let tracks = vec![(1, 20), (2, 10), (3, 20), (4, 10)];
        let out = order_by_release_rank(&tracks, |t| Some(t.1), &[10, 20]);
        assert_eq!(out.iter().map(|t| t.0).collect::<Vec<_>>(), vec![2, 4, 1, 3]);
    }

    #[test]
    fn place_roundtrip() {
        let (idx, off) = place_of(1010.0, 5, 250.0);
        assert_eq!(idx, 20);
        assert!((off - (-10.0)).abs() < 1e-9);
        assert!((scroll_for_place(idx, off, 5, 250.0) - 1010.0).abs() < 1e-9);
        // fewer columns after a resize: the same card, new row
        assert_eq!(scroll_for_place(20, 0.0, 4, 250.0), 1250.0);
        assert_eq!(scroll_for_place(0, 40.0, 4, 250.0), 0.0);
    }

    #[test]
    fn art_urls_are_resized() {
        assert_eq!(art_size("/api/art/release/1?size=full&v=ab", "medium"), "/api/art/release/1?size=medium&v=ab");
        assert_eq!(art_size("/x.jpg", "medium"), "/x.jpg");
    }

    #[test]
    fn bars_and_hues() {
        assert_eq!(bar_width(5, 5, 5), 100.0);
        assert!((bar_width(1, 1, 1000) - 4.0).abs() < 1e-9);
        assert!((bar_width(1000, 1, 1000) - 100.0).abs() < 1e-9);
        assert_eq!(tag_hue("Dub  Techno"), tag_hue("dub techno"));
        assert_ne!(tag_hue("techno"), tag_hue("ambient"));
        assert_eq!(cloud_step(1, 1, 1000), 0);
        assert_eq!(cloud_step(1000, 1, 1000), 4);
    }

    #[test]
    fn preorder_wording() {
        assert_eq!(format_release_date("2026-10-09", true).as_deref(), Some("9 Oct 2026"));
        assert_eq!(format_release_date("2026-10-09", false).as_deref(), Some("9 Oct"));
        assert_eq!(format_release_date("nonsense", true), None);
        assert_eq!(format_release_date("2026-13-01", true), None);
        let mut r = ReleaseOut { track_count: 1, expected_track_count: Some(4), is_preorder: true, unreleased_count: 3, release_date: Some("2026-10-09".into()), ..Default::default() };
        assert_eq!(preorder_label(&r).as_deref(), Some("Pre-order \u{b7} 3 tracks out 9 Oct 2026"));
        assert_eq!(out_badge(r.release_date.as_deref()), "Out 9 Oct");
        assert!(!shortfall_is_warning(&r), "unreleased is calm, not a warning");
        r.fillable_missing = 1;
        r.unreleased_count = 2;
        assert!(shortfall_is_warning(&r));
        r.is_preorder = false;
        assert_eq!(preorder_label(&r), None);
        assert_eq!(out_badge(None), "Not out yet");
    }
}
