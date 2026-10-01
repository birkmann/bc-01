//! EXPLAIN QUERY PLAN guards: no hot read path may full-scan `tracks` (a SCAN that is not driven by an index).

mod common;

use bc_libcore::Scope;
use bc_library::tracks;
use bc_types::library::*;
use common::*;

fn seed_many(a: &App) {
    let mut sql = String::from(
        "INSERT INTO library_roots(id,path,kind,watch,enabled) VALUES (1,'/m','library',0,1);
         INSERT INTO tags(id,name,name_key,kind,track_count) VALUES (1,'Techno','techno','genre',1000),(2,'Dub','dub','genre',50);",
    );
    for r in 1..=300 {
        sql += &format!("INSERT INTO artists(id,name,name_key,created_at) VALUES ({r},'A{r}','a{r}','x');
                         INSERT INTO releases(id,title,title_key,artist_id,kind,year,added_at) VALUES ({r},'R{r}','r{r}',{r},'album',{y},'2020-01-{d:02} 00:00:00');", y = 2000 + r % 25, d = 1 + r % 28);
    }
    for i in 1..=3000 {
        let r = 1 + i % 300;
        sql += &format!(
            "INSERT INTO tracks(id,release_id,artist_id,title,title_key,track_no,duration_ms,loved,play_count,skip_count,added_at,is_snippet) VALUES ({i},{r},{r},'T{i}','t{i}',{n},{i},{l},{p},0,'2021-01-01 00:00:{s:02}',{sn});
             INSERT INTO files(track_id,root_id,path,rel_path,ext,size_bytes,mtime_ns,first_seen_at,last_seen_at) VALUES ({i},1,'/m/{i}.mp3','{i}.mp3','mp3',1,1,'x','x');
             INSERT INTO track_tags(track_id,tag_id,source,weight) VALUES ({i},{tg},'f',1);
             INSERT INTO search_index(track_id,title,artist,album,label,tags) VALUES ({i},'T{i}','A{r}','R{r}','','');",
            n = 1 + i % 12, l = (i % 9 == 0) as i32, p = i % 5, s = i % 60, sn = (i % 50 == 0) as i32, tg = 1 + (i % 20 == 0) as i32
        );
    }
    a.sql(&sql);
    a.sql("ANALYZE");
}

fn plan(a: &App, q: &TrackQuery) -> Vec<String> {
    a.ctx
        .read(|c| {
            let scope = Scope::resolve(c, None, None).unwrap();
            let (sql, params) = tracks::explain_sql(c, q, &scope).unwrap();
            let mut st = c.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?;
            let rows = st
                .query_map(bc_db::rusqlite::params_from_iter(params.iter()), |r| r.get::<_, String>(3))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .unwrap()
}

/// A plan line that scans `tracks` without an index.
fn full_scan_of_tracks(lines: &[String]) -> Option<&String> {
    lines.iter().find(|l| {
        let l = l.trim();
        (l.starts_with("SCAN tracks") || l.starts_with("SCAN t ") || l == "SCAN t") && !l.contains("USING")
    })
}

#[test]
fn hot_track_listings_never_full_scan_tracks() {
    let a = app();
    seed_many(&a);
    a.sql("INSERT INTO settings(key,value,updated_at) VALUES ('library.hide_snippets','1','x')");
    let mk = |f: &dyn Fn(&mut TrackQuery)| {
        let mut q = TrackQuery { offset: Some(1000), ..Default::default() };
        f(&mut q);
        q
    };
    let cases: Vec<(&str, TrackQuery)> = vec![
        ("default", mk(&|_| {})),
        ("title", mk(&|q| q.sort = Some(TrackSort::Title))),
        ("artist", mk(&|q| q.sort = Some(TrackSort::Artist))),
        ("album asc", mk(&|q| { q.sort = Some(TrackSort::Album); q.order = Some(SortDir::Asc) })),
        ("duration", mk(&|q| q.sort = Some(TrackSort::Duration))),
        ("bpm", mk(&|q| q.sort = Some(TrackSort::Bpm))),
        ("play_count", mk(&|q| q.sort = Some(TrackSort::PlayCount))),
        ("year", mk(&|q| q.sort = Some(TrackSort::Year))),
        ("tag", mk(&|q| q.tags = vec!["dub".into()])),
        ("loved", mk(&|q| q.loved = Some(true))),
        ("artist_id", mk(&|q| q.artist_id = Some(5))),
        ("release_id", mk(&|q| q.release_id = Some(5))),
        ("label_id", mk(&|q| q.label_id = Some(5))),
        ("fts relevance", mk(&|q| q.q = Some("T12".into()))),
        ("fts + title sort", mk(&|q| { q.q = Some("T12".into()); q.sort = Some(TrackSort::Title) })),
        ("favorites", mk(&|q| q.favorites = Some(true))),
    ];
    for (name, q) in cases {
        let lines = plan(&a, &q);
        if let Some(bad) = full_scan_of_tracks(&lines) {
            panic!("{name}: full scan of tracks: {bad}\n{}", lines.join("\n"));
        }
    }
}

#[test]
fn deep_offset_walks_the_covering_index_only() {
    let a = app();
    seed_many(&a);
    let lines = plan(&a, &TrackQuery { offset: Some(2000), ..Default::default() });
    assert!(lines.iter().any(|l| l.contains("ix_tracks_s_added")), "{lines:?}");
}
