//! Pure time arithmetic for the plan panel (port of `plan/timing.ts`).
use bc_types::player::QueueItem;

/// The upcoming rows within the planning horizon, with their queue indices.
pub fn upcoming(queue: &[QueueItem], queue_index: i64, horizon: usize) -> Vec<(usize, &QueueItem)> {
    let start = (queue_index + 1).max(0) as usize;
    let end = queue.len().min(start + horizon);
    (start..end).filter_map(|i| queue.get(i).map(|t| (i, t))).collect()
}

/// How long a track plays when it starts at `start_s` and is moved on after `max_play_s`
/// (None = when it ends). A track of unknown length counts for nothing.
pub fn effective_length_ms(duration_ms: Option<i64>, start_s: f64, max_play_s: Option<f64>) -> i64 {
    let Some(d) = duration_ms else { return 0 };
    let left = (d as f64 - start_s * 1000.0).max(0.0);
    match max_play_s {
        None => left as i64,
        Some(m) => left.min(m * 1000.0) as i64,
    }
}

/// Play length of a row from the engine's own entry/exit points (`PlayerState.entry_points`):
/// it starts at `drop_s`, and is moved on at `out_s` (or its end), capped by the pace limit.
pub fn entry_length_ms(duration_ms: Option<i64>, drop_s: f64, out_s: Option<f64>, max_play_s: Option<f64>) -> i64 {
    let Some(d) = duration_ms else { return 0 };
    let end = out_s.map(|o| o * 1000.0).unwrap_or(d as f64).min(d as f64);
    let left = (end - drop_s * 1000.0).max(0.0);
    match max_play_s {
        None => left as i64,
        Some(m) => left.min(m * 1000.0) as i64,
    }
}

/// When each upcoming row starts, as an offset from now: the current track's remainder, then
/// each row's play length in turn.
pub fn plays_at_offsets(current_remaining_ms: i64, lengths: &[i64]) -> Vec<i64> {
    let mut t = current_remaining_ms.max(0);
    let mut out = Vec::with_capacity(lengths.len());
    for l in lengths {
        out.push(t);
        t += l;
    }
    out
}

/// Everything queued from now to the end of the last planned row.
pub fn planned_ms(current_remaining_ms: i64, lengths: &[i64]) -> i64 {
    lengths.iter().sum::<i64>() + current_remaining_ms.max(0)
}

/// Time left on the set clock, or None when no set is running.
pub fn remaining_ms(started_at_ms: Option<u64>, length_min: Option<f64>, now_ms: f64) -> Option<f64> {
    let (s, l) = (started_at_ms?, length_min?);
    Some(s as f64 + l * 60_000.0 - now_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(n: usize) -> Vec<QueueItem> {
        (0..n).map(|i| QueueItem { track_id: i as i64 + 1, ..Default::default() }).collect()
    }

    #[test]
    fn upcoming_is_bounded_by_horizon_and_queue() {
        let queue = q(30);
        assert_eq!(upcoming(&queue, 0, 20).len(), 20);
        assert_eq!(upcoming(&queue, 0, 20)[0].0, 1);
        assert_eq!(upcoming(&queue, 25, 20).len(), 4);
        assert!(upcoming(&queue, 29, 20).is_empty());
        assert_eq!(upcoming(&queue, -1, 3).iter().map(|r| r.0).collect::<Vec<_>>(), vec![0, 1, 2]);
    }

    #[test]
    fn effective_length_caps_and_counts_nothing_for_unknown() {
        assert_eq!(effective_length_ms(Some(300_000), 0.0, None), 300_000);
        assert_eq!(effective_length_ms(Some(300_000), 60.0, None), 240_000);
        assert_eq!(effective_length_ms(Some(300_000), 0.0, Some(120.0)), 120_000);
        assert_eq!(effective_length_ms(Some(60_000), 90.0, None), 0);
        assert_eq!(effective_length_ms(None, 0.0, Some(120.0)), 0);
    }

    #[test]
    fn entry_points_shorten_the_play_length() {
        assert_eq!(entry_length_ms(Some(300_000), 20.0, Some(200.0), None), 180_000);
        assert_eq!(entry_length_ms(Some(300_000), 0.0, None, Some(100.0)), 100_000);
        assert_eq!(entry_length_ms(Some(300_000), 0.0, Some(400.0), None), 300_000);
        assert_eq!(entry_length_ms(None, 0.0, None, None), 0);
    }

    #[test]
    fn offsets_start_after_the_current_remainder() {
        assert_eq!(plays_at_offsets(10_000, &[100_000, 200_000, 50_000]), vec![10_000, 110_000, 310_000]);
        assert_eq!(plays_at_offsets(-5, &[1]), vec![0]);
        assert_eq!(planned_ms(10_000, &[100_000, 200_000]), 310_000);
        assert_eq!(planned_ms(0, &[]), 0);
    }

    #[test]
    fn set_clock_remaining() {
        assert_eq!(remaining_ms(None, Some(60.0), 0.0), None);
        assert_eq!(remaining_ms(Some(1_000), None, 0.0), None);
        assert_eq!(remaining_ms(Some(1_000), Some(1.0), 31_000.0), Some(30_000.0));
        assert_eq!(remaining_ms(Some(1_000), Some(1.0), 100_000.0), Some(-39_000.0));
    }
}
