//! Editing the upcoming part of the queue without breaking the played chain.
//!
//! Port of `web/frontend/src/player/queueOps.ts`. `history` holds queue
//! *indices*: the rows actually played, in order, with `history_pos` on the
//! current one. Inserting, moving or removing a row shifts the indices of
//! everything after it, so each edit remaps every history entry -- including
//! forward-branch entries left behind by `jump_to` + `previous`. Only rows
//! after the current one may be edited; the current row and everything played
//! stay put, which keeps `queue_index` and the entry at `history_pos` valid.

#[derive(Debug, Clone, PartialEq)]
pub struct QueueShape<T> {
    pub queue: Vec<T>,
    /// -1 when nothing is current.
    pub queue_index: i64,
    pub history: Vec<usize>,
    pub history_pos: i64,
}

impl<T: Clone> QueueShape<T> {
    fn lo(&self) -> usize {
        (self.queue_index + 1).max(0) as usize
    }
}

/// Insert `items` before `at`. `at` is clamped to the upcoming range.
pub fn insert_ops<T: Clone>(s: &QueueShape<T>, at: usize, items: &[T]) -> Option<QueueShape<T>> {
    if items.is_empty() {
        return None;
    }
    let pos = at.max(s.lo()).min(s.queue.len());
    let k = items.len();
    let mut queue = Vec::with_capacity(s.queue.len() + k);
    queue.extend_from_slice(&s.queue[..pos]);
    queue.extend_from_slice(items);
    queue.extend_from_slice(&s.queue[pos..]);
    Some(QueueShape {
        queue,
        queue_index: s.queue_index,
        history: s.history.iter().map(|&h| if h >= pos { h + k } else { h }).collect(),
        history_pos: s.history_pos,
    })
}

/// Remove rows `[from, to)`. Both must lie after the current row; `None` (no-op) otherwise.
pub fn remove_ops<T: Clone>(s: &QueueShape<T>, from: usize, to: usize) -> Option<QueueShape<T>> {
    let start = from.max(s.lo());
    let end = to.min(s.queue.len());
    if start >= end {
        return None;
    }
    let n = end - start;
    let mut history = Vec::with_capacity(s.history.len());
    let mut drop_before = 0i64;
    for (i, &h) in s.history.iter().enumerate() {
        if h >= start && h < end {
            if i as i64 <= s.history_pos {
                drop_before += 1;
            }
            continue;
        }
        history.push(if h >= end { h - n } else { h });
    }
    let mut queue = Vec::with_capacity(s.queue.len() - n);
    queue.extend_from_slice(&s.queue[..start]);
    queue.extend_from_slice(&s.queue[end..]);
    Some(QueueShape { queue, queue_index: s.queue_index, history, history_pos: s.history_pos - drop_before })
}

/// Move the row at `from` so it ends up at index `to` (its final position).
/// Both must be upcoming rows; `None` (no-op) otherwise.
pub fn move_ops<T: Clone>(s: &QueueShape<T>, from: usize, to: usize) -> Option<QueueShape<T>> {
    let lo = s.lo();
    if s.queue.is_empty() {
        return None;
    }
    let hi = s.queue.len() - 1;
    if from < lo || from > hi || to < lo || to > hi || from == to {
        return None;
    }
    let item = s.queue[from].clone();
    let mut rest: Vec<T> = Vec::with_capacity(s.queue.len());
    rest.extend_from_slice(&s.queue[..from]);
    rest.extend_from_slice(&s.queue[from + 1..]);
    let mut queue = Vec::with_capacity(s.queue.len());
    queue.extend_from_slice(&rest[..to]);
    queue.push(item);
    queue.extend_from_slice(&rest[to..]);
    let remap = |i: usize| -> usize {
        if i == from {
            to
        } else if from < to {
            if i > from && i <= to { i - 1 } else { i }
        } else if i >= to && i < from {
            i + 1
        } else {
            i
        }
    };
    Some(QueueShape {
        queue,
        queue_index: s.queue_index,
        history: s.history.iter().map(|&h| remap(h)).collect(),
        history_pos: s.history_pos,
    })
}

/// Replace the row at `index` in place; history is untouched.
pub fn replace_ops<T: Clone>(s: &QueueShape<T>, index: usize, item: T) -> Option<QueueShape<T>> {
    if (index as i64) <= s.queue_index || index >= s.queue.len() {
        return None;
    }
    let mut queue = s.queue.clone();
    queue[index] = item;
    Some(QueueShape { queue, ..s.clone() })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> QueueShape<char> {
        QueueShape { queue: "abcde".chars().collect(), queue_index: 1, history: vec![0, 1], history_pos: 1 }
    }
    fn s(q: &QueueShape<char>) -> String {
        q.queue.iter().collect()
    }

    #[test]
    fn insert_after_current_and_shifts_later_history() {
        let b = QueueShape { history: vec![0, 3, 1], history_pos: 2, ..base() };
        let r = insert_ops(&b, 2, &['x', 'y']).unwrap();
        assert_eq!(s(&r), "abxycde");
        assert_eq!(r.history, vec![0, 5, 1]);
    }

    #[test]
    fn insert_never_goes_into_the_played_part() {
        assert_eq!(s(&insert_ops(&base(), 0, &['x']).unwrap()), "abxcde");
    }

    #[test]
    fn remove_drops_range_and_repoints_the_rest() {
        let b = QueueShape { history: vec![0, 3, 1], history_pos: 2, ..base() };
        let r = remove_ops(&b, 2, 4).unwrap();
        assert_eq!(s(&r), "abe");
        assert_eq!(r.history, vec![0, 1]);
        assert_eq!(r.history_pos, 1);
    }

    #[test]
    fn remove_walks_history_pos_back_when_a_dropped_entry_was_before_it() {
        let b = QueueShape { history: vec![3, 1], history_pos: 1, ..base() };
        let r = remove_ops(&b, 3, 4).unwrap();
        assert_eq!(s(&r), "abce");
        assert_eq!(r.history, vec![1]);
        assert_eq!(r.history_pos, 0);
    }

    #[test]
    fn remove_refuses_the_current_row() {
        assert!(remove_ops(&base(), 1, 2).is_none());
    }

    #[test]
    fn move_forward_and_back() {
        assert_eq!(s(&move_ops(&base(), 2, 4).unwrap()), "abdec");
        assert_eq!(s(&move_ops(&base(), 4, 2).unwrap()), "abecd");
    }

    #[test]
    fn move_remaps_history() {
        let b = QueueShape { history: vec![4, 1], history_pos: 1, ..base() };
        let r = move_ops(&b, 4, 2).unwrap();
        assert_eq!(s(&r), "abecd");
        assert_eq!(r.history, vec![2, 1]);
    }

    #[test]
    fn move_is_a_noop_outside_the_upcoming_range() {
        assert!(move_ops(&base(), 0, 3).is_none());
        assert!(move_ops(&base(), 3, 1).is_none());
    }

    #[test]
    fn replace_swaps_a_row_and_leaves_history_alone() {
        let b = base();
        let r = replace_ops(&b, 3, 'z').unwrap();
        assert_eq!(s(&r), "abcze");
        assert_eq!(r.history, b.history);
        assert!(replace_ops(&b, 1, 'z').is_none());
    }
}
