//! Fractional ordering for playlists and DJ sets (port of `services/playlists/ordering.py`).
//!
//! Positions are f64; placing an item between neighbours is `(a + b) / 2`, so a drag is one
//! UPDATE. A renormalisation pass runs when any gap drops below [`MIN_GAP`].

pub const STEP: f64 = 1024.0;
pub const MIN_GAP: f64 = 1e-6;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placement {
    pub position: f64,
    pub needs_renormalise: bool,
}

fn p(position: f64) -> Placement {
    Placement { position, needs_renormalise: false }
}

pub fn append_position(last: Option<f64>) -> f64 {
    last.map_or(STEP, |l| l + STEP)
}

/// Position for an item dropped between two neighbours.
pub fn between(before: Option<f64>, after: Option<f64>) -> Placement {
    match (before, after) {
        (None, None) => p(STEP),
        (None, Some(a)) => p(if a > MIN_GAP { a / 2.0 } else { -STEP }),
        (Some(b), None) => p(b + STEP),
        (Some(b), Some(a)) => {
            let gap = a - b;
            if gap <= MIN_GAP {
                Placement { position: (b + a) / 2.0, needs_renormalise: true }
            } else {
                p(b + gap / 2.0)
            }
        }
    }
}

/// Where an item lands when dragged from `from_index` to `to_index`. `order` is the current
/// position list INCLUDING the moving item; `to_index` indexes the list WITHOUT it.
pub fn positions_for_move(order: &[f64], from_index: usize, to_index: usize) -> Placement {
    let remaining: Vec<f64> =
        order.iter().enumerate().filter(|(i, _)| *i != from_index).map(|(_, v)| *v).collect();
    let target = to_index.min(remaining.len());
    let before = if target > 0 { Some(remaining[target - 1]) } else { None };
    let after = remaining.get(target).copied();
    between(before, after)
}

/// Evenly spaced positions for `count` items.
pub fn renormalise(count: usize) -> Vec<f64> {
    (0..count).map(|i| STEP * (i as f64 + 1.0)).collect()
}

pub fn needs_renormalise(positions: &[f64]) -> bool {
    let tight = positions.windows(2).any(|w| w[1] - w[0] <= MIN_GAP);
    if tight {
        return true;
    }
    let mut bits: Vec<u64> = positions.iter().map(|v| v.to_bits()).collect();
    bits.sort_unstable();
    bits.windows(2).any(|w| w[0] == w[1])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_walks_forward() {
        assert_eq!(append_position(None), STEP);
        assert_eq!(append_position(Some(1024.0)), 2048.0);
    }

    #[test]
    fn between_takes_the_midpoint() {
        assert_eq!(between(Some(1000.0), Some(2000.0)).position, 1500.0);
    }

    #[test]
    fn between_handles_the_ends() {
        assert_eq!(between(None, Some(1000.0)).position, 500.0);
        assert_eq!(between(Some(1000.0), None).position, 1000.0 + STEP);
        assert_eq!(between(None, None).position, STEP);
    }

    #[test]
    fn reordering_is_a_single_position_change() {
        let order = [1024.0, 2048.0, 3072.0, 4096.0];
        let pl = positions_for_move(&order, 0, 2);
        assert!(3072.0 < pl.position && pl.position < 4096.0);
        assert!(!pl.needs_renormalise);
    }

    #[test]
    fn reordering_lands_where_it_was_dropped() {
        let positions = [1024.0, 2048.0, 3072.0, 4096.0];
        let labels = ["a", "b", "c", "d"];
        let pl = positions_for_move(&positions, 0, 2);
        let mut moved: Vec<(f64, &str)> = positions
            .iter()
            .zip(labels)
            .enumerate()
            .map(|(i, (pos, l))| (if i != 0 { *pos } else { pl.position }, l))
            .collect();
        moved.sort_by(|a, b| a.0.total_cmp(&b.0));
        let order: Vec<_> = moved.iter().map(|m| m.1).collect();
        assert_eq!(order, ["b", "c", "a", "d"]);
    }

    #[test]
    fn move_to_the_start_and_end() {
        let order = [1024.0, 2048.0, 3072.0];
        assert!(positions_for_move(&order, 2, 0).position < 1024.0);
        assert!(positions_for_move(&order, 0, 99).position > 3072.0);
    }

    #[test]
    fn repeated_midpoint_inserts_eventually_demand_renormalisation() {
        let (low, high) = (1000.0, 1000.0 + MIN_GAP / 2.0);
        assert!(between(Some(low), Some(high)).needs_renormalise);
    }

    #[test]
    fn needs_renormalise_detects_collisions_and_gaps() {
        assert!(needs_renormalise(&[1.0, 1.0]));
        assert!(needs_renormalise(&[1.0, 1.0 + 1e-9]));
        assert!(!needs_renormalise(&[1024.0, 2048.0]));
    }

    #[test]
    fn renormalise_spaces_evenly() {
        let fresh = renormalise(4);
        assert_eq!(fresh, [1024.0, 2048.0, 3072.0, 4096.0]);
        assert!(!needs_renormalise(&fresh));
    }
}
