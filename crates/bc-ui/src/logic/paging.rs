//! Sparse index-addressed paging for the virtualised DataTable/CardGrid: count
//! first, then 200-row pages around the viewport (+/-1 page prefetch). Memory
//! stays flat at any library size and the scrollbar can jump to any row.
use std::collections::{HashMap, HashSet};

pub const PAGE_SIZE: usize = 200;

/// Visible row window with overscan.
pub fn visible_range(scroll_top: f64, viewport_h: f64, row_h: f64, total: usize, overscan: usize) -> (usize, usize) {
    if total == 0 || row_h <= 0.0 {
        return (0, 0);
    }
    let first = (scroll_top / row_h).floor().max(0.0) as usize;
    let last = ((scroll_top + viewport_h) / row_h).ceil().max(0.0) as usize;
    (first.saturating_sub(overscan).min(total), (last + overscan).min(total))
}

/// Pages touched by rows `[start, end)`, extended by `prefetch` pages each side.
pub fn pages_for_rows(start: usize, end: usize, total: usize, prefetch: usize) -> Vec<usize> {
    if total == 0 || end <= start {
        return vec![];
    }
    let last_page = (total - 1) / PAGE_SIZE;
    let a = (start / PAGE_SIZE).saturating_sub(prefetch);
    let b = (((end - 1).min(total - 1)) / PAGE_SIZE + prefetch).min(last_page);
    (a..=b).collect()
}

/// Sparse page store with bounded memory: pages farther than `keep_radius`
/// pages from the viewport are dropped.
#[derive(Debug)]
pub struct Pager<T> {
    pub total: Option<usize>,
    pages: HashMap<usize, Vec<T>>,
    inflight: HashSet<usize>,
    keep_radius: usize,
}

impl<T> Default for Pager<T> {
    fn default() -> Self {
        Self::new(6)
    }
}

impl<T> Pager<T> {
    pub fn new(keep_radius: usize) -> Self {
        Self { total: None, pages: HashMap::new(), inflight: HashSet::new(), keep_radius }
    }
    pub fn reset(&mut self) {
        self.total = None;
        self.pages.clear();
        self.inflight.clear();
    }
    /// Keep rows but forget freshness: used on invalidation (stale-while-revalidate).
    pub fn page_count(&self) -> usize {
        self.pages.len()
    }
    pub fn row(&self, i: usize) -> Option<&T> {
        self.pages.get(&(i / PAGE_SIZE)).and_then(|p| p.get(i % PAGE_SIZE))
    }
    pub fn has_page(&self, p: usize) -> bool {
        self.pages.contains_key(&p)
    }
    pub fn put(&mut self, page: usize, rows: Vec<T>) {
        self.inflight.remove(&page);
        self.pages.insert(page, rows);
    }
    pub fn fail(&mut self, page: usize) {
        self.inflight.remove(&page);
    }
    /// Which pages to request for the viewport, marking them in flight.
    pub fn need(&mut self, start: usize, end: usize) -> Vec<usize> {
        let Some(total) = self.total else { return vec![] };
        let want = pages_for_rows(start, end, total, 1);
        let out: Vec<usize> = want.into_iter().filter(|p| !self.pages.contains_key(p) && !self.inflight.contains(p)).collect();
        self.inflight.extend(out.iter().copied());
        out
    }
    /// Drop every page not in `keep` (rows of kept pages stay until replaced: SWR).
    pub fn keep_only(&mut self, keep: &[usize]) {
        self.pages.retain(|p, _| keep.contains(p));
    }
    /// Drop pages far from the viewport. Returns how many were dropped.
    pub fn trim(&mut self, start: usize, end: usize) -> usize {
        let centre = (start + end) / 2 / PAGE_SIZE;
        let before = self.pages.len();
        let r = self.keep_radius;
        self.pages.retain(|p, _| p.abs_diff(centre) <= r);
        before - self.pages.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_math() {
        assert_eq!(visible_range(0.0, 360.0, 36.0, 1000, 5), (0, 15));
        assert_eq!(visible_range(36.0 * 100.0, 360.0, 36.0, 1000, 5), (95, 115));
        assert_eq!(visible_range(1e9, 360.0, 36.0, 1000, 5), (1000, 1000));
        assert_eq!(visible_range(0.0, 360.0, 36.0, 0, 5), (0, 0));
    }

    #[test]
    fn pages_around_viewport_plus_minus_one() {
        assert_eq!(pages_for_rows(0, 20, 190_000, 1), vec![0, 1]);
        assert_eq!(pages_for_rows(74_000, 74_020, 190_000, 1), vec![369, 370, 371]);
        assert_eq!(pages_for_rows(189_990, 190_000, 190_000, 1), vec![948, 949]);
        assert!(pages_for_rows(0, 0, 10, 1).is_empty());
    }

    #[test]
    fn jump_fetches_only_the_target_pages() {
        let mut p: Pager<u32> = Pager::new(3);
        p.total = Some(190_000);
        assert_eq!(p.need(0, 30), vec![0, 1]);
        // already in flight: not asked again
        assert!(p.need(0, 30).is_empty());
        p.put(0, vec![1; 200]);
        p.put(1, vec![2; 200]);
        // scrollbar jump to the middle
        assert_eq!(p.need(95_000, 95_030), vec![474, 475, 476]);
        p.put(475, (0..200).collect());
        assert_eq!(p.row(95_000 + 5), Some(&5));
        assert_eq!(p.row(10), Some(&1));
    }

    #[test]
    fn memory_stays_bounded() {
        let mut p: Pager<u8> = Pager::new(2);
        p.total = Some(10_000);
        for i in 0..40 {
            p.put(i, vec![0; 200]);
        }
        let dropped = p.trim(6000, 6030);
        assert!(dropped > 0);
        assert!(p.page_count() <= 5);
    }

    #[test]
    fn failed_page_can_be_retried() {
        let mut p: Pager<u8> = Pager::default();
        p.total = Some(1000);
        assert!(!p.need(0, 10).is_empty());
        p.fail(0);
        assert!(p.need(0, 10).contains(&0));
    }
}
