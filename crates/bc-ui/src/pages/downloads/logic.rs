//! Pure helpers of the Downloads page (tested natively).
use bc_types::jobs::*;

/// How many lines of a pasted text look like something the server will accept: an album or
/// track page, or a band page (which queues as one item the worker expands). The preview
/// request is a convenience; the server stays the authority on what is valid.
pub fn local_url_count(text: &str) -> usize {
    text.split(['\n', ',']).map(str::trim).filter(|l| looks_like_bandcamp(l)).count()
}

pub fn looks_like_bandcamp(line: &str) -> bool {
    let low = line.to_ascii_lowercase();
    let Some(rest) = low.strip_prefix("https://").or_else(|| low.strip_prefix("http://")) else { return false };
    if rest.is_empty() || rest.contains(char::is_whitespace) {
        return false;
    }
    let (host, path) = match rest.split_once('/') {
        Some((h, p)) => (h, p),
        None => (rest, ""),
    };
    if host.is_empty() {
        return false;
    }
    if path.starts_with("album/") || path.starts_with("track/") {
        return true;
    }
    // a band page counts too: `x.bandcamp.com`, `x.bandcamp.com/`, `x.bandcamp.com/music`
    host.ends_with(".bandcamp.com") && matches!(path.trim_end_matches('/'), "" | "music")
}

/// Group key of an item URL: the host (a subdomain on Bandcamp is the artist or label).
pub fn host_of(url: &str) -> String {
    let rest = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://")).unwrap_or(url);
    rest.split(['/', '?', '#']).next().unwrap_or("").to_ascii_lowercase()
}

/// `julieslick.bandcamp.com` -> (`julieslick`, `.bandcamp.com`); other hosts have no suffix.
pub fn split_host(host: &str) -> (&str, &str) {
    let suffix = ".bandcamp.com";
    match host.strip_suffix(suffix) {
        Some(name) => (name, &host[name.len()..]),
        None => (host, ""),
    }
}

/// The path part of an item URL without scheme and host (`/album/x`), or the whole bare url.
pub fn bare_path(url: &str) -> String {
    let rest = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://")).unwrap_or(url);
    match rest.split_once('/') {
        Some((_, p)) => format!("/{p}"),
        None => String::new(),
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Filter {
    All,
    #[default]
    Queued,
    Failed,
    Done,
}

impl Filter {
    pub const ALL: [Filter; 4] = [Filter::All, Filter::Queued, Filter::Failed, Filter::Done];
    /// What each filter asks the server for; `None` is everything.
    pub fn status(self) -> Option<&'static str> {
        match self {
            Filter::All => None,
            Filter::Queued => Some("pending,running"),
            Filter::Failed => Some("failed"),
            Filter::Done => Some("done,skipped"),
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Filter::All => "All",
            Filter::Queued => "Queued",
            Filter::Failed => "Failed",
            Filter::Done => "Done",
        }
    }
    pub fn empty(self) -> &'static str {
        match self {
            Filter::All => "No items.",
            Filter::Queued => "Nothing queued.",
            Filter::Failed => "No failures.",
            Filter::Done => "Nothing finished yet.",
        }
    }
}

/// A live job opens on what is still to come (the part worth sorting); a finished one on the record.
pub fn default_filter(job_status: &str) -> Filter {
    if is_open(job_status) { Filter::Queued } else { Filter::All }
}

pub fn is_active(status: &str) -> bool {
    matches!(status, JOB_QUEUED | JOB_RUNNING)
}
/// Not finished: queued, running or paused. What can still be paused, resumed or cancelled.
pub fn is_open(status: &str) -> bool {
    is_active(status) || status == JOB_PAUSED
}

/// Status of an item (or job) -> (css tone, icon, label). Colour is never alone.
pub fn status_look(status: &str) -> (&'static str, &'static str) {
    match status {
        "running" => ("info", "refresh"),
        "completed" | "done" => ("ok", "check-circle"),
        "failed" => ("danger", "x-circle"),
        "paused" => ("warn", "pause-circle"),
        "cancelled" => ("idle", "x"),
        "skipped" => ("idle", "skip-next"),
        _ => ("idle", "clock"),
    }
}

/// Counts of a group by item status; mutated in place by item events.
pub fn move_between(g: &mut JobItemGroupOut, from: &str, to: &str) {
    if from == to {
        return;
    }
    sub(g, from);
    add(g, to);
}

fn field<'a>(g: &'a mut JobItemGroupOut, s: &str) -> Option<&'a mut i64> {
    match s {
        ITEM_PENDING => Some(&mut g.pending),
        ITEM_RUNNING => Some(&mut g.running),
        ITEM_DONE => Some(&mut g.done),
        ITEM_FAILED => Some(&mut g.failed),
        ITEM_SKIPPED => Some(&mut g.skipped),
        ITEM_CANCELLED => Some(&mut g.cancelled),
        _ => None,
    }
}
fn sub(g: &mut JobItemGroupOut, s: &str) {
    if let Some(f) = field(g, s) {
        *f = (*f - 1).max(0);
    }
}
fn add(g: &mut JobItemGroupOut, s: &str) {
    if let Some(f) = field(g, s) {
        *f += 1;
    }
}

/// Number of items of a group visible under a filter, from its counts.
pub fn visible_count(g: &JobItemGroupOut, filter: Filter) -> i64 {
    match filter {
        Filter::All => g.total,
        Filter::Queued => g.pending + g.running,
        Filter::Failed => g.failed,
        Filter::Done => g.done + g.skipped,
    }
}

/// `5s` / `3m` ... time since an RFC 3339 stamp is the caller's business; this formats eta-less counts.
pub fn counts_line(j: &JobOut) -> String {
    let done = j.completed + j.failed + j.skipped;
    let mut s = format!("{done}/{} \u{b7} {} ok", j.total, j.completed);
    if j.skipped > 0 {
        s.push_str(&format!(" \u{b7} {} skipped", j.skipped));
    }
    if j.failed > 0 {
        s.push_str(&format!(" \u{b7} {} failed", j.failed));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_bandcamp_lines() {
        let t = "https://a.bandcamp.com/album/x\nhttps://b.bandcamp.com/track/y, https://c.bandcamp.com\nnot a url\nhttps://d.bandcamp.com/music\nhttps://example.com/\nhttps://dom.example.org/album/z";
        assert_eq!(local_url_count(t), 5);
        assert!(!looks_like_bandcamp("ftp://a.bandcamp.com/album/x"));
        assert!(!looks_like_bandcamp("https://a.bandcamp.com/merch/x"));
        assert!(looks_like_bandcamp("HTTPS://A.BANDCAMP.COM/"));
    }

    #[test]
    fn hosts() {
        assert_eq!(host_of("https://Julie.bandcamp.com/album/x?y=1"), "julie.bandcamp.com");
        assert_eq!(split_host("julie.bandcamp.com"), ("julie", ".bandcamp.com"));
        assert_eq!(split_host("custom.example.org"), ("custom.example.org", ""));
        assert_eq!(bare_path("https://a.bandcamp.com/album/x"), "/album/x");
        assert_eq!(bare_path("https://a.bandcamp.com"), "");
    }

    #[test]
    fn filters() {
        assert_eq!(Filter::Queued.status(), Some("pending,running"));
        assert_eq!(default_filter(JOB_RUNNING), Filter::Queued);
        assert_eq!(default_filter(JOB_PAUSED), Filter::Queued);
        assert_eq!(default_filter(JOB_COMPLETED), Filter::All);
    }

    #[test]
    fn group_counts_follow_item_events() {
        let mut g = JobItemGroupOut { total: 3, pending: 2, running: 1, ..Default::default() };
        move_between(&mut g, ITEM_PENDING, ITEM_RUNNING);
        assert_eq!((g.pending, g.running), (1, 2));
        move_between(&mut g, ITEM_RUNNING, ITEM_DONE);
        move_between(&mut g, ITEM_RUNNING, ITEM_FAILED);
        assert_eq!((g.pending, g.running, g.done, g.failed), (1, 0, 1, 1));
        move_between(&mut g, ITEM_RUNNING, ITEM_DONE); // never below zero
        assert_eq!(g.running, 0);
        assert_eq!(visible_count(&g, Filter::Queued), 1);
        assert_eq!(visible_count(&g, Filter::Done), 2);
    }

    #[test]
    fn counts_line_mentions_only_non_zero() {
        let j = JobOut { total: 10, completed: 6, failed: 1, skipped: 0, ..Default::default() };
        assert_eq!(counts_line(&j), "7/10 \u{b7} 6 ok \u{b7} 1 failed");
    }
}
