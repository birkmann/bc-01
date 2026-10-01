//! Shift-click ranges over a listing (port of `rangeSelect.ts`).

/// Where a shift-click ranges from and the selection that click started out of.
#[derive(Debug, Clone, PartialEq)]
pub struct RangeAnchor<K, P> {
    pub key: K,
    pub base: P,
}

/// The run of `list` between two keys, in list order, whichever end was
/// clicked first. `None` when either end is no longer in the list.
pub fn run_between<'a, T, K: PartialEq>(
    list: &'a [T],
    key_of: impl Fn(&T) -> K,
    from: &K,
    to: &K,
) -> Option<&'a [T]> {
    let a = list.iter().position(|i| key_of(i) == *from)?;
    let b = list.iter().position(|i| key_of(i) == *to)?;
    Some(if a <= b { &list[a..=b] } else { &list[b..=a] })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone)]
    struct Item {
        id: i64,
    }
    fn list() -> Vec<Item> {
        (1..=4).map(|id| Item { id }).collect()
    }
    fn run(from: i64, to: i64) -> Option<Vec<i64>> {
        let l = list();
        run_between(&l, |i| i.id, &from, &to).map(|s| s.iter().map(|i| i.id).collect())
    }

    #[test]
    fn takes_the_run_in_list_order_whichever_end_was_clicked_first() {
        assert_eq!(run(2, 4), Some(vec![2, 3, 4]));
        assert_eq!(run(4, 2), Some(vec![2, 3, 4]));
    }
    #[test]
    fn a_range_of_one_is_the_item_itself() {
        assert_eq!(run(3, 3), Some(vec![3]));
    }
    #[test]
    fn gives_nothing_when_an_end_is_no_longer_on_show() {
        assert_eq!(run(9, 2), None);
        assert_eq!(run(2, 9), None);
    }
}
