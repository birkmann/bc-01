//! Exact port of CPython's `difflib.SequenceMatcher(None, a, b).ratio()`
//! (Ratcliff/Obershelp longest-matching-blocks, with the `autojunk` heuristic
//! that drops "popular" elements of `b` once it has 200 or more elements).
//!
//! The tracklist tests' expected scores are calibrated on difflib, so this is a
//! faithful port rather than `strsim`, whose metrics differ.

use std::collections::HashMap;

/// `SequenceMatcher(None, a, b).ratio()` over chars.
pub fn ratio(a: &str, b: &str) -> f64 {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let total = a.len() + b.len();
    if total == 0 {
        return 1.0;
    }
    let matches = Matcher::new(&a, &b).matching_total();
    2.0 * matches as f64 / total as f64
}

struct Matcher<'a> {
    a: &'a [char],
    b: &'a [char],
    b2j: HashMap<char, Vec<usize>>,
}

impl<'a> Matcher<'a> {
    fn new(a: &'a [char], b: &'a [char]) -> Self {
        let mut b2j: HashMap<char, Vec<usize>> = HashMap::new();
        for (i, c) in b.iter().enumerate() {
            b2j.entry(*c).or_default().push(i);
        }
        // autojunk: only for sequences of 200+ elements.
        let n = b.len();
        if n >= 200 {
            let ntest = n / 100 + 1;
            b2j.retain(|_, idxs| idxs.len() <= ntest);
        }
        Self { a, b, b2j }
    }

    fn find_longest_match(&self, alo: usize, ahi: usize, blo: usize, bhi: usize) -> (usize, usize, usize) {
        let (mut besti, mut bestj, mut bestsize) = (alo, blo, 0usize);
        let mut j2len: HashMap<usize, usize> = HashMap::new();
        for i in alo..ahi {
            let mut newj2len: HashMap<usize, usize> = HashMap::new();
            if let Some(idxs) = self.b2j.get(&self.a[i]) {
                for &j in idxs {
                    if j < blo {
                        continue;
                    }
                    if j >= bhi {
                        break;
                    }
                    let prev = if j > 0 { j2len.get(&(j - 1)).copied().unwrap_or(0) } else { 0 };
                    let k = prev + 1;
                    newj2len.insert(j, k);
                    if k > bestsize {
                        besti = i + 1 - k;
                        bestj = j + 1 - k;
                        bestsize = k;
                    }
                }
            }
            j2len = newj2len;
        }
        // Extend over elements the autojunk heuristic dropped from b2j.
        while besti > alo && bestj > blo && self.a[besti - 1] == self.b[bestj - 1] {
            besti -= 1;
            bestj -= 1;
            bestsize += 1;
        }
        while besti + bestsize < ahi
            && bestj + bestsize < bhi
            && self.a[besti + bestsize] == self.b[bestj + bestsize]
        {
            bestsize += 1;
        }
        (besti, bestj, bestsize)
    }

    fn matching_total(&self) -> usize {
        let mut queue = vec![(0usize, self.a.len(), 0usize, self.b.len())];
        let mut total = 0;
        while let Some((alo, ahi, blo, bhi)) = queue.pop() {
            let (i, j, k) = self.find_longest_match(alo, ahi, blo, bhi);
            if k > 0 {
                total += k;
                if alo < i && blo < j {
                    queue.push((alo, i, blo, j));
                }
                if i + k < ahi && j + k < bhi {
                    queue.push((i + k, ahi, j + k, bhi));
                }
            }
        }
        total
    }
}

#[cfg(test)]
mod tests {
    use super::ratio;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn known_cpython_values() {
        // difflib docs: SequenceMatcher(None, "abcd", "bcde").ratio() == 0.75
        assert!(close(ratio("abcd", "bcde"), 0.75));
        // "private Thread" example: ratio("pythonic", "python") = 2*6/14
        assert!(close(ratio("pythonic", "python"), 12.0 / 14.0));
        assert!(close(ratio("", ""), 1.0));
        assert!(close(ratio("abc", ""), 0.0));
        assert!(close(ratio("pinto nyc", "pinto"), 10.0 / 14.0));
        // classic Ratcliff/Obershelp pair
        assert!(close(ratio("WIKIMEDIA", "WIKIMANIA"), 14.0 / 18.0));
    }

    #[test]
    fn autojunk_applies_from_200_elements() {
        // 'a' is popular in b (>= 200 long, > n/100+1 occurrences) so it is dropped
        // from the index; the match then only grows through the extension loops.
        let a = "a".repeat(10);
        let b = "a".repeat(200);
        // longest match found via extension from (alo, blo) = (0, 0): 10 chars
        assert!(close(ratio(&a, &b), 20.0 / 210.0));
    }
}
