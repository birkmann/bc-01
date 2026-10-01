//! Where the albums grid stood, remembered across reloads (port of `gridPlace.ts`).
//! The place is a release, not a pixel offset: ids survive merges and resizes.
use serde::{Deserialize, Serialize};

pub const PLACE_KEY: &str = "bc:albums:place:v1";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GridPlace {
    /// The listing this place is in; another listing's place is ignored.
    pub key: String,
    /// The release at the top-left of the viewport when saved.
    #[serde(rename = "anchorId")]
    pub anchor_id: i64,
    /// Its index in the listing then.
    pub index: u64,
    /// Pixels from the scroller's top edge to that card's top edge (negative once scrolled past).
    pub offset: f64,
}

#[derive(Debug, Clone, Default)]
pub struct ListingWords {
    pub tags: Vec<String>,
    pub added: Option<String>,
    pub sort: String,
    pub order: String,
    pub fan: Option<i64>,
    pub missing: bool,
}

/// Stable key naming a listing. Absent and off flags are the same listing.
pub fn listing_key(w: &ListingWords) -> String {
    let mut m = serde_json::Map::new();
    m.insert("tags".into(), w.tags.clone().into());
    m.insert("added".into(), w.added.clone().map(Into::into).unwrap_or(serde_json::Value::Null));
    m.insert("sort".into(), w.sort.clone().into());
    m.insert("order".into(), w.order.clone().into());
    if let Some(f) = w.fan {
        m.insert("fan".into(), f.into());
    }
    if w.missing {
        m.insert("missing".into(), true.into());
    }
    serde_json::Value::Object(m).to_string()
}

/// A stored place, or `None` for anything that is not one.
pub fn parse_place(raw: Option<&str>) -> Option<GridPlace> {
    let v: serde_json::Value = serde_json::from_str(raw?).ok()?;
    let key = v.get("key")?.as_str()?.to_string();
    let anchor_id = v.get("anchorId")?.as_i64()?;
    let idx = v.get("index")?.as_f64()?;
    let offset = v.get("offset")?.as_f64()?;
    if idx < 0.0 || idx.fract() != 0.0 || !offset.is_finite() {
        return None;
    }
    Some(GridPlace { key, anchor_id, index: idx as u64, offset })
}

/// How many pages from the top it takes to have the release at `index` on screen.
pub fn pages_for(index: u64, page_size: u64) -> u64 {
    index / page_size + 1
}

pub fn away_from_top(p: &GridPlace) -> bool {
    p.index > 0 || p.offset < 0.0
}

/// Pages `0..count`, at most `width` in flight, resolves to the longest
/// contiguous prefix that arrived (a hole would make the feed continue from
/// the wrong place). A failure stops new requests. Runs sequentially in
/// batches of `width`, which keeps it executor-agnostic.
pub async fn fetch_pages<T, E, F, Fut>(count: usize, width: usize, mut fetch: F) -> Vec<T>
where
    F: FnMut(usize) -> Fut,
    Fut: std::future::Future<Output = Result<T, E>>,
{
    let width = width.max(1);
    let mut out: Vec<Option<T>> = (0..count).map(|_| None).collect();
    let mut next = 0;
    let mut failed = false;
    while next < count && !failed {
        let end = (next + width).min(count);
        let futs: Vec<_> = (next..end).map(|i| fetch(i)).collect();
        let res = futures::future::join_all(futs).await;
        for (k, r) in res.into_iter().enumerate() {
            match r {
                Ok(v) => out[next + k] = Some(v),
                Err(_) => failed = true,
            }
        }
        next = end;
    }
    let mut prefix = vec![];
    for p in out {
        match p {
            Some(v) => prefix.push(v),
            None => break,
        }
    }
    prefix
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::executor::block_on;

    fn base() -> ListingWords {
        ListingWords { tags: vec!["techno".into()], added: None, sort: "added".into(), order: "desc".into(), ..Default::default() }
    }

    #[test]
    fn listing_key_is_stable_and_distinguishing() {
        assert_eq!(listing_key(&base()), listing_key(&base()));
        let k = listing_key(&base());
        assert_ne!(listing_key(&ListingWords { tags: vec!["house".into()], ..base() }), k);
        assert_ne!(listing_key(&ListingWords { added: Some("24h".into()), ..base() }), k);
        assert_ne!(listing_key(&ListingWords { sort: "year".into(), ..base() }), k);
        assert_ne!(listing_key(&ListingWords { order: "asc".into(), ..base() }), k);
        assert_ne!(listing_key(&ListingWords { missing: true, ..base() }), k);
        assert_eq!(listing_key(&ListingWords { missing: false, ..base() }), k);
    }

    #[test]
    fn parse_place_roundtrip_and_rejects() {
        let valid = r#"{"key":"k","anchorId":42,"index":1234,"offset":-37.5}"#;
        assert_eq!(parse_place(Some(valid)), Some(GridPlace { key: "k".into(), anchor_id: 42, index: 1234, offset: -37.5 }));
        for bad in [
            None,
            Some(""),
            Some("not json"),
            Some(r#"{"key":7,"anchorId":42,"index":1,"offset":0}"#),
            Some(r#"{"key":"k","anchorId":"42","index":1,"offset":0}"#),
            Some(r#"{"key":"k","anchorId":42,"index":-1,"offset":0}"#),
            Some(r#"{"key":"k","anchorId":42,"index":1.5,"offset":0}"#),
            Some(r#"{"key":"k","anchorId":42,"index":1}"#),
        ] {
            assert_eq!(parse_place(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn pages_for_counts_pages_down_to_the_index() {
        assert_eq!(pages_for(0, 200), 1);
        assert_eq!(pages_for(199, 200), 1);
        assert_eq!(pages_for(200, 200), 2);
        assert_eq!(pages_for(12345, 200), 62);
    }

    #[test]
    fn away_from_top_only_false_at_very_top() {
        let p = |index, offset| GridPlace { key: "k".into(), anchor_id: 1, index, offset };
        assert!(!away_from_top(&p(0, 8.0)));
        assert!(away_from_top(&p(0, -120.0)));
        assert!(away_from_top(&p(3, 8.0)));
    }

    #[test]
    fn fetch_pages_in_order() {
        let got = block_on(fetch_pages(5, 2, |i| async move { Ok::<_, ()>(format!("p{i}")) }));
        assert_eq!(got, ["p0", "p1", "p2", "p3", "p4"]);
    }

    #[test]
    fn fetch_pages_keeps_contiguous_prefix_and_stops() {
        let issued = std::cell::RefCell::new(vec![]);
        let got = block_on(fetch_pages(6, 3, |i| {
            issued.borrow_mut().push(i);
            async move { if i == 1 { Err(()) } else { Ok(format!("p{i}")) } }
        }));
        assert_eq!(got, ["p0"]);
        assert!(!issued.borrow().contains(&5));
    }

    #[test]
    fn fetch_pages_empty() {
        let got: Vec<u8> = block_on(fetch_pages(0, 4, |_| async { Err::<u8, ()>(()) }));
        assert!(got.is_empty());
    }
}
