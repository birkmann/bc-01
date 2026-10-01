#![allow(dead_code)]
use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode};
use bc_db::Db;
use bc_libcore::Ctx;
use serde_json::Value;
use tower::ServiceExt;

pub struct App {
    pub dir: tempfile::TempDir,
    pub ctx: Ctx,
    pub router: axum::Router,
}

pub fn app() -> App {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("library.db")).unwrap();
    let mut cfg = bc_core::Config::from_env();
    cfg.data_dir = dir.path().to_path_buf();
    let ctx = Ctx::new(db, Arc::new(bc_core::EventBus::new()), cfg);
    let router = bc_library::routes::router(ctx.clone());
    App { dir, ctx, router }
}

impl App {
    pub fn sql(&self, sql: &str) {
        let s = sql.to_string();
        self.ctx.db.write(move |t| {
            t.execute_batch(&s)?;
            Ok(())
        })
        .unwrap();
    }

    pub fn scalar(&self, sql: &str) -> i64 {
        let s = sql.to_string();
        self.ctx.db.read(move |c| Ok(c.query_row(&s, [], |r| r.get(0))?)).unwrap()
    }

    pub async fn call(&self, method: Method, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut req = Request::builder().method(method).uri(uri);
        let body = match body {
            Some(b) => {
                req = req.header("content-type", "application/json");
                Body::from(b.to_string())
            }
            None => Body::empty(),
        };
        let resp = self.router.clone().oneshot(req.body(body).unwrap()).await.unwrap();
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), 64 << 20).await.unwrap();
        let v = if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap_or(Value::Null) };
        (status, v)
    }

    pub async fn get(&self, uri: &str) -> Value {
        let (s, v) = self.call(Method::GET, uri, None).await;
        assert_eq!(s, StatusCode::OK, "GET {uri}: {v}");
        v
    }
}

/// Two albums of mine, one on alice's shelf; each with its own artist, label and tag; two tracks each.
/// (port of `_seed` in test_library_scope.py)
pub fn seed_scope(a: &App) {
    let mut sql = String::from(
        "INSERT INTO library_roots(id,path,kind,watch,enabled) VALUES (1,'/tmp/bc-scope-test','downloads',0,1);
         INSERT INTO fans(id,username,url,display_name,is_self,created_at) VALUES (1,'alice','https://bandcamp.com/alice','alice',0,'x');",
    );
    let albums = [("mine", "My Artist", "My Label", "mytag", "NULL"), ("other", "Other Artist", "Other Label", "othertag", "NULL"), ("shelf", "Shelf Artist", "Shelf Label", "shelftag", "1")];
    let mut tid = 0;
    for (i, (key, artist, label, tag, fan)) in albums.iter().enumerate() {
        let n = i as i64 + 1;
        sql += &format!(
            "INSERT INTO artists(id,name,name_key,created_at) VALUES ({n},'{artist}',lower('{artist}'),'x');
             INSERT INTO labels(id,name,name_key) VALUES ({n},'{label}',lower('{label}'));
             INSERT INTO tags(id,name,name_key,kind,track_count) VALUES ({n},'{tag}','{tag}','genre',2);
             INSERT INTO releases(id,title,title_key,artist_id,label_id,kind,year,bandcamp_url,added_at,source_fan_id)
                  VALUES ({n},'{key} album','{key} album',{n},{n},'album',2024,'https://{key}.bandcamp.com/album/{key}-album','2026-01-0{n} 00:00:00',{fan});"
        );
        for k in 1..=2 {
            tid += 1;
            sql += &format!(
                "INSERT INTO tracks(id,release_id,artist_id,title,title_key,track_no,duration_ms,play_count,skip_count,loved,added_at)
                      VALUES ({tid},{n},{n},'{key} {k}','{key} {k}',{k},200000,3,0,0,'2026-01-0{n} 00:00:0{k}');
                 INSERT INTO files(track_id,root_id,path,rel_path,ext,size_bytes,mtime_ns,first_seen_at,last_seen_at)
                      VALUES ({tid},1,'/tmp/bc-scope-test/{key}/{k}.mp3','{key}/{k}.mp3','mp3',1000,1,'x','x');
                 INSERT INTO track_tags(track_id,tag_id,source,weight) VALUES ({tid},{n},'file',1);
                 INSERT INTO play_history(track_id,started_at,ms_played,completed,skipped,source) VALUES ({tid},'{now}',200000,1,0,'library');",
                now = bc_db::util::now_db()
            );
        }
    }
    a.sql(&sql);
}
