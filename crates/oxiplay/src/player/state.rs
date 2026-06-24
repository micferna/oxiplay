//! État partagé entre le moteur de lecture, les threads de décodage,
//! la sortie audio et l'interface graphique.
//!
//! Tout est conçu pour des lectures très fréquentes et sans blocage :
//! atomiques pour les scalaires, mutex courts pour le reste.

use crate::player::clock::PlaybackClock;
use crate::subtitles::{BitmapSubtitleTrack, SubtitleTrack};
use crate::video::VideoFrameData;
use std::sync::atomic::{
    AtomicBool, AtomicI32, AtomicI64, AtomicU32, AtomicU64, AtomicU8, Ordering,
};
use std::sync::{Arc, Mutex};

/// Description d'une piste (audio ou sous-titres) du média ouvert.
#[derive(Debug, Clone)]
pub struct TrackInfo {
    /// Index du flux dans le conteneur.
    pub stream_index: usize,
    /// Langue (tag `language` des métadonnées), si présente.
    pub language: Option<String>,
    /// Titre de la piste, si présent.
    pub title: Option<String>,
    /// Nom du codec (ex. `aac`, `subrip`).
    pub codec: String,
}

/// Un chapitre du média : point de navigation horodaté.
#[derive(Debug, Clone)]
pub struct ChapterInfo {
    /// Début du chapitre (µs).
    pub start_us: i64,
    /// Titre lisible (ou « Chapitre N » par défaut).
    pub title: String,
}

impl TrackInfo {
    /// Libellé lisible pour les menus de l'interface.
    pub fn label(&self, position: usize) -> String {
        let mut label = format!("Piste {}", position + 1);
        if let Some(lang) = &self.language {
            label.push_str(&format!(" [{lang}]"));
        }
        if let Some(title) = &self.title {
            label.push_str(&format!(" — {title}"));
        }
        label.push_str(&format!(" ({})", self.codec));
        label
    }
}

/// État global d'une session de lecture, partagé par `Arc`.
pub struct SharedState {
    /// Horloge maîtresse (position de lecture).
    pub clock: PlaybackClock,
    /// Durée totale du média (µs), 0 si inconnue (flux en direct).
    pub duration_us: AtomicI64,
    /// Vitesse demandée, en millièmes (1000 = 1.0x) — lue par l'audio.
    pub speed_milli: AtomicU32,
    /// Volume en millièmes (0..=1250).
    pub volume_milli: AtomicU32,
    pub muted: AtomicBool,
    /// Demande d'arrêt de tous les threads de la session.
    pub stop: AtomicBool,
    /// Génération de seek : chaque seek l'incrémente ; les paquets/images
    /// d'une génération périmée sont jetés sans être présentés.
    pub generation: AtomicU64,
    /// Fin de demuxage atteinte.
    pub demux_eof: AtomicBool,
    /// Plus aucune image vidéo à présenter (après EOF).
    pub video_done: AtomicBool,
    /// Plus aucun échantillon audio à jouer (après EOF).
    pub audio_done: AtomicBool,
    /// Dernière image présentée (pour les captures d'écran).
    pub last_frame: Mutex<Option<Arc<VideoFrameData>>>,
    /// Décalage utilisateur des sous-titres (µs, positif = retarder).
    pub subtitle_delay_us: AtomicI64,
    /// Décalage de synchronisation audio/vidéo (µs, positif = audio retardé
    /// par rapport à la vidéo). Appliqué au PTS rapporté à l'horloge maîtresse.
    pub audio_delay_us: AtomicI64,
    /// Piste de sous-titres externe chargée manuellement (prioritaire).
    pub external_subtitles: Mutex<Option<SubtitleTrack>>,
    /// Répliques décodées depuis la piste embarquée sélectionnée.
    pub embedded_subtitles: Mutex<SubtitleTrack>,
    /// Sous-titres image (PGS/DVD) à incruster sur la vidéo.
    pub bitmap_subtitles: Mutex<BitmapSubtitleTrack>,
    /// Pistes audio disponibles.
    pub audio_tracks: Mutex<Vec<TrackInfo>>,
    /// Pistes de sous-titres embarquées disponibles.
    pub subtitle_tracks: Mutex<Vec<TrackInfo>>,
    /// Chapitres du média (navigation), vide si aucun.
    pub chapters: Mutex<Vec<ChapterInfo>>,
    /// Fiche d'informations média (texte multi-lignes : conteneur, codecs,
    /// résolution, HDR, débits…), pour l'affichage à la demande.
    pub media_info: Mutex<String>,
    /// Durée d'une image vidéo (µs), pour l'avance image par image ; 0 si
    /// inconnue (on retombe alors sur une estimation à ~24 i/s).
    pub frame_duration_us: AtomicI64,
    /// Erreur fatale remontée par un thread (affichée par l'UI).
    pub error: Mutex<Option<String>>,
    /// Présence d'un flux audio/vidéo dans le média.
    pub has_audio: AtomicBool,
    pub has_video: AtomicBool,
    /// Gains de l'égaliseur 10 bandes (dB), lus par le graphe de filtres.
    pub equalizer: Mutex<[f32; 10]>,
    /// Compteur incrémenté à chaque modification de l'égaliseur **ou** de la
    /// normalisation : permet au thread audio de détecter un changement et de
    /// reconstruire le graphe.
    pub eq_generation: AtomicU64,
    /// Normalisation du volume (filtre `dynaudnorm`) : égalise la loudness, utile
    /// quand le niveau varie fortement (chaînes IPTV, playlists hétérogènes).
    pub normalize: AtomicBool,
    /// Décodage matériel autorisé (repli logiciel si indisponible).
    pub hwaccel_enabled: AtomicBool,
    /// Rotation d'affichage : 0 = aucune, 1 = 90° horaire, 2 = 180°,
    /// 3 = 270° (90° anti-horaire). Appliquée par le filtre `transpose`.
    pub rotation: AtomicU8,
    /// Réglages d'image en millièmes, appliqués par le filtre `eq` :
    /// luminosité (0 = neutre, −1000..1000), contraste et saturation
    /// (1000 = neutre).
    pub brightness_milli: AtomicI32,
    pub contrast_milli: AtomicI32,
    pub saturation_milli: AtomicI32,
    /// Correction gamma en millièmes (1000 = neutre), appliquée par le filtre
    /// `eq`. Netteté (`unsharp`) et débruitage (`hqdn3d`) en millièmes, 0 = off.
    pub gamma_milli: AtomicI32,
    pub sharpen_milli: AtomicI32,
    pub denoise_milli: AtomicI32,
    /// Statistiques de présentation (HUD de diagnostic) : images réellement
    /// affichées, images sautées (retard), et dernier décalage A/V (µs).
    pub frames_presented: AtomicU64,
    pub frames_dropped: AtomicU64,
    pub last_av_delta_us: AtomicI64,
}

impl Default for SharedState {
    fn default() -> Self {
        Self {
            clock: PlaybackClock::default(),
            duration_us: AtomicI64::new(0),
            speed_milli: AtomicU32::new(1000),
            volume_milli: AtomicU32::new(800),
            muted: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            demux_eof: AtomicBool::new(false),
            video_done: AtomicBool::new(false),
            audio_done: AtomicBool::new(false),
            last_frame: Mutex::new(None),
            subtitle_delay_us: AtomicI64::new(0),
            audio_delay_us: AtomicI64::new(0),
            external_subtitles: Mutex::new(None),
            embedded_subtitles: Mutex::new(SubtitleTrack::default()),
            bitmap_subtitles: Mutex::new(BitmapSubtitleTrack::default()),
            audio_tracks: Mutex::new(Vec::new()),
            subtitle_tracks: Mutex::new(Vec::new()),
            chapters: Mutex::new(Vec::new()),
            media_info: Mutex::new(String::new()),
            frame_duration_us: AtomicI64::new(0),
            error: Mutex::new(None),
            has_audio: AtomicBool::new(false),
            has_video: AtomicBool::new(false),
            equalizer: Mutex::new([0.0; 10]),
            eq_generation: AtomicU64::new(0),
            normalize: AtomicBool::new(false),
            hwaccel_enabled: AtomicBool::new(true),
            rotation: AtomicU8::new(0),
            brightness_milli: AtomicI32::new(0),
            contrast_milli: AtomicI32::new(1000),
            saturation_milli: AtomicI32::new(1000),
            gamma_milli: AtomicI32::new(1000),
            sharpen_milli: AtomicI32::new(0),
            denoise_milli: AtomicI32::new(0),
            frames_presented: AtomicU64::new(0),
            frames_dropped: AtomicU64::new(0),
            last_av_delta_us: AtomicI64::new(0),
        }
    }
}

impl SharedState {
    pub fn speed(&self) -> f64 {
        self.speed_milli.load(Ordering::Relaxed) as f64 / 1000.0
    }

    /// Gains courants de l'égaliseur (dB).
    pub fn equalizer_gains(&self) -> [f32; 10] {
        *self.equalizer.lock().unwrap()
    }

    /// Remplace tous les gains de l'égaliseur et invalide le graphe audio.
    pub fn set_equalizer(&self, gains: [f32; 10]) {
        *self.equalizer.lock().unwrap() = gains;
        self.eq_generation.fetch_add(1, Ordering::Release);
    }

    /// Normalisation du volume active ?
    pub fn normalize_enabled(&self) -> bool {
        self.normalize.load(Ordering::Relaxed)
    }

    /// (Dés)active la normalisation et invalide le graphe audio (réutilise le
    /// compteur de l'égaliseur pour forcer la reconstruction).
    pub fn set_normalize(&self, on: bool) {
        self.normalize.store(on, Ordering::Relaxed);
        self.eq_generation.fetch_add(1, Ordering::Release);
    }

    /// Modifie le gain d'une bande (dB, borné à ±12) et invalide le graphe.
    pub fn set_equalizer_band(&self, band: usize, gain_db: f32) {
        if band >= 10 {
            return;
        }
        self.equalizer.lock().unwrap()[band] = gain_db.clamp(-12.0, 12.0);
        self.eq_generation.fetch_add(1, Ordering::Release);
    }

    pub fn set_speed(&self, speed: f64) {
        let speed = speed.clamp(0.25, 4.0);
        self.speed_milli
            .store((speed * 1000.0).round() as u32, Ordering::Relaxed);
        self.clock.set_speed(speed);
    }

    /// Gain audio effectif (0.0 si muet).
    pub fn effective_volume(&self) -> f32 {
        if self.muted.load(Ordering::Relaxed) {
            0.0
        } else {
            self.volume_milli.load(Ordering::Relaxed) as f32 / 1000.0
        }
    }

    pub fn current_generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    pub fn should_stop(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    pub fn set_error(&self, message: impl Into<String>) {
        let message = message.into();
        log::error!("{message}");
        *self.error.lock().unwrap() = Some(message);
    }

    /// Texte de sous-titre à afficher à la position donnée, en tenant
    /// compte du décalage utilisateur. La piste externe a priorité.
    pub fn subtitle_at(&self, position_us: i64) -> Option<String> {
        let t = position_us - self.subtitle_delay_us.load(Ordering::Relaxed);
        if let Some(track) = self.external_subtitles.lock().unwrap().as_ref() {
            return track.query(t);
        }
        self.embedded_subtitles.lock().unwrap().query(t)
    }

    /// Style (alignement, gras, italique, couleur) du sous-titre actif.
    pub fn subtitle_style_at(&self, position_us: i64) -> crate::subtitles::CueStyle {
        let t = position_us - self.subtitle_delay_us.load(Ordering::Relaxed);
        if let Some(track) = self.external_subtitles.lock().unwrap().as_ref() {
            return track.query_style(t);
        }
        self.embedded_subtitles.lock().unwrap().query_style(t)
    }

    /// Vrai quand la lecture est entièrement terminée (EOF partout).
    pub fn playback_finished(&self) -> bool {
        if !self.demux_eof.load(Ordering::Relaxed) {
            return false;
        }
        let video_ok =
            !self.has_video.load(Ordering::Relaxed) || self.video_done.load(Ordering::Relaxed);
        let audio_ok =
            !self.has_audio.load(Ordering::Relaxed) || self.audio_done.load(Ordering::Relaxed);
        video_ok && audio_ok
    }
}
