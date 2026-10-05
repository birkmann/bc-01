//! The Bandcamp-links file (`links.db` + `.bc-links.db` in the library root) survives a rebuilt library.

mod common;

use std::path::Path;

use bc_library::ledger::{FILE_NAME, MIRROR_NAME, sync};
use common::{App, app};

/// A library over `music` holding one release as a fresh scan sees it: tag artist, tag label, no links.
fn scanned(music: &Path, folder: &str, artist: &str, label: &str, title: &str) -> App {
    let a = app();
    let key = |s: &str| s.to_lowercase();
    a.sql(&format!(
        "INSERT INTO library_roots(id,path,kind,watch,enabled) VALUES (1,'{root}','library',0,1);
         INSERT INTO artists(id,name,name_key,created_at) VALUES (1,'{artist}','{ak}','x');
         INSERT INTO labels(id,name,name_key) VALUES (1,'{label}','{lk}');
         INSERT INTO releases(id,title,title_key,artist_id,label_id,kind,year,folder_path,added_at)
              VALUES (1,'{title}','{tk}',1,1,'album',2020,'{root}/{folder}','x');",
        root = music.display(),
        ak = key(artist),
        lk = key(label),
        tk = key(title),
    ));
    a
}

/// Link it the way relink + the label resolver do.
fn link(a: &App) {
    a.sql(
        "INSERT OR IGNORE INTO artists(name,name_key,created_at) VALUES ('Oliver Hacke','oliver hacke','x');
         UPDATE artists SET bandcamp_url='https://oliverhacke.bandcamp.com' WHERE name_key='oliver hacke';
         INSERT OR IGNORE INTO labels(name,name_key) VALUES ('Yore Records','yore records');
         UPDATE labels SET bandcamp_url='https://yore.bandcamp.com' WHERE name_key='yore records';
         UPDATE releases SET bandcamp_url='https://yore.bandcamp.com/album/midatlantic', bandcamp_item_id=42,
                label_id=(SELECT id FROM labels WHERE name_key='yore records'),
                artist_id=(SELECT id FROM artists WHERE name_key='oliver hacke') WHERE id=1;
         INSERT INTO release_relink(release_id,tried_at,url) VALUES (1,'2026-10-04 20:00:00','https://yore.bandcamp.com/album/midatlantic');",
    );
}

fn text(a: &App, sql: &str) -> String {
    let s = sql.to_string();
    a.ctx.db.read(move |c| Ok(c.query_row(&s, [], |r| r.get::<_, String>(0))?)).unwrap()
}

#[test]
fn a_rebuilt_library_gets_its_links_back_from_the_copy_in_the_music_folder() {
    let music = tempfile::tempdir().unwrap();
    let old = scanned(music.path(), "yore/midatlantic", "Yore Records", "Yore Records", "Midatlantic EP");
    link(&old);
    let r = sync(&old.ctx).unwrap();
    assert_eq!((r.restored, r.mirrored), (0, 1));
    assert!(old.dir.path().join(FILE_NAME).is_file());
    assert!(music.path().join(MIRROR_NAME).is_file());

    // The data dir is gone: a new library.db, rescanned from tags, and no links.db.
    let new = scanned(music.path(), "yore/midatlantic", "Yore Records", "Yore Records", "Midatlantic EP");
    assert!(!new.dir.path().join(FILE_NAME).exists());
    let r = sync(&new.ctx).unwrap();
    assert_eq!(r.restored, 1);
    assert_eq!(r.artists_restored, 1);
    assert_eq!(text(&new, "SELECT bandcamp_url FROM releases WHERE id=1"), "https://yore.bandcamp.com/album/midatlantic");
    assert_eq!(new.scalar("SELECT bandcamp_item_id FROM releases WHERE id=1"), 42);
    assert_eq!(text(&new, "SELECT l.name || ' ' || l.bandcamp_url FROM releases r JOIN labels l ON l.id=r.label_id"), "Yore Records https://yore.bandcamp.com");
    assert_eq!(text(&new, "SELECT a.name || ' ' || a.bandcamp_url FROM releases r JOIN artists a ON a.id=r.artist_id"), "Oliver Hacke https://oliverhacke.bandcamp.com");
    // Relink will not search it again.
    assert_eq!(text(&new, "SELECT url FROM release_relink WHERE release_id=1"), "https://yore.bandcamp.com/album/midatlantic");
    assert!(new.dir.path().join(FILE_NAME).is_file());

    // A second sync has nothing to do.
    let r = sync(&new.ctx).unwrap();
    assert_eq!((r.restored, r.saved, r.mirrored), (0, 0, 0));
}

#[test]
fn an_edit_made_in_the_app_is_kept_and_saved() {
    let music = tempfile::tempdir().unwrap();
    let a = scanned(music.path(), "x/y", "Artist", "Tag Label", "Album");
    link(&a);
    sync(&a.ctx).unwrap();
    a.sql("INSERT INTO labels(id,name,name_key) VALUES (3,'My Label','my label'); UPDATE releases SET label_id=3 WHERE id=1;");
    let r = sync(&a.ctx).unwrap();
    assert_eq!((r.restored, r.saved), (0, 1));
    assert_eq!(a.scalar("SELECT label_id FROM releases WHERE id=1"), 3);

    let fresh = scanned(music.path(), "x/y", "Artist", "Tag Label", "Album");
    sync(&fresh.ctx).unwrap();
    assert_eq!(text(&fresh, "SELECT l.name FROM releases r JOIN labels l ON l.id=r.label_id"), "My Label");
}

#[test]
fn a_moved_folder_is_matched_by_artist_title_and_year() {
    let music = tempfile::tempdir().unwrap();
    let a = scanned(music.path(), "old/place", "Oliver Hacke", "Tag", "Midatlantic EP");
    link(&a);
    sync(&a.ctx).unwrap();
    let b = scanned(music.path(), "new/place", "Oliver Hacke", "Tag", "Midatlantic EP");
    assert_eq!(sync(&b.ctx).unwrap().restored, 1);
    assert_eq!(text(&b, "SELECT bandcamp_url FROM releases WHERE id=1"), "https://yore.bandcamp.com/album/midatlantic");
}

#[test]
fn an_unlinked_release_seen_for_the_first_time_never_clears_a_saved_row() {
    let music = tempfile::tempdir().unwrap();
    let a = scanned(music.path(), "x/y", "Artist", "Tag", "Album");
    link(&a);
    sync(&a.ctx).unwrap();
    // Same folder and title, but its URL already belongs to another release: nothing restores.
    let b = scanned(music.path(), "x/y", "Artist", "Tag", "Album");
    b.sql("UPDATE releases SET label_id=NULL WHERE id=1;
           INSERT INTO releases(id,title,title_key,kind,bandcamp_url,added_at) VALUES (2,'Other','other','album','https://yore.bandcamp.com/album/midatlantic','x');");
    sync(&b.ctx).unwrap();
    let c = scanned(music.path(), "x/y", "Artist", "Tag", "Album");
    sync(&c.ctx).unwrap();
    assert_eq!(text(&c, "SELECT bandcamp_url FROM releases WHERE id=1"), "https://yore.bandcamp.com/album/midatlantic");
}

#[test]
fn an_unreadable_copy_in_the_music_folder_is_left_alone() {
    let music = tempfile::tempdir().unwrap();
    std::fs::write(music.path().join(MIRROR_NAME), b"not a database").unwrap();
    let a = scanned(music.path(), "x/y", "Artist", "Tag", "Album");
    link(&a);
    let r = sync(&a.ctx).unwrap();
    assert_eq!(r.mirrored, 0);
    assert_eq!(std::fs::read(music.path().join(MIRROR_NAME)).unwrap(), b"not a database");
    assert!(a.dir.path().join(FILE_NAME).is_file());
}
