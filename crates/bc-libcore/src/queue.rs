//! Download-queue access for library features (fill missing, loved download, strays).
//! Rows go straight into the shared durable queue tables (`jobs`, `job_items`, legacy
//! schema) so workstream 2's worker claims them with its normal claim SQL; the queue
//! owner is notified through [`crate::JobHost::notify_download_queue`].

use bc_db::rusqlite::{Connection, Transaction, params};
use bc_db::util::now_db;

use crate::error::ApiResult;

#[derive(Debug, Clone, Default)]
pub struct NewItem {
    pub url: Option<String>,
    pub url_kind: Option<String>,
    pub source: Option<String>,
    pub target_dir: Option<String>,
    pub track_id: Option<i64>,
    pub release_id: Option<i64>,
}

/// Insert a queued `download` job with its items; returns the job id. Runs in the caller's tx.
pub fn create_job(
    t: &Transaction<'_>,
    kind: &str,
    label: &str,
    priority: i64,
    params_json: &serde_json::Value,
    items: &[NewItem],
) -> ApiResult<String> {
    let id = uuid::Uuid::new_v4().to_string();
    t.execute(
        "INSERT INTO jobs (id, kind, status, label, priority, params, total, completed, failed, skipped, cancel_requested, created_at)
         VALUES (?1, ?2, 'queued', ?3, ?4, ?5, ?6, 0, 0, 0, 0, ?7)",
        params![id, kind, label, priority, params_json.to_string(), items.len() as i64, now_db()],
    )?;
    let mut st = t.prepare_cached(
        "INSERT INTO job_items (job_id, seq, status, url, url_kind, source, target_dir, track_id, release_id, attempts, max_attempts, progress)
         VALUES (?1, ?2, 'pending', ?3, ?4, ?5, ?6, ?7, ?8, 0, 3, 0)",
    )?;
    for (seq, it) in items.iter().enumerate() {
        st.execute(params![id, seq as i64, it.url, it.url_kind, it.source, it.target_dir, it.track_id, it.release_id])?;
    }
    Ok(id)
}

/// URLs of unfinished download items (pending/running in a live job): the "already queued" check.
pub fn pending_urls(c: &Connection) -> ApiResult<std::collections::HashSet<String>> {
    let mut st = c.prepare(
        "SELECT ji.url FROM job_items ji JOIN jobs j ON j.id = ji.job_id
          WHERE ji.url IS NOT NULL AND ji.status IN ('pending','running') AND j.status IN ('queued','running','paused')",
    )?;
    Ok(st.query_map([], |r| r.get::<_, String>(0))?.collect::<Result<_, _>>()?)
}

/// The Bandcamp URL a fill for this release would be queued from (port of `fill_source_url`).
pub fn fill_source_url(c: &Connection, release_id: i64) -> ApiResult<Option<String>> {
    use bc_db::rusqlite::OptionalExtension;
    if let Some(Some(u)) = c
        .query_row("SELECT bandcamp_url FROM releases WHERE id = ?1", [release_id], |r| r.get::<_, Option<String>>(0))
        .optional()?
    {
        return Ok(Some(u));
    }
    if let Some(u) = c
        .query_row(
            "SELECT url FROM harvest_items WHERE release_id = ?1 ORDER BY (url_kind = 'album') DESC, id LIMIT 1",
            [release_id],
            |r| r.get::<_, String>(0),
        )
        .optional()?
    {
        return Ok(Some(u));
    }
    Ok(c.query_row(
        "SELECT bandcamp_url FROM tracks WHERE release_id = ?1 AND bandcamp_url IS NOT NULL ORDER BY id LIMIT 1",
        [release_id],
        |r| r.get::<_, String>(0),
    )
    .optional()?)
}

/// `track` URL (`/track/`) vs `album`.
pub fn url_kind(url: &str) -> &'static str {
    if url.contains("/track/") { "track" } else { "album" }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bc_db::Db;

    #[test]
    fn creates_job_with_items() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("t.db")).unwrap();
        let id = db
            .write(|t| {
                Ok(create_job(
                    t,
                    "download",
                    "fill",
                    90,
                    &serde_json::json!({"force": true}),
                    &[NewItem { url: Some("https://a.bandcamp.com/album/x".into()), url_kind: Some("album".into()), source: Some("fill".into()), ..Default::default() }],
                )
                .unwrap())
            })
            .unwrap();
        db.read(|c| {
            let urls = pending_urls(c).unwrap();
            assert!(urls.contains("https://a.bandcamp.com/album/x"));
            let n: i64 = c.query_row("SELECT total FROM jobs WHERE id=?1", [&id], |r| r.get(0))?;
            assert_eq!(n, 1);
            Ok(())
        })
        .unwrap();
    }
}
