//! Extension points the job-control routes call into. Workers (the download
//! worker, WS1's scanner/mover, WS3's analyzer) implement [`JobHooks`] and
//! register with `JobsService::add_hooks`, so cancel/pause reach whatever is
//! mid-item without `bc-jobs` knowing about them.

use async_trait::async_trait;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interrupt {
    /// The job was cancelled: kill the work, settle the item `cancelled`.
    Cancel,
    /// The job was paused: kill the work, hand the item back (`release_item`).
    Pause,
}

#[async_trait]
pub trait JobHooks: Send + Sync {
    /// Interrupt in-flight items of `job_id` and return once they are settled.
    /// Default: nothing in flight here.
    async fn interrupt_job(&self, _job_id: &str, _reason: Interrupt) {}

    /// URLs of removed / unrun items. The harvest inbox flips its `queued` rows
    /// back to `new`.
    async fn release_urls(&self, _urls: &[String]) {}

    /// New work may be claimable (resume/retry).
    fn wake(&self) {}
}
