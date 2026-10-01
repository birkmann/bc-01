//! `bc_maint::BandcampLookup` for the real client, so WS5 can inject it with
//! `LibraryService::with_bandcamp(Arc::new(BcLookup::new(ctx.client.clone())))`.

use async_trait::async_trait;
use bc_maint::lookup::{AlbumInfo, AlbumTrack, BandcampLookup, LookupError};

use crate::net::{BandcampClient, GetOpts, PageKind};
use crate::{extract, sources, urls};

/// The Bandcamp operations the library maintenance code needs, over the shared client (so every
/// page read rides the token bucket and the page cache).
#[derive(Clone)]
pub struct BcLookup {
    client: BandcampClient,
}

impl BcLookup {
    pub fn new(client: BandcampClient) -> Self {
        Self { client }
    }
}

#[async_trait]
impl BandcampLookup for BcLookup {
    /// A `/track/` URL resolves to its parent album, a standalone single (or an album URL) to itself.
    async fn resolve_album_url(&self, url: &str) -> Result<String, LookupError> {
        sources::resolve_album_url(&self.client, url).await.map_err(|e| LookupError(e.to_string()))
    }

    async fn fetch_album(&self, url: &str) -> Result<AlbumInfo, LookupError> {
        let canonical = urls::normalise(url);
        let body =
            self.client.get_html(&canonical, GetOpts::kind(PageKind::Album)).await.map_err(|e| LookupError(e.to_string()))?;
        let r = extract::parse_tralbum(&body, &canonical);
        Ok(AlbumInfo {
            url: r.url,
            title: r.title,
            artist_name: r.artist_name,
            release_date: r.release_date,
            is_preorder: r.is_preorder,
            label_name: r.label_name,
            about: r.about,
            credits: r.credits,
            tracks: r.tracks.into_iter().map(|t| AlbumTrack { available: t.stream_url.is_some(), title: t.title, track_num: t.track_num, duration_sec: t.duration_sec.filter(|d| *d > 0.0) }).collect(),
        })
    }
}
