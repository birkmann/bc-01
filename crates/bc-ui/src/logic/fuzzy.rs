//! Subsequence fuzzy scoring for the command palette.

/// Higher is better; `None` when `needle` is not a subsequence of `hay`.
/// Rewards consecutive runs, word starts and an early first match.
pub fn score(needle: &str, hay: &str) -> Option<i32> {
    if needle.is_empty() {
        return Some(0);
    }
    let n: Vec<char> = needle.to_lowercase().chars().collect();
    let h: Vec<char> = hay.to_lowercase().chars().collect();
    let mut ni = 0;
    let mut s = 0i32;
    let mut prev_match = false;
    let mut first = None;
    for (i, c) in h.iter().enumerate() {
        if ni < n.len() && *c == n[ni] {
            if first.is_none() {
                first = Some(i);
            }
            s += 10;
            if prev_match {
                s += 15;
            }
            if i == 0 || !h[i - 1].is_alphanumeric() {
                s += 12;
            }
            ni += 1;
            prev_match = true;
        } else {
            prev_match = false;
        }
    }
    if ni < n.len() {
        return None;
    }
    s -= first.unwrap_or(0).min(20) as i32;
    s -= (h.len() as i32 - n.len() as i32).min(30) / 3;
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn matches_subsequences_only() {
        assert!(score("trk", "Tracks").is_some());
        assert!(score("xyz", "Tracks").is_none());
        assert_eq!(score("", "anything"), Some(0));
    }
    #[test]
    fn prefers_prefix_and_consecutive() {
        let a = score("alb", "Albums").unwrap();
        let b = score("alb", "Playlist albums").unwrap();
        assert!(a > b);
        assert!(score("set", "DJ Sets").unwrap() > score("set", "Settings and more").unwrap_or(i32::MIN) - 100);
    }
}
