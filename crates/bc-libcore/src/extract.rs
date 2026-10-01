//! Query extractor that understands repeated keys (`tags=a&tags=b`) and maps
//! rejections to problem+json.

use axum::extract::{FromRequestParts, Request};
use axum::http::request::Parts;
use serde::de::DeserializeOwned;

use crate::error::ApiError;

/// `?a=1&a=2` aware query extractor (serde_html_form).
pub struct Q<T>(pub T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequestParts<S> for Q<T> {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        let q = parts.uri.query().unwrap_or("");
        parse_query(q).map(Q)
    }
}

pub fn parse_query<T: DeserializeOwned>(q: &str) -> Result<T, ApiError> {
    axum_extra::extract::Query::<T>::try_from_uri(&format!("/?{q}").parse().map_err(|_| ApiError::unprocessable("bad query"))?)
        .map(|q| q.0)
        .map_err(|e| ApiError::unprocessable(format!("bad query: {e}")))
}

#[allow(dead_code)]
fn _assert_request_unused(_: Request) {}

#[cfg(test)]
mod tests {
    use super::*;
    use bc_types::library::{SortDir, TrackQuery, TrackSort};

    #[test]
    fn repeated_keys_and_numbers() {
        let q: TrackQuery = parse_query("tags=a&tags=b%20c&release_ids=1&release_ids=2&offset=74000&sort=play_count&order=asc&loved=true&q=dub+techno").unwrap();
        assert_eq!(q.tags, vec!["a", "b c"]);
        assert_eq!(q.release_ids, vec![1, 2]);
        assert_eq!(q.offset, Some(74000));
        assert_eq!(q.sort, Some(TrackSort::PlayCount));
        assert_eq!(q.order, Some(SortDir::Asc));
        assert_eq!(q.loved, Some(true));
        assert_eq!(q.q.as_deref(), Some("dub techno"));
    }

    #[test]
    fn bad_value_is_422() {
        let e = parse_query::<TrackQuery>("offset=abc").err().unwrap();
        assert_eq!(e.status(), 422);
    }

    #[test]
    fn empty_query() {
        let q: TrackQuery = parse_query("").unwrap();
        assert_eq!(q, TrackQuery::default());
    }
}
