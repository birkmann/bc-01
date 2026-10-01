use axum::Json;
use axum::extract::State;
use bc_libcore::{ApiResult, scope};
use bc_types::library::{LibraryChanged, LibraryScopeIn, LibraryScopeOut, SnippetSettingIn, SnippetSettingOut, TOPIC_LIBRARY_CHANGED};

use super::{MaintState, blocking};
use crate::{adopt, snippets};

fn scope_out(c: &bc_db::rusqlite::Connection) -> ApiResult<LibraryScopeOut> {
    let n: i64 = c.query_row("SELECT COUNT(*) FROM releases WHERE source_fan_id IS NOT NULL", [], |r| r.get(0))?;
    Ok(LibraryScopeOut { unified: scope::unified(c)?, foreign_releases: n })
}

pub async fn get_scope(State(s): State<MaintState>) -> ApiResult<Json<LibraryScopeOut>> {
    let ctx = s.ctx.clone();
    Ok(Json(blocking(move || ctx.read(scope_out)).await?))
}

/// Persisted server-side, like the auto-download switch: every listing reads it, so it has to
/// hold for any tab and for the player's continuation.
pub async fn set_scope(State(s): State<MaintState>, Json(body): Json<LibraryScopeIn>) -> ApiResult<Json<LibraryScopeOut>> {
    let ctx = s.ctx.clone();
    let out = blocking(move || {
        ctx.write(move |t| {
            adopt::set_unified(t, body.unified)?;
            scope_out(t)
        })
    })
    .await?;
    s.ctx.bus.publish(TOPIC_LIBRARY_CHANGED, &LibraryChanged { scope: Some(if body.unified { "all" } else { "mine" }.into()), ..Default::default() });
    Ok(Json(out))
}

fn snippet_out(c: &bc_db::rusqlite::Connection) -> ApiResult<SnippetSettingOut> {
    let (tracks, releases) = snippets::counts(c)?;
    Ok(SnippetSettingOut { hidden: snippets::hidden(c)?, snippet_tracks: tracks, snippet_releases: releases })
}

pub async fn get_snippets(State(s): State<MaintState>) -> ApiResult<Json<SnippetSettingOut>> {
    let ctx = s.ctx.clone();
    Ok(Json(blocking(move || ctx.read(snippet_out)).await?))
}

/// Server-side like the library scope beside it. Kept apart from `/library/scope` because the two
/// exclusions are unrelated (whose records, versus whether the file is the record at all).
pub async fn set_snippets(State(s): State<MaintState>, Json(body): Json<SnippetSettingIn>) -> ApiResult<Json<SnippetSettingOut>> {
    let ctx = s.ctx.clone();
    let out = blocking(move || {
        ctx.write(move |t| {
            snippets::set_hidden(t, body.hidden)?;
            snippet_out(t)
        })
    })
    .await?;
    s.ctx.bus.publish(TOPIC_LIBRARY_CHANGED, &LibraryChanged { snippets: Some(if body.hidden { "hidden" } else { "shown" }.into()), ..Default::default() });
    Ok(Json(out))
}
