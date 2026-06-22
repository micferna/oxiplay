//! Contrôles média du bureau (MPRIS sous Linux, SMTC sous Windows, Now Playing
//! sous macOS) via `souvlaki` : touches multimédia du clavier (lecture/pause,
//! suivant, précédent…) et affichage du média en cours dans le bureau.
//!
//! Le gestionnaire d'événements de `souvlaki` s'exécute sur son propre thread ;
//! on y pousse simplement les événements dans un canal, drainé par le thread
//! d'interface (voir `App::tick`). Si l'initialisation échoue (pas de bus
//! D-Bus, plateforme non gérée…), les contrôles sont silencieusement absents.

use crossbeam_channel::{unbounded, Receiver, Sender};
use souvlaki::{MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, PlatformConfig};
use std::time::Duration;

/// Façade des contrôles média : reçoit les commandes du bureau et publie
/// l'état de lecture.
pub struct MediaKeys {
    controls: Option<MediaControls>,
    rx: Receiver<MediaControlEvent>,
}

impl MediaKeys {
    pub fn new() -> Self {
        let (tx, rx) = unbounded();
        Self {
            controls: Self::init(tx),
            rx,
        }
    }

    fn init(tx: Sender<MediaControlEvent>) -> Option<MediaControls> {
        let config = PlatformConfig {
            dbus_name: "oxiplay",
            display_name: "OxiPlay",
            hwnd: None,
        };
        let mut controls = match MediaControls::new(config) {
            Ok(c) => c,
            Err(e) => {
                log::debug!("contrôles média du bureau indisponibles : {e:?}");
                return None;
            }
        };
        if let Err(e) = controls.attach(move |event| {
            let _ = tx.send(event);
        }) {
            log::debug!("attache des contrôles média échouée : {e:?}");
            return None;
        }
        log::info!("contrôles média du bureau actifs (MPRIS)");
        Some(controls)
    }

    /// Événements reçus depuis le dernier appel (non bloquant).
    pub fn poll(&self) -> Vec<MediaControlEvent> {
        self.rx.try_iter().collect()
    }

    /// Publie le titre (et la durée si connue) du média en cours.
    pub fn set_metadata(&mut self, title: &str, duration_us: i64) {
        if let Some(c) = &mut self.controls {
            let _ = c.set_metadata(MediaMetadata {
                title: Some(title),
                album: None,
                artist: None,
                cover_url: None,
                duration: (duration_us > 0).then(|| Duration::from_micros(duration_us as u64)),
            });
        }
    }

    /// Publie l'état de lecture (lecture / pause / arrêt).
    pub fn set_playback(&mut self, playing: bool, stopped: bool) {
        if let Some(c) = &mut self.controls {
            let playback = if stopped {
                MediaPlayback::Stopped
            } else if playing {
                MediaPlayback::Playing { progress: None }
            } else {
                MediaPlayback::Paused { progress: None }
            };
            let _ = c.set_playback(playback);
        }
    }
}

impl Default for MediaKeys {
    fn default() -> Self {
        Self::new()
    }
}
