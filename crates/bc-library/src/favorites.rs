//! `/favorites`: artists, labels and tags pinned for quick access from Home. Idempotent adds and removes.

use bc_db::rusqlite::{Connection, OptionalExtension};
use bc_db::util::name_key;
use bc_libcore::{ApiError, ApiResult, Ctx, Scope};
use bc_types::library::*;

pub fn list(c: &Connection, scope: &Scope) -> ApiResult<FavoritesOut> {
    let rows: Vec<(Option<i64>, Option<i64>, Option<i64>)> = {
        let mut st = c.prepare("SELECT artist_id, label_id, tag_id FROM favorites ORDER BY created_at DESC, id DESC")?;
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<_, _>>()?
    };
    let artist_ids: Vec<i64> = rows.iter().filter_map(|r| r.0).collect();
    let label_ids: Vec<i64> = rows.iter().filter_map(|r| r.1).collect();
    let tag_ids: Vec<i64> = rows.iter().filter_map(|r| r.2).collect();
    let artists = crate::artists::artists_out(c, &artist_ids, scope, &Default::default())?;
    let labels = crate::labels::labels_out(c, &label_ids, scope)?;
    let mut tags = Vec::new();
    for id in tag_ids {
        if let Some(t) = c
            .query_row("SELECT id, name, track_count FROM tags WHERE id = ?1", [id], |r| Ok(TagOut { id: r.get(0)?, name: r.get(1)?, track_count: r.get(2)? }))
            .optional()?
        {
            tags.push(t);
        }
    }
    Ok(FavoritesOut { artists, labels, tags })
}

#[derive(Clone, Copy)]
pub enum Kind {
    Artist,
    Label,
}

impl Kind {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "artist" => Some(Self::Artist),
            "label" => Some(Self::Label),
            _ => None,
        }
    }
    fn table(self) -> &'static str {
        match self {
            Self::Artist => "artists",
            Self::Label => "labels",
        }
    }
    fn col(self) -> &'static str {
        match self {
            Self::Artist => "artist_id",
            Self::Label => "label_id",
        }
    }
}

pub fn pin(ctx: &Ctx, kind: Kind, id: i64) -> ApiResult<()> {
    ctx.write(move |t| {
        let exists = t.query_row(&format!("SELECT 1 FROM {} WHERE id = ?1", kind.table()), [id], |_| Ok(())).optional()?.is_some();
        if !exists {
            return Err(ApiError::not_found(format!("{} {id} not found", kind.col().trim_end_matches("_id"))));
        }
        t.execute(
            &format!("INSERT INTO favorites ({0}, created_at) SELECT ?1, ?2 WHERE NOT EXISTS (SELECT 1 FROM favorites WHERE {0} = ?1)", kind.col()),
            bc_db::rusqlite::params![id, bc_db::util::now_db()],
        )?;
        Ok(())
    })
}

pub fn unpin(ctx: &Ctx, kind: Kind, id: i64) -> ApiResult<()> {
    ctx.write(move |t| {
        t.execute(&format!("DELETE FROM favorites WHERE {} = ?1", kind.col()), [id])?;
        Ok(())
    })
}

/// Tags are pinned by name (a tag chip knows its name and nothing else).
pub fn pin_tag(ctx: &Ctx, name: String) -> ApiResult<()> {
    ctx.write(move |t| {
        let key = name_key(&name);
        let id: i64 = t
            .query_row("SELECT id FROM tags WHERE name_key = ?1", [&key], |r| r.get(0))
            .optional()?
            .ok_or_else(|| ApiError::not_found(format!("tag {name:?} not found")))?;
        t.execute(
            "INSERT INTO favorites (tag_id, created_at) SELECT ?1, ?2 WHERE NOT EXISTS (SELECT 1 FROM favorites WHERE tag_id = ?1)",
            bc_db::rusqlite::params![id, bc_db::util::now_db()],
        )?;
        Ok(())
    })
}

pub fn unpin_tag(ctx: &Ctx, name: String) -> ApiResult<()> {
    ctx.write(move |t| {
        let key = name_key(&name);
        if let Some(id) = t.query_row("SELECT id FROM tags WHERE name_key = ?1", [&key], |r| r.get::<_, i64>(0)).optional()? {
            t.execute("DELETE FROM favorites WHERE tag_id = ?1", [id])?;
        }
        Ok(())
    })
}
