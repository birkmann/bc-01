//! Selection as a tagged union (`none | include(ids) | all(filter, excluded)`),
//! with Shift-range support. Bulk mutations send `{ids}` or `{filter, exclude}`.
use bc_types::ui::Selection;

pub trait SelectionOps {
    fn is_selected(&self, id: i64) -> bool;
    fn count(&self, total: i64) -> i64;
    fn toggle(&mut self, id: i64);
    fn add_many(&mut self, ids: &[i64]);
    fn select_all(&mut self, filter: serde_json::Value);
    fn clear(&mut self);
    fn is_empty(&self) -> bool;
    /// Body fragment for a bulk request.
    fn to_request(&self) -> serde_json::Value;
}

impl SelectionOps for Selection {
    fn is_selected(&self, id: i64) -> bool {
        match self {
            Selection::None => false,
            Selection::Include { ids } => ids.contains(&id),
            Selection::All { excluded, .. } => !excluded.contains(&id),
        }
    }
    fn count(&self, total: i64) -> i64 {
        match self {
            Selection::None => 0,
            Selection::Include { ids } => ids.len() as i64,
            Selection::All { excluded, .. } => (total - excluded.len() as i64).max(0),
        }
    }
    fn toggle(&mut self, id: i64) {
        match self {
            Selection::None => *self = Selection::Include { ids: vec![id] },
            Selection::Include { ids } => {
                if let Some(p) = ids.iter().position(|i| *i == id) {
                    ids.remove(p);
                    if ids.is_empty() {
                        *self = Selection::None;
                    }
                } else {
                    ids.push(id);
                }
            }
            Selection::All { excluded, .. } => {
                if let Some(p) = excluded.iter().position(|i| *i == id) {
                    excluded.remove(p);
                } else {
                    excluded.push(id);
                }
            }
        }
    }
    fn add_many(&mut self, new: &[i64]) {
        match self {
            Selection::None => *self = Selection::Include { ids: new.to_vec() },
            Selection::Include { ids } => {
                for n in new {
                    if !ids.contains(n) {
                        ids.push(*n);
                    }
                }
            }
            Selection::All { excluded, .. } => excluded.retain(|e| !new.contains(e)),
        }
    }
    fn select_all(&mut self, filter: serde_json::Value) {
        *self = Selection::All { filter, excluded: vec![] };
    }
    fn clear(&mut self) {
        *self = Selection::None;
    }
    fn is_empty(&self) -> bool {
        matches!(self, Selection::None)
    }
    fn to_request(&self) -> serde_json::Value {
        match self {
            Selection::None => serde_json::json!({ "ids": [] }),
            Selection::Include { ids } => serde_json::json!({ "ids": ids }),
            Selection::All { filter, excluded } => serde_json::json!({ "filter": filter, "exclude": excluded }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn include_toggle_and_count() {
        let mut s = Selection::None;
        s.toggle(5);
        s.toggle(6);
        assert!(s.is_selected(5));
        assert_eq!(s.count(1000), 2);
        s.toggle(5);
        s.toggle(6);
        assert!(s.is_empty());
    }

    #[test]
    fn all_with_exclusions_scales_to_190k() {
        let mut s = Selection::None;
        s.select_all(serde_json::json!({"q":"dub"}));
        assert_eq!(s.count(190_000), 190_000);
        s.toggle(42);
        assert!(!s.is_selected(42));
        assert!(s.is_selected(43));
        assert_eq!(s.count(190_000), 189_999);
        assert_eq!(s.to_request(), serde_json::json!({"filter":{"q":"dub"},"exclude":[42]}));
        s.toggle(42);
        assert!(s.is_selected(42));
    }

    #[test]
    fn add_many_merges_without_duplicates() {
        let mut s = Selection::Include { ids: vec![1, 2] };
        s.add_many(&[2, 3]);
        assert_eq!(s, Selection::Include { ids: vec![1, 2, 3] });
    }
}
