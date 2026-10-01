//! Pure logic ported from the legacy frontend `web/frontend/src/lib/*.ts` (wasm32-safe: only
//! `serde`/`std`; no tokio, no clock, no rand: time and randomness are parameters).
//! Owned by the WS2 logic-port agent.
//!
//! | Rust module | TypeScript source | Used by the Leptos UI in |
//! | --- | --- | --- |
//! | [`feed_groups`] | `feedGroups.ts` | Feed page (label / artist grouping of an inbox page) |
//! | [`bandcamp_search`] | `bandcampSearch.ts` | Explore screen + library search's Bandcamp fallback |
//! | [`tracklist`] | `tracklist.ts` | Tracklist review table (state, selection, summary, pool) |
//! | [`gate`] | `gate.ts` | Any page throttling parallel Bandcamp requests |
//! | [`fan_queue`] | `fanQueue.ts` | Player store: wishlist "play through" continuation |
//! | [`explore_sweep`] | `exploreSweep.ts` | Explore/grid "play all / shuffle" queue filler |
#![allow(clippy::collapsible_if, clippy::type_complexity)]

pub mod bandcamp_search;
pub mod explore_sweep;
pub mod fan_queue;
pub mod feed_groups;
pub mod gate;
pub mod tracklist;

/// Shared minimal single-threaded executor helpers for the async tests.
#[cfg(test)]
pub(crate) mod testutil {
    use std::future::Future;
    use std::pin::Pin;
    use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

    fn raw() -> RawWaker {
        fn clone(_: *const ()) -> RawWaker {
            raw()
        }
        fn noop(_: *const ()) {}
        static VT: RawWakerVTable = RawWakerVTable::new(clone, noop, noop, noop);
        RawWaker::new(std::ptr::null(), &VT)
    }

    pub fn waker() -> Waker {
        // SAFETY: the vtable functions ignore the (null) data pointer.
        unsafe { Waker::from_raw(raw()) }
    }

    /// Busy-poll a future to completion (tests only; futures here never block on real I/O).
    pub fn block_on<F: Future>(fut: F) -> F::Output {
        let mut fut = Box::pin(fut);
        let w = waker();
        let mut cx = Context::from_waker(&w);
        loop {
            if let Poll::Ready(v) = fut.as_mut().poll(&mut cx) {
                return v;
            }
        }
    }

    pub fn poll_once<F: Future + ?Sized>(fut: &mut Pin<Box<F>>) -> Poll<F::Output> {
        let w = waker();
        let mut cx = Context::from_waker(&w);
        fut.as_mut().poll(&mut cx)
    }

    /// Yields to the executor once (the `await setTimeout(1)` of the TS tests).
    pub struct YieldNow(pub bool);
    impl Future for YieldNow {
        type Output = ();
        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            if self.0 {
                Poll::Ready(())
            } else {
                self.0 = true;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }
    }
}
