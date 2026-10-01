//! A counting semaphore for futures: at most `size` holders at once, the rest wait their turn in
//! the order they asked. Single-threaded (`Rc`), which is what wasm32 and the Leptos UI use.
//!
//! Ports `gate.ts`: [`Gate::acquire`] = `acquire()` (resolves with a [`Release`]),
//! [`Gate::held`] / [`Gate::waiting`] = the `held` / `waiting` getters. Releasing twice is a
//! no-op, and dropping a [`Release`] also releases it.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

#[derive(Default)]
struct Waiter {
    granted: Cell<bool>,
    cancelled: Cell<bool>,
    waker: RefCell<Option<Waker>>,
}

struct Inner {
    size: usize,
    active: Cell<usize>,
    queue: RefCell<VecDeque<Rc<Waiter>>>,
}

impl Inner {
    fn release_slot(&self) {
        self.active.set(self.active.get().saturating_sub(1));
        loop {
            let next = self.queue.borrow_mut().pop_front();
            match next {
                None => return,
                Some(w) if w.cancelled.get() => continue,
                Some(w) => {
                    self.active.set(self.active.get() + 1);
                    w.granted.set(true);
                    if let Some(waker) = w.waker.borrow_mut().take() {
                        waker.wake();
                    }
                    return;
                }
            }
        }
    }
}

#[derive(Clone)]
pub struct Gate(Rc<Inner>);

impl Gate {
    pub fn new(size: usize) -> Self {
        Gate(Rc::new(Inner {
            size,
            active: Cell::new(0),
            queue: RefCell::new(VecDeque::new()),
        }))
    }

    /// Resolves with the release once a slot is free.
    pub fn acquire(&self) -> Acquire {
        Acquire { gate: self.0.clone(), waiter: None }
    }

    /// Slots currently held.
    pub fn held(&self) -> usize {
        self.0.active.get()
    }

    /// Callers queued for a slot.
    pub fn waiting(&self) -> usize {
        self.0.queue.borrow().iter().filter(|w| !w.cancelled.get()).count()
    }
}

/// A held slot. [`Release::release`] (or drop) frees it; further calls are no-ops.
pub struct Release {
    gate: Rc<Inner>,
    released: Cell<bool>,
}

impl Release {
    pub fn release(&self) {
        if self.released.replace(true) {
            return;
        }
        self.gate.release_slot();
    }
}

impl Drop for Release {
    fn drop(&mut self) {
        self.release();
    }
}

pub struct Acquire {
    gate: Rc<Inner>,
    waiter: Option<Rc<Waiter>>,
}

impl Acquire {
    fn grant(&mut self) -> Release {
        self.waiter = None;
        Release { gate: self.gate.clone(), released: Cell::new(false) }
    }
}

impl Future for Acquire {
    type Output = Release;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Release> {
        if let Some(w) = self.waiter.clone() {
            if w.granted.get() {
                return Poll::Ready(self.grant());
            }
            *w.waker.borrow_mut() = Some(cx.waker().clone());
            return Poll::Pending;
        }
        if self.gate.active.get() < self.gate.size {
            self.gate.active.set(self.gate.active.get() + 1);
            return Poll::Ready(self.grant());
        }
        let w = Rc::new(Waiter::default());
        *w.waker.borrow_mut() = Some(cx.waker().clone());
        self.gate.queue.borrow_mut().push_back(w.clone());
        self.waiter = Some(w);
        Poll::Pending
    }
}

impl Drop for Acquire {
    fn drop(&mut self) {
        if let Some(w) = self.waiter.take() {
            if w.granted.get() {
                // Granted but never collected: hand the slot on.
                self.gate.release_slot();
            } else {
                w.cancelled.set(true);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bandcamp::logic::testutil::poll_once;

    type Fut = Pin<Box<Acquire>>;

    fn ready(f: &mut Fut) -> Option<Release> {
        match poll_once(f) {
            Poll::Ready(r) => Some(r),
            Poll::Pending => None,
        }
    }

    #[test]
    fn admits_size_at_once_and_the_rest_in_order_as_slots_free_up() {
        let gate = Gate::new(2);
        let mut order: Vec<String> = Vec::new();

        let mut fa: Fut = Box::pin(gate.acquire());
        let a = ready(&mut fa).expect("a");
        order.push("in:a".into());
        let mut fb: Fut = Box::pin(gate.acquire());
        let b = ready(&mut fb).expect("b");
        order.push("in:b".into());
        let mut fc: Fut = Box::pin(gate.acquire());
        let mut fd: Fut = Box::pin(gate.acquire());
        assert!(ready(&mut fc).is_none());
        assert!(ready(&mut fd).is_none());
        assert_eq!(order, ["in:a", "in:b"]);
        assert_eq!(gate.held(), 2);
        assert_eq!(gate.waiting(), 2);

        b.release();
        let c = ready(&mut fc).expect("c");
        order.push("in:c".into());
        assert_eq!(order, ["in:a", "in:b", "in:c"]);
        assert_eq!(gate.waiting(), 1);
        assert!(ready(&mut fd).is_none());

        // Releasing twice must not open a second slot.
        b.release();
        assert!(ready(&mut fd).is_none());
        assert_eq!(gate.held(), 2);

        a.release();
        let d = ready(&mut fd).expect("d");
        order.push("in:d".into());
        assert_eq!(order, ["in:a", "in:b", "in:c", "in:d"]);
        c.release();
        d.release();
        assert_eq!(gate.held(), 0);
        assert_eq!(gate.waiting(), 0);
    }

    #[test]
    fn a_dropped_waiter_does_not_keep_its_place_in_line() {
        let gate = Gate::new(1);
        let mut f1: Fut = Box::pin(gate.acquire());
        let r1 = ready(&mut f1).expect("1");
        let mut f2: Fut = Box::pin(gate.acquire());
        let mut f3: Fut = Box::pin(gate.acquire());
        assert!(ready(&mut f2).is_none());
        assert!(ready(&mut f3).is_none());
        drop(f2);
        assert_eq!(gate.waiting(), 1);
        r1.release();
        assert!(ready(&mut f3).is_some());
    }
}
