//! Legacy cover conversion.

use bc_libcore::Ctx;

use crate::art::{convert_legacy_blocking, convert_one};
use crate::testutil::*;

fn jpeg(w: u32, h: u32, seed: u8) -> Vec<u8> {
    let img = image::RgbImage::from_fn(w, h, |x, y| image::Rgb([(x as u8).wrapping_add(seed), y as u8, seed]));
    let mut buf = std::io::Cursor::new(Vec::new());
    img.write_to(&mut buf, image::ImageFormat::Jpeg).unwrap();
    buf.into_inner()
}

/// n releases with legacy JPEGs under `<dir>/legacy/NNNN/<id>.jpg` and `artwork(source='legacy')` rows.
fn seed(ctx: &Ctx, dir: &std::path::Path, n: i64) -> std::path::PathBuf {
    let legacy = dir.join("legacy");
    for id in 1..=n {
        let p = bc_media::artwork::legacy_jpeg_path(&legacy, id, false);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, jpeg(200 + id as u32, 180, id as u8)).unwrap();
    }
    let l = legacy.clone();
    ctx.write(move |tx| {
        for id in 1..=n {
            let p = bc_media::artwork::legacy_jpeg_path(&l, id, false);
            tx.execute(
                "INSERT INTO releases(id, title, title_key, kind, added_at, cover_path, snippet_only) VALUES (?1, 't', 't', 'album', CURRENT_TIMESTAMP, ?2, 0)",
                bc_db::rusqlite::params![id, p.to_string_lossy()],
            )?;
            tx.execute("INSERT INTO artwork(release_id, version, sizes, source) VALUES (?1, '0', 0, 'legacy')", [id])?;
        }
        Ok(())
    })
    .unwrap();
    legacy
}

#[test]
fn converts_all_legacy_rows_and_reports_progress() {
    let e = env();
    let legacy = seed(&e.ctx, e.dir.path(), 5);
    let handle = e.ctx.jobs.begin("art", "convert");
    let r = convert_legacy_blocking(&e.ctx, &legacy, &handle).unwrap();
    assert_eq!((r.converted, r.failed, r.total), (5, 0, 5));
    assert_eq!(scalar(&e.ctx, "SELECT COUNT(*) FROM artwork WHERE source = 'legacy-webp' AND sizes = 7 AND blurhash IS NOT NULL AND width >= 200"), 5);
    assert_eq!(scalar(&e.ctx, "SELECT COUNT(*) FROM releases WHERE cover_path LIKE '%.webp'"), 5);
    let p: String = e.ctx.db.read(|c| Ok(c.query_row("SELECT cover_path FROM releases WHERE id = 3", [], |r| r.get(0))?)).unwrap();
    assert!(std::path::Path::new(&p).exists());
    // idempotent: nothing left to do
    let again = convert_legacy_blocking(&e.ctx, &legacy, &handle).unwrap();
    assert_eq!(again.total, 0);
}

#[test]
fn missing_legacy_file_counts_as_failed_and_cancel_is_honoured() {
    let e = env();
    let legacy = seed(&e.ctx, e.dir.path(), 3);
    std::fs::remove_file(bc_media::artwork::legacy_jpeg_path(&legacy, 2, false)).unwrap();
    let handle = e.ctx.jobs.begin("art", "convert");
    let r = convert_legacy_blocking(&e.ctx, &legacy, &handle).unwrap();
    assert_eq!((r.converted, r.failed), (2, 1));
    assert_eq!(scalar(&e.ctx, "SELECT COUNT(*) FROM artwork WHERE source = 'legacy'"), 1);

    let e2 = env();
    let legacy2 = seed(&e2.ctx, e2.dir.path(), 3);
    let h2 = e2.ctx.jobs.begin("art", "convert");
    e2.ctx.jobs.request_cancel(&h2.id);
    let r = convert_legacy_blocking(&e2.ctx, &legacy2, &h2).unwrap();
    assert!(r.cancelled);
    assert_eq!(r.converted, 0);
}

#[test]
fn convert_one_on_demand() {
    let e = env();
    seed(&e.ctx, e.dir.path(), 2);
    convert_one(&e.ctx, 1).unwrap();
    assert_eq!(scalar(&e.ctx, "SELECT COUNT(*) FROM artwork WHERE source = 'legacy-webp'"), 1);
    convert_one(&e.ctx, 1).unwrap(); // already converted: no-op
    assert!(convert_one(&e.ctx, 999).is_err());
}
