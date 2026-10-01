//! MPRIS / media keys (`playerctl`) through `souvlaki`. The OS controls only
//! ever send [`PlayerCommand`]s to the session; the session's published state
//! is mirrored out. Failure to reach D-Bus is logged and ignored.

use crate::service::PlayerHandle;
use crate::session::Publisher;
use bc_types::player::*;

/// Start the MPRIS endpoint and return the publisher that feeds it.
#[cfg(feature = "mpris")]
pub fn start(handle: PlayerHandle) -> Option<Box<dyn Publisher>> {
    imp::start(handle)
}

#[cfg(not(feature = "mpris"))]
pub fn start(_handle: PlayerHandle) -> Option<Box<dyn Publisher>> {
    None
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

    pub fn start(handle: PlayerHandle) -> Option<Box<dyn Publisher>> {
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
                                let _ = controls.set_metadata(MediaMetadata {
                                    title: Some(&s.title),
                                    artist: Some(&s.artist),
                                    album: Some(&s.album),
                                    cover_url: s.art.as_deref(),
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
