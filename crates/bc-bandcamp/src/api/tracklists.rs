//! Uploading tracklists and matching their rows over HTTP (legacy `api/routes/tracklists.py`).
//!
//! The parse route is the only multipart endpoint in the app, and it exists in that shape for one
//! reason: these CSVs arrive mis-encoded, and a browser that reads them as text has already
//! destroyed what the repair works from. Matching is one request per row, driven from the client,
//! because Bandcamp's search sits behind a shared token bucket. Downloading is not here: the
//! confirmed URLs go to `POST /downloads` with `single_folder` and `tracks_only`.

use std::sync::Arc;

use async_trait::async_trait;
use axum::extract::{Multipart, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use bc_jobs::ApiError;
use bc_types::bandcamp::{CandidateOut, MatchOut, MatchRequest, ParsedFileOut, ParsedTracklists, SearchHitOut, TrackRowOut};

use super::explore::common::{blacklisted, known_releases, owned, unprocessable};
use crate::error::HarvestError;
use crate::net::BandcampClient;
use crate::service::Ctx;
use crate::sources;
use crate::tracklist::{self, SearchHit, SearchKind, Searcher, TrackRow, UploadError, UploadedFile};

pub fn router(ctx: Arc<Ctx>) -> Router {
    Router::new()
        .route("/tracklists/parse", post(parse))
        .route("/tracklists/match", post(match_row))
        // Room for 20 files of 2 MB plus multipart framing; the per-file limit is the real guard.
        .layer(axum::extract::DefaultBodyLimit::max(48 * 1024 * 1024))
        .with_state(ctx)
}

// -- the Searcher adapter ----------------------------------------------------------------------

/// `sources::search` behind the matcher's [`Searcher`] seam (`sources::SearchHit` <-> `tracklist::SearchHit`).
pub struct ClientSearcher {
    pub client: BandcampClient,
}

impl From<sources::SearchHit> for SearchHit {
    fn from(h: sources::SearchHit) -> Self {
        SearchHit { kind: h.kind, name: h.name, url: h.url, subtitle: h.subtitle, art_url: h.art_url, band_id: h.band_id, item_id: h.item_id }
    }
}

impl From<SearchHit> for sources::SearchHit {
    fn from(h: SearchHit) -> Self {
        sources::SearchHit {
            kind: h.kind,
            name: h.name,
            url: h.url,
            subtitle: h.subtitle,
            art_url: h.art_url,
            band_id: h.band_id,
            item_id: h.item_id,
            location: None,
        }
    }
}

/// The matcher's default page size (`matching::DEFAULT_LIMIT`).
const SEARCH_LIMIT: usize = 20;

#[async_trait]
impl Searcher for ClientSearcher {
    async fn search(&self, query: &str, kind: SearchKind, limit: usize) -> Result<Vec<SearchHit>, HarvestError> {
        let hits = sources::search(&self.client, query, kind.as_filter(), limit.max(1)).await?;
        Ok(hits.into_iter().map(SearchHit::from).collect())
    }
}

// -- parse -----------------------------------------------------------------------------------

/// A 400 whose problem body also carries the per-file errors (`files`), like the legacy
/// `BadRequest(files=...)`.
struct ParseRejected {
    detail: String,
    files: Vec<String>,
}

impl IntoResponse for ParseRejected {
    fn into_response(self) -> Response {
        let body = serde_json::json!({
            "type": "about:blank", "title": "Bad Request", "status": 400, "detail": self.detail, "files": self.files,
        });
        let mut resp = (StatusCode::BAD_REQUEST, Json(body)).into_response();
        resp.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/problem+json"));
        resp
    }
}

enum ParseError {
    Rejected(ParseRejected),
    Api(ApiError),
}

impl IntoResponse for ParseError {
    fn into_response(self) -> Response {
        match self {
            ParseError::Rejected(r) => r.into_response(),
            ParseError::Api(e) => e.into_response(),
        }
    }
}

async fn parse(mut mp: Multipart) -> Result<Json<ParsedTracklists>, ParseError> {
    let mut uploads: Vec<UploadedFile> = Vec::new();
    while let Some(field) = mp.next_field().await.map_err(|e| ParseError::Api(ApiError::bad_request(e.to_string())))? {
        if field.name() != Some("files") {
            continue;
        }
        let filename = field.file_name().map(str::to_string);
        let data = field.bytes().await.map_err(|e| ParseError::Api(ApiError::bad_request(e.to_string())))?;
        uploads.push(UploadedFile { filename, data: data.to_vec() });
    }

    let parsed = tracklist::parse_upload(&uploads).map_err(|e| match &e {
        UploadError::NoneReadable(files) => {
            ParseError::Rejected(ParseRejected { detail: e.to_string(), files: files.clone() })
        }
        _ => ParseError::Api(ApiError::bad_request(e.to_string())),
    })?;

    Ok(Json(ParsedTracklists {
        suggested_title: parsed.suggested_title,
        files: parsed
            .files
            .iter()
            .map(|p| ParsedFileOut {
                filename: p.filename.clone(),
                title: p.title.clone(),
                rows: p.rows.len() as i64,
                skipped: p.skipped as i64,
                error: p.error.clone(),
            })
            .collect(),
        rows: parsed
            .rows
            .iter()
            .map(|r| TrackRowOut {
                seq: r.seq as i64,
                artist: r.artist.clone(),
                title: r.title.clone(),
                label: r.label.clone(),
                start: r.start.clone(),
                end: r.end.clone(),
                source_file: r.source_file.clone(),
            })
            .collect(),
        duplicates: parsed.duplicates as i64,
    }))
}

// -- match -----------------------------------------------------------------------------------

/// One row, up to three rate-limited searches, ranked candidates back. Called once per row by the
/// review table; the pacing is the shared token bucket's job, not the caller's.
async fn match_row(State(ctx): State<Arc<Ctx>>, Json(body): Json<MatchRequest>) -> Result<Json<MatchOut>, ApiError> {
    let len = |s: &str| s.chars().count();
    if !(1..=200).contains(&len(&body.artist)) {
        return Err(unprocessable("artist: must be 1 to 200 characters"));
    }
    if !(1..=300).contains(&len(&body.title)) {
        return Err(unprocessable("title: must be 1 to 300 characters"));
    }
    if len(&body.label) > 200 {
        return Err(unprocessable("label: at most 200 characters"));
    }
    if body.query.as_deref().is_some_and(|q| !(1..=300).contains(&len(q))) {
        return Err(unprocessable("query: must be 1 to 300 characters"));
    }

    let row = TrackRow::new(body.artist.trim(), body.title.trim(), body.label.trim());
    let searcher = ClientSearcher { client: ctx.client.clone() };
    let result = tracklist::match_row(&searcher, &row, SEARCH_LIMIT, body.query.as_deref()).await?;

    // The same badge the Explore grid shows, and for the same reason: a crate can legitimately
    // re-download something the shelf already holds, but the user should be told.
    let items: Vec<_> = result.candidates.iter().map(|c| (c.hit.url.clone(), c.hit.subtitle.clone(), c.hit.name.clone())).collect();
    let known = known_releases(&ctx.db, &items).await?;

    Ok(Json(MatchOut {
        query: result.query,
        best_index: result.best_index.map(|i| i as i64),
        searches: result.searches as i64,
        candidates: result
            .candidates
            .into_iter()
            .map(|c| CandidateOut {
                hit: SearchHitOut {
                    in_library: owned(&known, &c.hit.url),
                    blacklisted: blacklisted(&known, &c.hit.url),
                    library_release_id: None,
                    kind: c.hit.kind,
                    name: c.hit.name,
                    url: c.hit.url,
                    subtitle: c.hit.subtitle,
                    art_url: c.hit.art_url,
                    band_id: c.hit.band_id,
                    item_id: c.hit.item_id,
                },
                score: c.score,
                tier: c.tier.to_string(),
                label_match: c.label_match,
            })
            .collect(),
    }))
}
