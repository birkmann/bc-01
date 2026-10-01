//! `/history/*`: record plays, top charts, recent, reset.

use bc_db::rusqlite::{OptionalExtension, params};
use bc_db::util::now_db;
use bc_libcore::{ApiError, ApiResult, Ctx, Scope, hydrate};
use bc_types::library::*;
use bc_db::rusqlite::Connection;

/// Record a play inside the caller's transaction (history row + counters). For WS4's engine too.
pub fn record_play_tx(t: &bc_db::rusqlite::Transaction<'_>, ev: &PlayEvent) -> ApiResult<()> {
    let id = ev.track_id;
    let exists = t.query_row("SELECT 1 FROM tracks WHERE id = ?1", [id], |_| Ok(())).optional()?.is_some();
    if !exists {
        return Err(ApiError::not_found(format!("track {id} not found")));
    }
    let now = now_db();
    t.execute(
        "INSERT INTO play_history (track_id, started_at, ms_played, completed, skipped, source) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![id, now, ev.ms_played, ev.completed, ev.skipped, ev.source.clone().unwrap_or_else(|| "library".into())],
    )?;
    if ev.completed {
        t.execute("UPDATE tracks SET play_count = play_count + 1, last_played_at = ?2 WHERE id = ?1", params![id, now])?;
    } else if ev.skipped {
        t.execute("UPDATE tracks SET skip_count = skip_count + 1 WHERE id = ?1", params![id])?;
    }
    Ok(())
}

/// Plain blocking function: `record_play(&db, track_id, ms_played, completed, skipped)`.
pub fn record_play_db(db: &bc_db::Db, track_id: i64, ms_played: i64, completed: bool, skipped: bool) -> ApiResult<()> {
    let ev = PlayEvent { track_id, ms_played, completed, skipped, source: None };
    db.write_with::<(), ApiError>(move |t| record_play_tx(t, &ev))
}

pub fn record_play(ctx: &Ctx, ev: PlayEvent) -> ApiResult<()> {
    let id = ev.track_id;
    ctx.write(move |t| record_play_tx(t, &ev))?;
    ctx.bus.invalidate("track", vec![id]);
    Ok(())
}

fn cutoff(days: i64) -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0) - days * 86_400;
    bc_db::util::db_from_unix(secs)
}

/// The chart of the last `days` days from play history (completed plays only: a skip is not a listen).
pub fn top(c: &Connection, days: i64, limit: i64, scope: &Scope) -> ApiResult<HistoryTop> {
    let days = days.clamp(1, 365);
    let limit = limit.clamp(1, 50);
    let sql = format!(
        "SELECT h.track_id, COUNT(*) AS n FROM play_history h JOIN tracks t ON t.id = h.track_id
          WHERE h.started_at >= ?1 AND h.completed = 1{} GROUP BY h.track_id ORDER BY n DESC, h.track_id LIMIT {limit}",
        scope.and_track("t")
    );
    let mut st = c.prepare(&sql)?;
    let rows: Vec<(i64, i64)> = st.query_map([cutoff(days)], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?;
    let ids: Vec<i64> = rows.iter().map(|r| r.0).collect();
    let mut tracks = hydrate::tracks_out(c, &ids)?;
    let mut items = Vec::new();
    for (id, plays) in rows {
        if let Some(pos) = tracks.iter().position(|t| t.id == id) {
            items.push(HistoryTopItem { track: tracks.swap_remove(pos), plays });
        }
    }
    Ok(HistoryTop { days, items })
}

/// Recently played, newest first.
pub fn recent(c: &Connection, limit: i64, scope: &Scope) -> ApiResult<Vec<HistoryEntry>> {
    let limit = limit.clamp(1, 200);
    let sql = format!(
        "SELECT h.id, h.track_id, h.started_at, h.ms_played, h.completed, h.skipped, h.source FROM play_history h JOIN tracks t ON t.id = h.track_id
          WHERE 1=1{} ORDER BY h.started_at DESC, h.id DESC LIMIT {limit}",
        scope.and_track("t")
    );
    let mut st = c.prepare(&sql)?;
    let rows: Vec<(i64, i64, String, i64, bool, bool, String)> =
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)))?.collect::<Result<_, _>>()?;
    let ids: Vec<i64> = rows.iter().map(|r| r.1).collect();
    let tracks = hydrate::tracks_out(c, &ids)?;
    let by: std::collections::HashMap<i64, TrackOut> = tracks.into_iter().map(|t| (t.id, t)).collect();
    Ok(rows
        .into_iter()
        .filter_map(|(id, tid, started, ms, completed, skipped, source)| {
            by.get(&tid).map(|t| HistoryEntry { id, started_at: bc_db::util::iso(&started), ms_played: ms, completed, skipped, source, track: t.clone() })
        })
        .collect())
}

/// Forget plays: everything (counters zeroed) or only the last `days` days (counters backed out).
pub fn reset(ctx: &Ctx, days: Option<i64>) -> ApiResult<HistoryReset> {
    let out = ctx.write(move |t| {
        match days {
            None => {
                let events: i64 = t.query_row("SELECT COUNT(*) FROM play_history", [], |r| r.get(0))?;
                let tracks: i64 =
                    t.query_row("SELECT COUNT(*) FROM tracks WHERE play_count > 0 OR skip_count > 0 OR last_played_at IS NOT NULL", [], |r| r.get(0))?;
                t.execute("DELETE FROM play_history", [])?;
                t.execute("UPDATE tracks SET play_count = 0, skip_count = 0, last_played_at = NULL WHERE play_count > 0 OR skip_count > 0 OR last_played_at IS NOT NULL", [])?;
                Ok(HistoryReset { days: None, events, tracks })
            }
            Some(d) => {
                let d = d.clamp(1, 365);
                let cut = cutoff(d);
                let removed: Vec<(i64, i64, i64, i64)> = {
                    let mut st = t.prepare(
                        "SELECT track_id, SUM(completed), SUM(skipped), COUNT(*) FROM play_history WHERE started_at >= ?1 GROUP BY track_id",
                    )?;
                    st.query_map([&cut], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<Result<_, _>>()?
                };
                if removed.is_empty() {
                    return Ok(HistoryReset { days: Some(d), events: 0, tracks: 0 });
                }
                t.execute("DELETE FROM play_history WHERE started_at >= ?1", [&cut])?;
                let mut events = 0;
                for (tid, plays, skips, count) in &removed {
                    events += count;
                    t.execute(
                        "UPDATE tracks SET play_count = MAX(0, play_count - ?2), skip_count = MAX(0, skip_count - ?3),
                                last_played_at = (SELECT MAX(started_at) FROM play_history WHERE track_id = ?1 AND completed = 1) WHERE id = ?1",
                        params![tid, plays, skips],
                    )?;
                }
                Ok(HistoryReset { days: Some(d), events, tracks: removed.len() as i64 })
            }
        }
    })?;
    ctx.bus.invalidate("track", vec![]);
    Ok(out)
}
