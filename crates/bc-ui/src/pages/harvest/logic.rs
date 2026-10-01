//! Pure Harvest logic: what a resolved source offers, the notices after a run / a queue,
//! and how an inbox item reads (scope badge). Natively tested.
use bc_types::bandcamp::{HarvestItemOut, QueueResult, RunResult};

pub const FAN_TABS: [&str; 3] = ["collection", "wishlist", "hidden"];

/// The inbox states in lifecycle order, with human labels.
pub const STATE_TABS: [(&str, &str); 6] =
    [("new", "New"), ("queued", "Queued"), ("in_library", "In library"), ("downloaded", "Downloaded"), ("ignored", "Ignored"), ("all", "All")];

pub fn kind_label(kind: &str) -> &str {
    match kind {
        "url_list" => "URL list",
        "artist" => "Artist",
        "label" => "Label",
        "collection" => "Collection",
        "wishlist" => "Wishlist",
        "hidden" => "Hidden",
        "discover" => "Discover",
        other => other,
    }
}

/// What an empty tab means: each state is a station in the pipeline.
pub fn empty_text(state: &str) -> &'static str {
    match state {
        "new" => "Nothing new. Identify a source above and press Harvest to fill the inbox.",
        "queued" => "Nothing queued. Select items under New and press Queue; they wait here while the download job runs.",
        "in_library" => "Nothing here yet. Items move here once they match a release you own.",
        "downloaded" => "Nothing here. Finished downloads that could not be matched to a library release land here.",
        "ignored" => "Nothing ignored. The eye button on a card hides it from New without deleting it.",
        _ => "The inbox is empty. Identify a source above and press Harvest.",
    }
}

/// Fan pages have tabs; the resolved kind says which one the URL points at.
pub fn fan_tab(resolved_kind: &str) -> Option<String> {
    FAN_TABS.contains(&resolved_kind).then(|| resolved_kind.to_string())
}

/// The kind to run: the picked fan tab, else what was resolved, else a plain URL list.
pub fn run_kind(tab: Option<&str>, resolved_kind: Option<&str>) -> String {
    tab.or(resolved_kind).unwrap_or("url_list").to_string()
}

pub fn run_notice(r: &RunResult) -> String {
    let mut s = format!("Found {} \u{2014} {} new, {} already known, {} already in your library.", r.seen, r.new, r.already_known, r.in_library);
    if !r.errors.is_empty() {
        s.push_str(&format!(" {} error(s).", r.errors.len()));
    }
    if r.new > 0 {
        s.push_str(" Select what you want below and queue it.");
    }
    s
}

pub enum QueueOutcome {
    /// Neither owned nor free: the button turns into "Queue anyway".
    NeedsConfirmation(usize),
    Queued(String),
}

pub fn queue_outcome(r: &QueueResult) -> QueueOutcome {
    if !r.needs_confirmation.is_empty() && r.queued == 0 {
        return QueueOutcome::NeedsConfirmation(r.needs_confirmation.len());
    }
    let mut s = format!("Queued {} \u{2014} watch the job on the Downloads page.", r.queued);
    if r.skipped_in_library > 0 {
        s.push_str(&format!(" Skipped {} already in library \u{2014} tick \u{201c}include in library\u{201d} to queue those too.", r.skipped_in_library));
    }
    QueueOutcome::Queued(s)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    InLibrary,
    Owned,
    Free,
    NotOwned,
}

/// Owned or freely offered is in scope; everything else needs confirmation.
pub fn scope(i: &HarvestItemOut) -> Scope {
    if i.in_library {
        Scope::InLibrary
    } else if i.in_collection {
        Scope::Owned
    } else if i.is_free_download {
        Scope::Free
    } else {
        Scope::NotOwned
    }
}

/// The Bandcamp page hosting a release: an artist's or a label's root.
pub fn band_root(release_url: &str) -> Option<String> {
    let (scheme, rest) = release_url.split_once("://")?;
    let host = rest.split(['/', '?', '#']).next().filter(|h| !h.is_empty())?;
    Some(format!("{scheme}://{host}"))
}

/// Default folder layout: a wishlist queues into one flat folder, anything else keeps artist/album.
pub fn single_folder_default(tab_or_kind: &str) -> bool {
    tab_or_kind == "wishlist"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fan_tabs_follow_the_resolved_kind() {
        assert_eq!(fan_tab("wishlist").as_deref(), Some("wishlist"));
        assert_eq!(fan_tab("label"), None);
        assert_eq!(run_kind(Some("hidden"), Some("collection")), "hidden");
        assert_eq!(run_kind(None, Some("label")), "label");
        assert_eq!(run_kind(None, None), "url_list");
        assert!(single_folder_default("wishlist"));
        assert!(!single_folder_default("collection"));
    }

    #[test]
    fn run_notice_reads_like_the_legacy_one() {
        let r = RunResult { seen: 12, new: 5, already_known: 4, in_library: 3, errors: vec!["x".into()], ..Default::default() };
        let n = run_notice(&r);
        assert!(n.starts_with("Found 12 \u{2014} 5 new, 4 already known, 3 already in your library."));
        assert!(n.contains("1 error(s)."));
        assert!(n.ends_with("queue it."));
        let none = RunResult::default();
        assert!(!run_notice(&none).contains("queue it"));
    }

    #[test]
    fn unowned_items_ask_before_queueing() {
        let r = QueueResult { queued: 0, needs_confirmation: vec![serde_json::json!({}), serde_json::json!({})], ..Default::default() };
        assert!(matches!(queue_outcome(&r), QueueOutcome::NeedsConfirmation(2)));
        let ok = QueueResult { queued: 3, skipped_in_library: 1, ..Default::default() };
        match queue_outcome(&ok) {
            QueueOutcome::Queued(s) => {
                assert!(s.starts_with("Queued 3"));
                assert!(s.contains("Skipped 1 already in library"));
            }
            _ => panic!("expected queued"),
        }
        // partially queued: no confirmation step
        let mixed = QueueResult { queued: 2, needs_confirmation: vec![serde_json::json!({})], ..Default::default() };
        assert!(matches!(queue_outcome(&mixed), QueueOutcome::Queued(_)));
    }

    #[test]
    fn scope_badge_order() {
        let mut i = HarvestItemOut { is_purchasable: true, ..Default::default() };
        assert_eq!(scope(&i), Scope::NotOwned);
        i.is_free_download = true;
        assert_eq!(scope(&i), Scope::Free);
        i.in_collection = true;
        assert_eq!(scope(&i), Scope::Owned);
        i.in_library = true;
        assert_eq!(scope(&i), Scope::InLibrary);
    }

    #[test]
    fn band_root_is_the_origin() {
        assert_eq!(band_root("https://a.bandcamp.com/album/x").as_deref(), Some("https://a.bandcamp.com"));
        assert_eq!(band_root("nonsense"), None);
    }

    #[test]
    fn tabs_cover_the_lifecycle() {
        assert_eq!(STATE_TABS.first().map(|t| t.0), Some("new"));
        assert_eq!(STATE_TABS.last().map(|t| t.0), Some("all"));
        assert!(empty_text("queued").contains("Nothing queued"));
        assert!(empty_text("all").contains("inbox is empty"));
    }
}
