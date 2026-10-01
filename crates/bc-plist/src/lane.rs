//! Fractional ordering of list items in SQL (playlist items and DJ-set items).
//!
//! `position` is a FLOAT; an insert or a drag is one write (`bc_music::ordering`). When the gaps
//! collapse (or two rows share a position) the whole lane is renumbered, in its current order.

use bc_db::rusqlite::{Connection, params};
use bc_libcore::{ApiError, ApiResult};
use bc_music::ordering;

/// One ordered list table.
#[derive(Debug, Clone, Copy)]
pub struct Lane {
    pub table: &'static str,
    /// Column holding the owning list id.
    pub owner: &'static str,
}

/// `playlist_items(playlist_id, track_id, position)`.
pub const PLAYLIST: Lane = Lane { table: "playlist_items", owner: "playlist_id" };
/// `dj_set_items(set_id, track_id, position, ...)`.
pub const SET: Lane = Lane { table: "dj_set_items", owner: "set_id" };

impl Lane {
    /// `(item id, position)` of a list, in order (ties broken by id, so the order is stable).
    pub fn items(&self, c: &Connection, owner: i64) -> ApiResult<Vec<(i64, f64)>> {
        let sql = format!("SELECT id, position FROM {} WHERE {} = ?1 ORDER BY position, id", self.table, self.owner);
        let mut st = c.prepare_cached(&sql)?;
        let rows = st.query_map([owner], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Positions for `count` new items going in at `at_index` (clamped; `None` = append), in the
    /// order they were handed in. Appending is the bulk case (a whole shelf, a 14,000-track
    /// view): a running cursor, never a re-sort per track.
    pub fn plan_insert(existing: &[f64], count: usize, at_index: Option<i64>) -> Vec<f64> {
        let mut out = Vec::with_capacity(count);
        match at_index {
            None => {
                let mut last = existing.last().copied();
                for _ in 0..count {
                    let p = ordering::append_position(last);
                    out.push(p);
                    last = Some(p);
                }
            }
            Some(i) => {
                let index = usize::try_from(i.max(0)).unwrap_or(0).min(existing.len());
                let mut before = if index > 0 { Some(existing[index - 1]) } else { None };
                let after = existing.get(index).copied();
                for _ in 0..count {
                    let p = ordering::between(before, after).position;
                    out.push(p);
                    before = Some(p);
                }
            }
        }
        out
    }

    /// Collapse fractional gaps if repeated inserts exhausted the precision.
    pub fn renormalise_if_needed(&self, c: &Connection, owner: i64) -> ApiResult<()> {
        let items = self.items(c, owner)?;
        let positions: Vec<f64> = items.iter().map(|(_, p)| *p).collect();
        if !ordering::needs_renormalise(&positions) {
            return Ok(());
        }
        let sql = format!("UPDATE {} SET position = ?1 WHERE id = ?2", self.table);
        let mut st = c.prepare_cached(&sql)?;
        for ((id, _), p) in items.iter().zip(ordering::renormalise(items.len())) {
            st.execute(params![p, id])?;
        }
        Ok(())
    }

    /// Drag `item_id` so it ends up at `to_index` of the list WITHOUT it. Returns its new position.
    pub fn move_item(&self, c: &Connection, owner: i64, item_id: i64, to_index: i64) -> ApiResult<f64> {
        let items = self.items(c, owner)?;
        let Some(index) = items.iter().position(|(id, _)| *id == item_id) else {
            return Err(ApiError::not_found(format!("item {item_id} not in {} {owner}", self.label())));
        };
        let order: Vec<f64> = items.iter().map(|(_, p)| *p).collect();
        let to = usize::try_from(to_index.max(0)).unwrap_or(0);
        let placement = ordering::positions_for_move(&order, index, to);
        let sql = format!("UPDATE {} SET position = ?1 WHERE id = ?2", self.table);
        c.execute(&sql, params![placement.position, item_id])?;
        if placement.needs_renormalise {
            self.renormalise_if_needed(c, owner)?;
        }
        let sql = format!("SELECT position FROM {} WHERE id = ?1", self.table);
        Ok(c.query_row(&sql, [item_id], |r| r.get(0))?)
    }

    /// Does `item_id` belong to list `owner`?
    pub fn contains(&self, c: &Connection, owner: i64, item_id: i64) -> ApiResult<bool> {
        let sql = format!("SELECT 1 FROM {} WHERE id = ?1 AND {} = ?2", self.table, self.owner);
        Ok(c.query_row(&sql, params![item_id, owner], |_| Ok(())).is_ok())
    }

    fn label(&self) -> &'static str {
        if self.table == "playlist_items" { "playlist" } else { "set" }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appending_a_pile_keeps_order() {
        let p = Lane::plan_insert(&[1024.0, 2048.0], 3, None);
        assert_eq!(p, vec![3072.0, 4096.0, 5120.0]);
        assert_eq!(Lane::plan_insert(&[], 2, None), vec![1024.0, 2048.0]);
    }

    #[test]
    fn dropping_in_the_middle_keeps_the_batch_in_order() {
        let p = Lane::plan_insert(&[1024.0, 2048.0], 2, Some(1));
        assert!(p[0] > 1024.0 && p[0] < p[1] && p[1] < 2048.0);
        // clamped
        let at_end = Lane::plan_insert(&[1024.0], 1, Some(99));
        assert_eq!(at_end, vec![2048.0]);
        let head = Lane::plan_insert(&[1024.0], 1, Some(-5));
        assert_eq!(head, vec![512.0]);
    }
}
