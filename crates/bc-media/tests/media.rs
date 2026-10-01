//! Ports of `test_metadata.py` and `test_metadata_write.py`: tag reading across containers,
//! gap-fill write-back, atomicity and undo. Fixtures are tiny ffmpeg-generated, tag-free files in
//! `tests/fixtures/` (0.5 s sine in each container), copied into a temp dir per test.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use bc_media::tags::{self, TrackTags, find_sidecar_cover, read_cover, read_tags};
use bc_media::write::{
    self, DjFields, FieldGroup, WriteMode, WriteStatus, plan_gaps, plan_write, purge_tag_tmp,
    read_dj_fields, remove_dj_fields, undo_fields, write_dj_fields,
};
use lofty::config::WriteOptions;
use lofty::file::TaggedFileExt;
use lofty::picture::{MimeType, Picture, PictureType};
use lofty::prelude::*;
use lofty::tag::{ItemKey, Tag};

const GENRES: [&str; 3] = ["techno", "dub techno", "industrial"];
const FORMATS: [&str; 3] = ["mp3", "flac", "m4a"];
const ALL_FORMATS: [&str; 7] = ["mp3", "flac", "m4a", "ogg", "opus", "wav", "aiff"];

/// A 1x1-ish JPEG so "did the art survive?" is a byte comparison rather than a hope.
const JPEG: &[u8] = &[
    0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10, b'J', b'F', b'I', b'F', 0x00, 0x01, 0x01, 0x00, 0x00, 0x01,
    0x00, 0x01, 0x00, 0x00, 0xff, 0xdb, 0x00, 0x43, 0x00, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16,
    16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16,
    16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16,
    16, 16, 16, 16, 16, 16, 16, 16, 0xff, 0xd9,
];

fn full() -> DjFields {
    DjFields {
        bpm: Some(128.02),
        initial_key: Some("Am".into()),
        camelot: Some("8A".into()),
        energy: Some(7),
        replaygain_track_gain: Some(-3.42),
        replaygain_track_peak: Some(0.944061),
    }
}

fn fixture(dir: &Path, ext: &str) -> PathBuf {
    let src = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(format!("sine.{ext}"));
    let dst = dir.join(format!("track.{ext}"));
    fs::copy(src, &dst).unwrap();
    dst
}

/// Common tags + (for ID3/MP4) embedded cover, written with lofty's generic API.
fn tag_common(path: &Path, with_art: bool) {
    let mut tagged = lofty::read_from_path(path).unwrap();
    let tt = tagged.primary_tag_type();
    if tagged.primary_tag().is_none() {
        tagged.insert_tag(Tag::new(tt));
    }
    let tag = tagged.primary_tag_mut().unwrap();
    tag.set_title("Grid Failure".into());
    tag.set_artist("Somatic".into());
    tag.set_album("Vault".into());
    tag.insert_text(ItemKey::AlbumArtist, "Somatic".into());
    for g in &GENRES[..2] {
        tag.push(lofty::tag::TagItem::new(
            ItemKey::Genre,
            lofty::tag::ItemValue::Text((*g).to_string()),
        ));
    }
    if with_art {
        tag.push_picture(
            Picture::unchecked(JPEG.to_vec())
                .pic_type(PictureType::CoverFront)
                .mime_type(MimeType::Jpeg)
                .build(),
        );
    }
    tagged.save_to_path(path, WriteOptions::default()).unwrap();

    if path.extension().and_then(|e| e.to_str()) == Some("m4a") {
        // the generic conversion keeps only one genre; MP4 stores several values in one atom
        use lofty::file::AudioFile;
        use lofty::mp4::{Atom, AtomData, AtomIdent, Mp4File};
        let mut f =
            Mp4File::read_from(&mut fs::File::open(path).unwrap(), Default::default()).unwrap();
        let data = GENRES[..2]
            .iter()
            .map(|g| AtomData::UTF8((*g).to_string()))
            .collect();
        let atom = Atom::from_collection(AtomIdent::Fourcc(*b"\xa9gen"), data).unwrap();
        f.ilst_mut().unwrap().replace_atom(atom);
        f.save_to_path(path, WriteOptions::default()).unwrap();
    }
}

fn track(ext: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let p = fixture(dir.path(), ext);
    tag_common(&p, ext != "flac");
    (dir, p)
}

fn plan(path: &Path, v: &DjFields) -> write::TagWritePlan {
    plan_gaps(path, &read_tags(path), v)
}

fn write_ok(path: &Path, v: &DjFields) -> write::WriteOutcome {
    write_dj_fields(path, &plan(path, v), WriteMode::Auto).unwrap()
}

fn tmp_files(dir: &Path) -> usize {
    fs::read_dir(dir)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().contains(".bctmp"))
        .count()
}

// ---------------------------------------------------------------------------
// test_metadata.py
// ---------------------------------------------------------------------------

#[test]
fn mp3_tags() {
    let dir = tempfile::tempdir().unwrap();
    let p = fixture(dir.path(), "mp3");
    let mut tagged = lofty::read_from_path(&p).unwrap();
    let mut tag = Tag::new(lofty::tag::TagType::Id3v2);
    tag.set_title("Grid Failure".into());
    tag.set_artist("Somatic".into());
    tag.set_album("Vault".into());
    tag.set_track(3);
    tag.set_track_total(9);
    tag.insert_text(ItemKey::Label, "Vault Sector".into());
    tag.insert_text(ItemKey::RecordingDate, "2026".into());
    tagged.insert_tag(tag);
    tagged.save_to_path(&p, WriteOptions::default()).unwrap();

    let t = read_tags(&p);
    assert_eq!(t.title.as_deref(), Some("Grid Failure"));
    assert_eq!(t.artist.as_deref(), Some("Somatic"));
    assert_eq!(t.album.as_deref(), Some("Vault"));
    assert_eq!(t.year(), Some(2026));
    assert_eq!(t.track_no, Some(3));
    assert_eq!(t.track_total, Some(9));
    assert_eq!(t.label.as_deref(), Some("Vault Sector"));
    assert_eq!(t.codec.as_deref(), Some("mp3"));
    assert!(t.duration_ms.unwrap() > 0);
    assert!(t.sample_rate.unwrap() > 0);
    assert!(!t.has_art);
}

#[test]
fn flac_tags() {
    let dir = tempfile::tempdir().unwrap();
    let p = fixture(dir.path(), "flac");
    let mut tagged = lofty::read_from_path(&p).unwrap();
    let mut tag = Tag::new(lofty::tag::TagType::VorbisComments);
    tag.set_title("Slow Erosion".into());
    tag.set_artist("Nul Object".into());
    tag.set_album("Cassette Ritual".into());
    tag.set_track(2);
    tag.insert_text(ItemKey::Label, "Ferric Tapes".into());
    tag.insert_text(ItemKey::RecordingDate, "2025".into());
    tagged.insert_tag(tag);
    tagged.save_to_path(&p, WriteOptions::default()).unwrap();

    let t = read_tags(&p);
    assert_eq!(t.title.as_deref(), Some("Slow Erosion"));
    assert_eq!(t.artist.as_deref(), Some("Nul Object"));
    assert_eq!(t.album.as_deref(), Some("Cassette Ritual"));
    assert_eq!(t.year(), Some(2025));
    assert_eq!(t.track_no, Some(2));
    assert_eq!(t.label.as_deref(), Some("Ferric Tapes"));
    assert!(t.duration_ms.unwrap() > 0);
    assert_eq!(t.sample_rate, Some(22050));
    assert_eq!(t.channels, Some(1));
}

#[test]
fn m4a_tags() {
    let dir = tempfile::tempdir().unwrap();
    let p = fixture(dir.path(), "m4a");
    let mut tagged = lofty::read_from_path(&p).unwrap();
    let mut tag = Tag::new(lofty::tag::TagType::Mp4Ilst);
    tag.set_title("Pressure I".into());
    tag.set_artist("Vault Sector".into());
    tag.set_album("Pressure Tests".into());
    tag.set_track(1);
    tag.set_track_total(4);
    tag.insert_text(ItemKey::RecordingDate, "2024".into());
    tag.insert_text(ItemKey::Label, "Vault Sector".into());
    tagged.insert_tag(tag);
    tagged.save_to_path(&p, WriteOptions::default()).unwrap();

    let t = read_tags(&p);
    assert_eq!(t.title.as_deref(), Some("Pressure I"));
    assert_eq!(t.artist.as_deref(), Some("Vault Sector"));
    assert_eq!(t.album.as_deref(), Some("Pressure Tests"));
    assert_eq!(t.year(), Some(2024));
    assert_eq!(t.track_no, Some(1));
    assert_eq!(t.track_total, Some(4));
    assert_eq!(t.label.as_deref(), Some("Vault Sector"));
    assert!(t.duration_ms.unwrap() > 0);
}

/// Bandcamp tags arrive as several genre values; collapsing them to one would destroy the main
/// recommendation signal.
#[test]
fn multi_value_genres_survive_in_every_format() {
    for ext in ALL_FORMATS {
        let dir = tempfile::tempdir().unwrap();
        let p = fixture(dir.path(), ext);
        tag_common(&p, false);
        let t = read_tags(&p);
        assert_eq!(t.genres, GENRES[..2].to_vec(), "{ext}");
        assert_eq!(t.title.as_deref(), Some("Grid Failure"), "{ext}");
        assert_eq!(t.artist.as_deref(), Some("Somatic"), "{ext}");
        assert_eq!(t.album.as_deref(), Some("Vault"), "{ext}");
        assert_eq!(t.codec.as_deref(), Some(ext));
        assert!(t.duration_ms.unwrap_or(0) > 0, "{ext} duration");
    }
}

#[test]
fn untagged_fixtures_read_without_error_in_every_format() {
    for ext in ALL_FORMATS {
        let dir = tempfile::tempdir().unwrap();
        let p = fixture(dir.path(), ext);
        let t = read_tags(&p);
        assert_eq!(t.title.as_deref(), Some("track"), "{ext}: stem fallback");
        assert!(t.duration_ms.unwrap_or(0) > 0, "{ext}");
    }
}

/// A corrupt file must still appear in the library rather than aborting a scan.
#[test]
fn unreadable_file_yields_a_titled_record_not_a_panic() {
    let dir = tempfile::tempdir().unwrap();
    let bogus = dir.path().join("broken.mp3");
    fs::write(&bogus, b"not audio at all").unwrap();
    let t = read_tags(&bogus);
    assert_eq!(t.title.as_deref(), Some("broken"));
    assert_eq!(t.codec.as_deref(), Some("mp3"));

    let missing = read_tags(&dir.path().join("gone.flac"));
    assert_eq!(missing.title.as_deref(), Some("gone"));
}

#[test]
fn unknown_extension_falls_back_to_the_filename() {
    let dir = tempfile::tempdir().unwrap();
    let odd = dir.path().join("weird.wma");
    fs::write(&odd, vec![0u8; 64]).unwrap();
    assert_eq!(read_tags(&odd).title.as_deref(), Some("weird"));
}

#[test]
fn year_parses_from_iso_and_bare_year() {
    let y = |d: &str| {
        TrackTags {
            date: Some(d.into()),
            ..Default::default()
        }
        .year()
    };
    assert_eq!(y("2026-07-17"), Some(2026));
    assert_eq!(y("2026"), Some(2026));
    assert_eq!(y(""), None);
    assert_eq!(y("not a date"), None);
    assert_eq!(TrackTags::default().year(), None);
}

/// Re-encoding at a different bitrate must not read as a tag edit.
#[test]
fn tag_hash_ignores_technical_properties() {
    let a = TrackTags {
        title: Some("X".into()),
        artist: Some("Y".into()),
        bitrate: Some(192_000),
        duration_ms: Some(1000),
        ..Default::default()
    };
    let b = TrackTags {
        title: Some("X".into()),
        artist: Some("Y".into()),
        bitrate: Some(320_000),
        duration_ms: Some(2000),
        has_art: true,
        ..Default::default()
    };
    assert_eq!(a.tag_hash(), b.tag_hash());
    let c = TrackTags {
        title: Some("X".into()),
        artist: Some("Z".into()),
        ..Default::default()
    };
    assert_ne!(a.tag_hash(), c.tag_hash());
    assert_eq!(a.tag_hash().len(), 64);
}

/// bandcamp-dl deletes its own cover.jpg, but imported files often have one.
#[test]
fn sidecar_cover_is_found_when_art_is_not_embedded() {
    let dir = tempfile::tempdir().unwrap();
    let p = fixture(dir.path(), "mp3");
    assert!(find_sidecar_cover(dir.path()).is_none());
    assert!(read_cover(&p).is_none());
    fs::write(dir.path().join("Cover.JPG"), b"\xff\xd8\xff\xe0fake-jpeg").unwrap();
    let found = find_sidecar_cover(dir.path()).unwrap();
    assert_eq!(
        found.file_name().unwrap().to_str().unwrap().to_lowercase(),
        "cover.jpg"
    );
    let art = read_cover(&p).unwrap();
    assert_eq!(art.mime, "image/jpeg");

    // priority: cover.* beats folder.*; png carries its mime
    fs::remove_file(dir.path().join("Cover.JPG")).unwrap();
    fs::write(dir.path().join("folder.png"), b"\x89PNG....").unwrap();
    fs::write(dir.path().join("front.jpg"), b"x").unwrap();
    assert_eq!(read_cover(&p).unwrap().mime, "image/png");
}

#[test]
fn embedded_art_wins_over_sidecar_and_sets_has_art() {
    for ext in ["mp3", "m4a"] {
        let (dir, p) = track(ext);
        fs::write(dir.path().join("cover.png"), b"\x89PNG....").unwrap();
        assert!(read_tags(&p).has_art, "{ext}");
        let art = tags::read_embedded_art(&p).unwrap();
        assert_eq!(art.data, JPEG, "{ext}");
        assert_eq!(art.mime, "image/jpeg", "{ext}");
        assert_eq!(read_cover(&p).unwrap().data, JPEG);
    }
}

#[test]
fn audio_extension_helpers() {
    assert!(tags::is_audio_path(Path::new("/a/b/Track.MP3")));
    assert!(tags::is_audio_path(Path::new("x.opus")));
    assert!(!tags::is_audio_path(Path::new("x.jpg")));
    assert!(!tags::is_audio_path(Path::new("noext")));
    assert!(!tags::is_audio_path(Path::new(".x.mp3.bctmp")));
}

// ---------------------------------------------------------------------------
// test_metadata_write.py: round trip
// ---------------------------------------------------------------------------

#[test]
fn written_fields_read_back() {
    for ext in ["mp3", "flac", "m4a", "ogg", "opus", "wav", "aiff"] {
        let (_d, p) = track(ext);
        let out = write_ok(&p, &full());
        assert_eq!(out.status, WriteStatus::Written, "{ext}");
        let t = read_tags(&p);
        assert!(
            (t.bpm.unwrap() - 128.02).abs() < 0.01,
            "{ext} bpm {:?}",
            t.bpm
        );
        assert_eq!(t.initial_key.as_deref(), Some("Am"), "{ext}");
        assert_eq!(t.camelot.as_deref(), Some("8A"), "{ext}");
        assert_eq!(t.energy, Some(7), "{ext}");
        assert!(
            (t.replaygain_track_gain.unwrap() + 3.42).abs() < 1e-9,
            "{ext}"
        );
        assert!(
            (t.replaygain_track_peak.unwrap() - 0.944061).abs() < 1e-6,
            "{ext}"
        );
    }
}

#[test]
fn outcome_carries_new_stat_and_hash() {
    let (_d, p) = track("mp3");
    let before = read_tags(&p).tag_hash();
    let out = write_ok(&p, &full());
    let meta = fs::metadata(&p).unwrap();
    assert_eq!(out.size_bytes, Some(meta.len()));
    let ns = meta
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as i64;
    assert_eq!(out.mtime_ns, Some(ns));
    assert_eq!(
        out.tag_hash.as_deref(),
        Some(read_tags(&p).tag_hash().as_str())
    );
    assert_ne!(out.tag_hash.unwrap(), before);
    assert!(out.frames.contains(&"TXXX:CAMELOT".to_string()));
    assert!(out.fields.contains(&"bpm".to_string()));
}

/// The read path would be satisfied by either frame; these pin the exact ones Rekordbox, Traktor
/// and Mixxx read.
#[test]
fn frames_land_where_dj_tools_look() {
    use lofty::file::AudioFile;
    use lofty::id3::v2::{Frame, FrameId};
    use lofty::mpeg::MpegFile;

    let dir = tempfile::tempdir().unwrap();
    let mp3 = fixture(dir.path(), "mp3");
    write_ok(&mp3, &full());
    let f = MpegFile::read_from(&mut fs::File::open(&mp3).unwrap(), Default::default()).unwrap();
    let id3 = f.id3v2().unwrap();
    let text = |id: &'static str| id3.get_text(&FrameId::Valid(id.into())).map(str::to_string);
    assert_eq!(text("TBPM").as_deref(), Some("128")); // integer-only by spec
    assert_eq!(id3.get_user_text("BPM"), Some("128.02")); // the exact decimal
    assert_eq!(text("TKEY").as_deref(), Some("Am"));
    assert_eq!(id3.get_user_text("CAMELOT"), Some("8A"));
    assert_eq!(id3.get_user_text("EnergyLevel"), Some("7"));
    assert_eq!(id3.get_user_text("REPLAYGAIN_TRACK_GAIN"), Some("-3.42 dB"));
    assert_eq!(id3.get_user_text("REPLAYGAIN_TRACK_PEAK"), Some("0.944061"));
    assert!(
        id3.iter()
            .any(|fr| matches!(fr, Frame::UserText(u) if u.description == "EnergyLevel"))
    );

    let flac = fixture(dir.path(), "flac");
    write_ok(&flac, &full());
    let f =
        lofty::flac::FlacFile::read_from(&mut fs::File::open(&flac).unwrap(), Default::default())
            .unwrap();
    let vc = f.vorbis_comments().unwrap();
    assert_eq!(vc.get("CAMELOT"), Some("8A"));
    assert_eq!(vc.get("INITIALKEY"), Some("Am"));
    assert_eq!(vc.get("ENERGYLEVEL"), Some("7"));
    assert_eq!(vc.get("BPM"), Some("128.02"));

    let m4a = fixture(dir.path(), "m4a");
    write_ok(&m4a, &full());
    let f = lofty::mp4::Mp4File::read_from(&mut fs::File::open(&m4a).unwrap(), Default::default())
        .unwrap();
    let ilst = f.ilst().unwrap();
    let tmpo = ilst.get(&lofty::mp4::AtomIdent::Fourcc(*b"tmpo")).unwrap();
    assert!(tmpo.data().any(|d| matches!(
        d,
        lofty::mp4::AtomData::SignedInteger(128) | lofty::mp4::AtomData::UnsignedInteger(128)
    )));
    let ff = ilst
        .get(&lofty::mp4::AtomIdent::Freeform {
            mean: "com.apple.iTunes".into(),
            name: "camelot".into(),
        })
        .unwrap();
    assert!(
        ff.data()
            .any(|d| matches!(d, lofty::mp4::AtomData::UTF8(s) if s == "8A"))
    );
}

// ---------------------------------------------------------------------------
// never overwrite
// ---------------------------------------------------------------------------

#[test]
fn an_existing_value_is_kept_and_the_rest_filled() {
    for ext in FORMATS {
        let (_d, p) = track(ext);
        write_ok(
            &p,
            &DjFields {
                bpm: Some(90.0),
                ..Default::default()
            },
        );
        let out = write_ok(&p, &full());
        assert_eq!(out.status, WriteStatus::Written);
        assert!(!out.fields.contains(&"bpm".to_string()), "{ext}");
        let t = read_tags(&p);
        assert!((t.bpm.unwrap() - 90.0).abs() < 0.01, "{ext}: untouched");
        assert_eq!(t.camelot.as_deref(), Some("8A"), "{ext}: filled");
    }
}

/// A file with only TBPM already has a BPM. Filling TXXX:BPM from our own analysis would leave it
/// carrying two tempos that disagree.
#[test]
fn a_gap_is_the_field_not_the_frame() {
    use lofty::id3::v2::{Frame, FrameId, Id3v2Tag, TextInformationFrame};
    let dir = tempfile::tempdir().unwrap();
    let p = fixture(dir.path(), "mp3");
    let mut tag = Id3v2Tag::new();
    tag.insert(Frame::Text(TextInformationFrame::new(
        FrameId::Valid("TBPM".into()),
        lofty::TextEncoding::UTF8,
        "120".to_string(),
    )));
    tag.save_to_path(&p, WriteOptions::default()).unwrap();

    write_ok(&p, &full());

    let f = <lofty::mpeg::MpegFile as lofty::file::AudioFile>::read_from(
        &mut fs::File::open(&p).unwrap(),
        Default::default(),
    )
    .unwrap();
    let id3 = f.id3v2().unwrap();
    assert_eq!(id3.get_text(&FrameId::Valid("TBPM".into())), Some("120"));
    assert_eq!(id3.get_user_text("BPM"), None);
    assert!((read_tags(&p).bpm.unwrap() - 120.0).abs() < 1e-9);
}

/// What a broken tagger writes for "unknown". Treating it as real would leave those files
/// permanently unfillable.
#[test]
fn a_zero_bpm_counts_as_empty() {
    use lofty::id3::v2::{Frame, FrameId, Id3v2Tag, TextInformationFrame};
    let dir = tempfile::tempdir().unwrap();
    let p = fixture(dir.path(), "mp3");
    let mut tag = Id3v2Tag::new();
    tag.insert(Frame::Text(TextInformationFrame::new(
        FrameId::Valid("TBPM".into()),
        lofty::TextEncoding::UTF8,
        "0".to_string(),
    )));
    tag.save_to_path(&p, WriteOptions::default()).unwrap();
    assert!(plan(&p, &full()).gaps().contains(&"bpm"));
}

#[test]
fn a_disagreeing_value_is_reported_not_overwritten() {
    let dir = tempfile::tempdir().unwrap();
    let p = fixture(dir.path(), "mp3");
    write_ok(
        &p,
        &DjFields {
            camelot: Some("5A".into()),
            ..Default::default()
        },
    );

    let pl = plan(&p, &full());
    assert_eq!(pl.conflicts(), vec!["camelot"]);
    assert!(!pl.gaps().contains(&"camelot"));
    let c = pl.fields.iter().find(|f| f.field == "camelot").unwrap();
    assert_eq!(
        (c.existing.as_deref(), c.proposed.as_deref()),
        (Some("5A"), Some("8A"))
    );
    write_dj_fields(&p, &pl, WriteMode::Auto).unwrap();
    assert_eq!(read_tags(&p).camelot.as_deref(), Some("5A"));
}

#[test]
fn an_agreeing_value_is_kept() {
    let dir = tempfile::tempdir().unwrap();
    let p = fixture(dir.path(), "flac");
    write_ok(&p, &full());
    let pl = plan(
        &p,
        &DjFields {
            bpm: Some(128.03),
            camelot: Some("8a".into()),
            ..Default::default()
        },
    );
    let status = |n: &str| pl.fields.iter().find(|f| f.field == n).unwrap().status;
    assert_eq!(status("bpm"), write::FieldStatus::Kept);
    assert_eq!(status("camelot"), write::FieldStatus::Kept);
    assert_eq!(status("energy"), write::FieldStatus::NoSource);
}

/// `ID3.add` keyed on the descriptor verbatim in mutagen; a `TXXX:bpm` and `TXXX:BPM` must not
/// coexist.
#[test]
fn a_duplicate_case_txxx_does_not_survive() {
    use lofty::id3::v2::{ExtendedTextFrame, Frame, Id3v2Tag};
    let dir = tempfile::tempdir().unwrap();
    let p = fixture(dir.path(), "mp3");
    let mut tag = Id3v2Tag::new();
    tag.insert(Frame::UserText(ExtendedTextFrame::new(
        lofty::TextEncoding::UTF8,
        "bpm".to_string(),
        String::new(),
    )));
    tag.save_to_path(&p, WriteOptions::default()).unwrap();

    write_ok(&p, &full());

    let f = <lofty::mpeg::MpegFile as lofty::file::AudioFile>::read_from(
        &mut fs::File::open(&p).unwrap(),
        Default::default(),
    )
    .unwrap();
    let n = f
        .id3v2()
        .unwrap()
        .iter()
        .filter(|fr| matches!(fr, Frame::UserText(u) if u.description.eq_ignore_ascii_case("bpm")))
        .count();
    assert_eq!(n, 1);
}

#[test]
fn nothing_to_do_is_a_skip() {
    for ext in FORMATS {
        let (_d, p) = track(ext);
        write_ok(&p, &full());
        let before = fs::metadata(&p).unwrap().modified().unwrap();
        let bytes = fs::read(&p).unwrap();
        let out = write_ok(&p, &full());
        assert_eq!(out.status, WriteStatus::Skipped, "{ext}");
        assert_eq!(fs::metadata(&p).unwrap().modified().unwrap(), before);
        assert_eq!(fs::read(&p).unwrap(), bytes);
    }
}

// ---------------------------------------------------------------------------
// everything else stays put
// ---------------------------------------------------------------------------

#[test]
fn no_other_tag_moves() {
    for ext in ALL_FORMATS {
        let (_d, p) = track(ext);
        let before = read_tags(&p);
        write_ok(&p, &full());
        let after = read_tags(&p);
        assert_eq!(after.title.as_deref(), Some("Grid Failure"), "{ext}");
        assert_eq!(after.title, before.title);
        assert_eq!(after.artist, before.artist, "{ext}");
        assert_eq!(after.album, before.album, "{ext}");
        assert_eq!(after.genres, before.genres, "{ext}");
        assert_eq!(after.genres, GENRES[..2].to_vec(), "{ext}");
        assert_eq!(after.duration_ms, before.duration_ms, "{ext}");
        assert_eq!(after.has_art, before.has_art, "{ext}");
    }
}

#[test]
fn cover_art_survives() {
    for ext in FORMATS {
        let dir = tempfile::tempdir().unwrap();
        let p = fixture(dir.path(), ext);
        tag_common(&p, true);
        let before = tags::read_embedded_art(&p).expect("fixture art");
        write_ok(&p, &full());
        let after = tags::read_embedded_art(&p).expect("art after write");
        assert_eq!(after.data, before.data, "{ext}");
        assert!(read_tags(&p).has_art, "{ext}");
    }
}

// ---------------------------------------------------------------------------
// write mechanics
// ---------------------------------------------------------------------------

#[test]
fn atomic_mode_leaves_no_temp_file() {
    for ext in FORMATS {
        let (d, p) = track(ext);
        let out = write_dj_fields(&p, &plan(&p, &full()), WriteMode::Atomic).unwrap();
        assert_eq!(out.mode, write::WriteKind::Copy);
        assert_eq!(read_tags(&p).camelot.as_deref(), Some("8A"));
        assert_eq!(tmp_files(d.path()), 0);
    }
}

#[test]
fn a_failure_mid_edit_leaves_the_original_untouched() {
    let (d, p) = track("mp3");
    let original = fs::read(&p).unwrap();
    let res: bc_media::Result<()> = write::atomic_edit(&p, |tmp| {
        // half-written garbage in the scratch copy, then the "disk goes away"
        fs::write(tmp, b"garbage").unwrap();
        Err(bc_media::MediaError::Write("disk went away".into()))
    });
    assert!(res.is_err());
    assert_eq!(fs::read(&p).unwrap(), original);
    assert_eq!(tmp_files(d.path()), 0);
}

#[test]
fn a_corrupt_file_fails_without_damage() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("broken.flac");
    fs::write(&p, b"definitely not flac").unwrap();
    let pl = write::plan_gaps(&p, &TrackTags::default(), &full());
    assert!(pl.supported);
    assert!(write_dj_fields(&p, &pl, WriteMode::Auto).is_err());
    assert_eq!(fs::read(&p).unwrap(), b"definitely not flac");
    assert_eq!(tmp_files(dir.path()), 0);
}

#[test]
fn a_read_only_file_fails_without_damage() {
    use std::os::unix::fs::PermissionsExt;
    let (d, p) = track("mp3");
    let original = fs::read(&p).unwrap();
    fs::set_permissions(&p, fs::Permissions::from_mode(0o444)).unwrap();
    let res = write_dj_fields(&p, &plan(&p, &full()), WriteMode::Auto);
    fs::set_permissions(&p, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(res.is_err());
    assert_eq!(fs::read(&p).unwrap(), original);
    assert_eq!(tmp_files(d.path()), 0);
}

#[test]
fn permissions_survive_the_rename() {
    use std::os::unix::fs::PermissionsExt;
    let (_d, p) = track("flac");
    fs::set_permissions(&p, fs::Permissions::from_mode(0o640)).unwrap();
    write_ok(&p, &full());
    assert_eq!(
        fs::metadata(&p).unwrap().permissions().mode() & 0o777,
        0o640
    );
}

#[test]
fn a_symlinked_file_stays_a_symlink() {
    let (d, p) = track("mp3");
    let link = d.path().join("link.mp3");
    std::os::unix::fs::symlink(&p, &link).unwrap();
    write_ok(&link, &full());
    assert!(
        fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(read_tags(&p).camelot.as_deref(), Some("8A"));
}

/// A container with no writer is reported, not attempted.
#[test]
fn an_unwritable_container_is_reported_not_attempted() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("a.wma");
    fs::write(&p, b"not really a wma").unwrap();
    assert!(!write::is_writable(&p));
    let pl = plan(&p, &full());
    assert!(!pl.supported);
    assert!(pl.is_noop());
    let out = write_dj_fields(&p, &pl, WriteMode::Auto).unwrap();
    assert_eq!(out.status, WriteStatus::Unsupported);
    assert_eq!(fs::read(&p).unwrap(), b"not really a wma");
}

#[test]
fn plan_write_respects_groups() {
    let (_d, p) = track("flac");
    let pl = plan_write(&p, &full(), &write::DEFAULT_GROUPS);
    assert!(!pl.gaps().contains(&"energy"));
    assert!(pl.gaps().contains(&"bpm"));
    let pl = plan_write(&p, &full(), &[FieldGroup::Energy]);
    assert_eq!(pl.gaps(), vec!["energy"]);
    assert_eq!(
        pl.fields.iter().find(|f| f.field == "bpm").unwrap().status,
        write::FieldStatus::NoSource
    );
}

#[test]
fn the_tag_hash_changes_only_because_of_our_write() {
    let (_d, p) = track("flac");
    let before = read_tags(&p).tag_hash();
    write_ok(&p, &full());
    let after = read_tags(&p).tag_hash();
    assert_ne!(before, after);
    assert_eq!(read_tags(&p).tag_hash(), after);
}

#[test]
fn purge_removes_only_our_orphans() {
    let dir = tempfile::tempdir().unwrap();
    let sub = dir.path().join("a/b");
    fs::create_dir_all(&sub).unwrap();
    fs::write(sub.join(".track.mp3.00ff.bctmp"), b"x").unwrap();
    fs::write(dir.path().join(".other.flac.1234.bctmp"), b"x").unwrap();
    fs::write(dir.path().join("keep.mp3"), b"x").unwrap();
    fs::write(dir.path().join("download.tmp"), b"x").unwrap();
    assert_eq!(
        purge_tag_tmp(dir.path(), Some(std::time::Duration::from_secs(3600))),
        0
    );
    assert_eq!(purge_tag_tmp(dir.path(), None), 2);
    assert!(dir.path().join("keep.mp3").exists());
    assert!(dir.path().join("download.tmp").exists());
    assert_eq!(purge_tag_tmp(&dir.path().join("missing"), None), 0);
}

// ---------------------------------------------------------------------------
// undo
// ---------------------------------------------------------------------------

fn written_map(pl: &write::TagWritePlan) -> BTreeMap<String, String> {
    pl.fields
        .iter()
        .filter(|f| f.status == write::FieldStatus::Gap)
        .filter_map(|f| f.proposed.clone().map(|p| (f.field.to_string(), p)))
        .collect()
}

#[test]
fn undo_removes_exactly_what_was_written() {
    for ext in FORMATS {
        let (_d, p) = track(ext);
        let pl = plan(&p, &full());
        let written = written_map(&pl);
        write_dj_fields(&p, &pl, WriteMode::Auto).unwrap();
        let fields: Vec<String> = pl.gaps().iter().map(|s| s.to_string()).collect();

        let res = undo_fields(&p, &fields, &written).unwrap();
        let mut removed = res.removed.clone();
        removed.sort();
        let mut want = fields.clone();
        want.sort();
        assert_eq!(removed, want, "{ext}");
        let out = res.outcome.unwrap();
        assert_eq!(
            out.tag_hash.as_deref(),
            Some(read_tags(&p).tag_hash().as_str())
        );

        let t = read_tags(&p);
        assert_eq!(t.bpm, None, "{ext}");
        assert_eq!(t.camelot, None, "{ext}");
        assert_eq!(t.initial_key, None, "{ext}");
        assert_eq!(t.energy, None, "{ext}");
        assert_eq!(t.replaygain_track_gain, None, "{ext}");
        assert_eq!(
            t.title.as_deref(),
            Some("Grid Failure"),
            "nothing else went with it"
        );
        assert_eq!(t.genres, GENRES[..2].to_vec());
    }
}

#[test]
fn undo_leaves_a_field_edited_since_the_write() {
    for ext in FORMATS {
        let (_d, p) = track(ext);
        let pl = plan(&p, &full());
        let written = written_map(&pl);
        write_dj_fields(&p, &pl, WriteMode::Auto).unwrap();

        // the user changes camelot by hand afterwards
        remove_dj_fields(&p, &["camelot".to_string()]).unwrap();
        write_ok(
            &p,
            &DjFields {
                camelot: Some("11B".into()),
                ..Default::default()
            },
        );

        let fields: Vec<String> = pl.gaps().iter().map(|s| s.to_string()).collect();
        let res = undo_fields(&p, &fields, &written).unwrap();
        assert!(!res.removed.contains(&"camelot".to_string()), "{ext}");
        assert!(res.removed.contains(&"bpm".to_string()), "{ext}");
        assert_eq!(read_tags(&p).camelot.as_deref(), Some("11B"), "{ext}");
    }
}

#[test]
fn read_and_remove_dj_fields_round_trip() {
    let (_d, p) = track("m4a");
    assert!(read_dj_fields(&p).is_empty());
    write_ok(&p, &full());
    let cur = read_dj_fields(&p);
    assert!((cur.bpm.unwrap() - 128.02).abs() < 0.01);
    assert_eq!(cur.camelot.as_deref(), Some("8A"));

    let out = remove_dj_fields(&p, &["bpm".to_string(), "camelot".to_string()]).unwrap();
    assert_eq!(out.status, WriteStatus::Written);
    assert!(out.frames.contains(&"tmpo".to_string()));
    let cur = read_dj_fields(&p);
    assert_eq!(cur.bpm, None);
    assert_eq!(cur.camelot, None);
    assert_eq!(cur.initial_key.as_deref(), Some("Am"));

    // removing what is not there is a no-op
    let again = remove_dj_fields(&p, &["bpm".to_string()]).unwrap();
    assert_eq!(again.status, WriteStatus::Skipped);
}

// ---------------------------------------------------------------------------
// real library timing (manual)
// ---------------------------------------------------------------------------

/// Reads 200 real files read-only and prints throughput:
/// `cargo test -p bc-media --release -- --ignored --nocapture real_library_sample`
#[test]
#[ignore]
fn real_library_sample() {
    use std::time::Instant;
    let root = Path::new("/mnt/storage/bandcamp");
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if tags::is_audio_path(&p) {
                files.push(p);
                if files.len() >= 200 {
                    break;
                }
            }
        }
        if files.len() >= 200 {
            break;
        }
    }
    if files.is_empty() {
        eprintln!("no files under {}", root.display());
        return;
    }
    let t0 = Instant::now();
    let mut ok = 0;
    let mut art = 0;
    for f in &files {
        let t = read_tags(f);
        if t.duration_ms.is_some() {
            ok += 1;
        }
        if t.has_art {
            art += 1;
        }
    }
    let cold = t0.elapsed();
    let t1 = Instant::now();
    for f in &files {
        let _ = read_tags(f);
    }
    let warm = t1.elapsed();
    println!(
        "read_tags: {} files, {} with duration, {} with art; cold {:.2}s ({:.0} files/s), warm {:.2}s ({:.0} files/s)",
        files.len(),
        ok,
        art,
        cold.as_secs_f64(),
        files.len() as f64 / cold.as_secs_f64(),
        warm.as_secs_f64(),
        files.len() as f64 / warm.as_secs_f64()
    );
}
