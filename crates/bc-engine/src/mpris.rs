//! MPRIS / media keys (`playerctl`) through `souvlaki`. The OS controls only
//! ever send [`PlayerCommand`]s to the session; the session's published state
//! is mirrored out. Failure to reach D-Bus is logged and ignored.

use crate::service::PlayerHandle;
use crate::session::Publisher;
use bc_types::player::*;

/// Start the MPRIS endpoint and return the publisher that feeds it. `base_url` is this server,
/// which library covers (`/api/art/...`) are relative to.
#[cfg(feature = "mpris")]
pub fn start(handle: PlayerHandle, base_url: String) -> Option<Box<dyn Publisher>> {
    imp::start(handle, base_url)
}

#[cfg(not(feature = "mpris"))]
pub fn start(_handle: PlayerHandle, _base_url: String) -> Option<Box<dyn Publisher>> {
    None
}

// ---- covers -------------------------------------------------------------------------------
//
// souvlaki's macOS backend turns the cover URL into an `NSImage` with no nil check and aborts the
// whole process when the image does not load: a relative path, a 404, no network, a space in a
// file path. So a cover reaches it only as a local file of bytes already known to be an image.

/// Where a queue item's cover can be fetched from: absolute as is, server paths against `base`.
pub fn absolute_art(art: &str, base: &str) -> Option<String> {
    let art = art.trim();
    if art.starts_with("https://") || art.starts_with("http://") {
        Some(art.to_string())
    } else if art.starts_with('/') && !art.starts_with("//") {
        Some(format!("{}{art}", base.trim_end_matches('/')))
    } else {
        None
    }
}

/// The file extension for image bytes the OS can decode, or `None` for anything else.
pub fn image_ext(bytes: &[u8]) -> Option<&'static str> {
    match bytes {
        [0xFF, 0xD8, 0xFF, ..] => Some("jpg"),
        [0x89, b'P', b'N', b'G', ..] => Some("png"),
        [b'G', b'I', b'F', b'8', ..] => Some("gif"),
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => Some("webp"),
        _ => None,
    }
}

/// A `file://` URL for a path, percent-encoded so `NSURL URLWithString` accepts it.
pub fn file_url(path: &std::path::Path) -> String {
    let mut out = String::from("file://");
    for b in path.to_string_lossy().bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// What the endpoint shows for a state.
#[derive(Debug, Clone, PartialEq)]
pub struct Shown {
    pub uid: Option<u64>,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub art: Option<String>,
    pub duration_s: Option<f64>,
    pub status: PlayerStatus,
}

pub fn shown(s: &PlayerState) -> Shown {
    let c = s.current.as_ref();
    Shown {
        uid: c.map(|c| c.uid),
        title: c.map(|c| c.title.clone()).unwrap_or_default(),
        artist: c.and_then(|c| c.artist.clone()).unwrap_or_default(),
        album: c.and_then(|c| c.album.clone()).unwrap_or_default(),
        art: c.and_then(|c| c.art_url.clone()),
        duration_s: c.and_then(|c| c.duration_s()),
        status: s.status,
    }
}

#[cfg(feature = "mpris")]
mod imp {
    use super::*;
    use crossbeam_channel::{Sender, unbounded};
    use souvlaki::{MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, MediaPosition, PlatformConfig, SeekDirection};
    use std::time::{Duration, Instant};

    enum Msg {
        State(Shown),
        Clock { position_s: f64 },
    }

    struct MprisPublisher {
        tx: Sender<Msg>,
    }

    impl Publisher for MprisPublisher {
        fn state(&self, s: &PlayerState) {
            let _ = self.tx.send(Msg::State(shown(s)));
        }
        fn clock(&self, c: &Clock) {
            let _ = self.tx.send(Msg::Clock { position_s: c.position_s });
        }
        fn transition(&self, _t: Option<&TransitionState>) {}
    }

    /// Fetch a cover and keep it as a local file the OS controls can load, or `None`.
    fn local_cover(art: &str, base: &str, client: &reqwest::blocking::Client, dir: &std::path::Path) -> Option<std::path::PathBuf> {
        let url = absolute_art(art, base)?;
        let res = client.get(&url).send().ok()?.error_for_status().ok()?;
        let bytes = res.bytes().ok()?;
        let ext = image_ext(&bytes)?;
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        url.hash(&mut h);
        let path = dir.join(format!("cover-{:016x}.{ext}", h.finish()));
        std::fs::create_dir_all(dir).ok()?;
        std::fs::write(&path, &bytes).ok()?;
        Some(path)
    }

    pub fn start(handle: PlayerHandle, base_url: String) -> Option<Box<dyn Publisher>> {
        let (tx, rx) = unbounded::<Msg>();
        let h = handle.clone();
        std::thread::Builder::new()
            .name("bc-mpris".into())
            .spawn(move || {
                // `org.mpris.MediaPlayer2.<name>`; a second instance (dev next to prod) sets BC_MPRIS_NAME
                let name = std::env::var("BC_MPRIS_NAME").unwrap_or_else(|_| "bc_rust".into());
                let config = PlatformConfig { dbus_name: &name, display_name: "bc", hwnd: None };
                let mut controls = match MediaControls::new(config) {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!("MPRIS unavailable: {e:?}");
                        return;
                    }
                };
                let hh = h.clone();
                let attached = controls.attach(move |ev: MediaControlEvent| {
                    let cmd = match ev {
                        MediaControlEvent::Play => PlayerCommand::Play,
                        MediaControlEvent::Pause => PlayerCommand::Pause,
                        MediaControlEvent::Toggle => PlayerCommand::Toggle,
                        MediaControlEvent::Next => PlayerCommand::Next,
                        MediaControlEvent::Previous => PlayerCommand::Previous,
                        MediaControlEvent::Stop => PlayerCommand::Pause,
                        MediaControlEvent::Seek(SeekDirection::Forward) => PlayerCommand::SeekRelative { delta_s: 10.0 },
                        MediaControlEvent::Seek(SeekDirection::Backward) => PlayerCommand::SeekRelative { delta_s: -10.0 },
                        MediaControlEvent::SeekBy(SeekDirection::Forward, d) => PlayerCommand::SeekRelative { delta_s: d.as_secs_f64() },
                        MediaControlEvent::SeekBy(SeekDirection::Backward, d) => PlayerCommand::SeekRelative { delta_s: -d.as_secs_f64() },
                        MediaControlEvent::SetPosition(MediaPosition(d)) => PlayerCommand::Seek { seconds: d.as_secs_f64() },
                        _ => return,
                    };
                    hh.send(cmd);
                });
                if let Err(e) = attached {
                    tracing::warn!("MPRIS attach failed: {e:?}");
                    return;
                }
                tracing::info!("MPRIS endpoint org.mpris.MediaPlayer2.{name} up");
                let covers = std::env::temp_dir().join("bc-now-playing");
                let client = reqwest::blocking::Client::builder().timeout(Duration::from_secs(5)).build().ok();
                // The cover file on show; replaced (and the old one removed) when the track changes.
                let mut cover: Option<std::path::PathBuf> = None;
                let mut last: Option<Shown> = None;
                let mut position = 0.0f64;
                let mut last_progress = Instant::now() - Duration::from_secs(5);
                let playback = |controls: &mut MediaControls, st: PlayerStatus, pos: f64| {
                    let progress = Some(MediaPosition(Duration::from_secs_f64(pos.max(0.0))));
                    let pb = match st {
                        PlayerStatus::Playing | PlayerStatus::Loading => MediaPlayback::Playing { progress },
                        PlayerStatus::Paused | PlayerStatus::Error => MediaPlayback::Paused { progress },
                        PlayerStatus::Idle => MediaPlayback::Stopped,
                    };
                    let _ = controls.set_playback(pb);
                };
                while let Ok(msg) = rx.recv() {
                    match msg {
                        Msg::State(s) => {
                            let meta_changed = last.as_ref().map(|l| l.uid != s.uid || l.title != s.title).unwrap_or(true);
                            if meta_changed {
                                let fresh = match (&client, s.art.as_deref()) {
                                    (Some(c), Some(art)) => local_cover(art, &base_url, c, &covers),
                                    _ => None,
                                };
                                if let Some(old) = cover.take().filter(|o| Some(o) != fresh.as_ref()) {
                                    let _ = std::fs::remove_file(old);
                                }
                                cover = fresh;
                                let cover_url = cover.as_deref().map(file_url);
                                let _ = controls.set_metadata(MediaMetadata {
                                    title: Some(&s.title),
                                    artist: Some(&s.artist),
                                    album: Some(&s.album),
                                    cover_url: cover_url.as_deref(),
                                    duration: s.duration_s.map(Duration::from_secs_f64),
                                });
                            }
                            if last.as_ref().map(|l| l.status != s.status).unwrap_or(true) || meta_changed {
                                playback(&mut controls, s.status, position);
                                last_progress = Instant::now();
                            }
                            last = Some(s);
                        }
                        Msg::Clock { position_s } => {
                            position = position_s;
                            if last_progress.elapsed() >= Duration::from_secs(1)
                                && let Some(l) = &last {
                                    playback(&mut controls, l.status, position);
                                    last_progress = Instant::now();
                                }
                        }
                    }
                }
            })
            .ok()?;
        Some(Box::new(MprisPublisher { tx }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn covers_resolve_against_the_server_or_not_at_all() {
        let base = "http://127.0.0.1:8420/";
        assert_eq!(absolute_art("/api/art/release/2?size=thumb", base).as_deref(), Some("http://127.0.0.1:8420/api/art/release/2?size=thumb"));
        assert_eq!(absolute_art("https://f4.bcbits.com/img/a1_10.jpg", base).as_deref(), Some("https://f4.bcbits.com/img/a1_10.jpg"));
        assert_eq!(absolute_art("api/art/1", base), None, "no scheme and not a server path");
        assert_eq!(absolute_art("//cdn.example/x.jpg", base), None);
        assert_eq!(absolute_art("", base), None);
    }

    #[test]
    fn only_image_bytes_count() {
        assert_eq!(image_ext(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("jpg"));
        assert_eq!(image_ext(b"\x89PNG\r\n"), Some("png"));
        assert_eq!(image_ext(b"RIFF\0\0\0\0WEBPVP8 "), Some("webp"));
        assert_eq!(image_ext(b"<html>404</html>"), None);
        assert_eq!(image_ext(&[]), None);
    }

    #[test]
    fn file_urls_survive_spaces() {
        let p = std::path::Path::new("/Users/a b/Library/Application Support/cover-1.jpg");
        assert_eq!(file_url(p), "file:///Users/a%20b/Library/Application%20Support/cover-1.jpg");
    }
}
