//! Couche application : relie le moteur de lecture, la playlist, les
//! paramètres persistants et l'interface Slint.
//!
//! Toutes les méthodes sont appelées depuis le thread d'interface (via les
//! callbacks Slint et le minuteur de rafraîchissement) ; seuls les
//! callbacks de livraison d'images traversent les threads.

use crate::audio::AudioOutput;
use crate::player::state::TrackInfo;
use crate::player::{AudioSink, FrameSink, PlayerEngine};
use crate::playlist::{Playlist, PlaylistItem};
use crate::settings::Settings;
use crate::ui::{frame_to_image, MainWindow, PlaylistEntry};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel, Weak};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Vitesses proposées — doit rester aligné avec le ComboBox de `main.slint`.
pub const SPEEDS: [f64; 9] = [0.25, 0.5, 0.75, 1.0, 1.25, 1.5, 2.0, 3.0, 4.0];
/// Index de la vitesse 1.00× dans [`SPEEDS`].
pub const SPEED_NORMAL_INDEX: i32 = 3;

/// Filtres de fichiers des boîtes de dialogue.
const MEDIA_EXTENSIONS: &[&str] = &[
    "mp4", "mkv", "avi", "mov", "webm", "mpg", "mpeg", "flv", "ts", "m2ts", "wmv", "ogv", "mp3",
    "flac", "wav", "ogg", "oga", "aac", "m4a", "opus", "wma",
];
const SUBTITLE_EXTENSIONS: &[&str] = &["srt", "ass", "ssa", "vtt"];

/// État applicatif principal (vivant sur le thread d'interface).
pub struct App {
    ui: Weak<MainWindow>,
    audio: Option<AudioOutput>,
    engine: Option<PlayerEngine>,
    playlist: Playlist,
    settings: Settings,
    current_source: Option<String>,
    /// Limiteur : une seule image en vol vers le thread UI à la fois.
    ui_busy: Arc<AtomicBool>,
    /// Correspondance index de combo → index de flux, pour chaque liste.
    audio_track_streams: Vec<usize>,
    subtitle_track_streams: Vec<usize>,
    sub_delay_secs: f64,
    muted: bool,
    speed_index: i32,
}

impl App {
    pub fn new(ui: Weak<MainWindow>) -> Self {
        let settings = Settings::load();
        let audio = match AudioOutput::new() {
            Ok(out) => Some(out),
            Err(e) => {
                log::warn!("audio désactivé : {e}");
                None
            }
        };
        let app = Self {
            ui,
            audio,
            engine: None,
            playlist: Playlist::default(),
            current_source: None,
            ui_busy: Arc::new(AtomicBool::new(false)),
            audio_track_streams: Vec::new(),
            subtitle_track_streams: Vec::new(),
            sub_delay_secs: settings.subtitle_delay_secs as f64,
            muted: false,
            speed_index: SPEED_NORMAL_INDEX,
            settings,
        };
        if let Some(ui) = app.ui.upgrade() {
            ui.set_volume(app.settings.volume);
            ui.set_dark(app.settings.dark_theme);
            ui.set_sub_delay_text(format_delay(app.sub_delay_secs));
            ui.set_eq_frequencies(string_model(
                crate::decoder::EQ_FREQUENCIES
                    .iter()
                    .map(|f| {
                        if *f >= 1000 {
                            format!("{}k", f / 1000)
                        } else {
                            f.to_string()
                        }
                    })
                    .collect(),
            ));
            ui.set_eq_gains(float_model(&app.settings.equalizer_gains));
        }
        app
    }

    // ---- Ouverture de médias --------------------------------------------

    /// Ouvre l'entrée courante de la playlist.
    fn open_current(&mut self) {
        let Some(item) = self.playlist.current().cloned() else {
            return;
        };
        self.open_source(&item.source, &item.title);
        self.refresh_playlist_model();
    }

    fn open_source(&mut self, source: &str, title: &str) {
        self.stop_current(true);

        let resume = self.settings.resume_position(source);
        let sink = self.make_frame_sink();
        let audio_sink = self.audio.as_ref().map(|a| AudioSink {
            queue: a.queue(),
            sample_rate: a.sample_rate(),
        });

        let engine = PlayerEngine::open(source, audio_sink, sink, resume);
        if let Some(out) = &self.audio {
            out.attach(Arc::clone(&engine.shared));
        }

        // Applique l'état utilisateur courant à la nouvelle session.
        engine.set_volume(self.settings.volume);
        engine.set_muted(self.muted);
        engine.set_speed(SPEEDS[self.speed_index as usize]);
        engine.set_subtitle_delay(self.sub_delay_secs);
        engine.set_equalizer(self.settings.equalizer_gains);

        self.engine = Some(engine);
        self.current_source = Some(source.to_string());
        self.settings.push_history(source);
        self.audio_track_streams.clear();
        self.subtitle_track_streams.clear();

        if let Some(ui) = self.ui.upgrade() {
            ui.set_media_title(title.into());
            ui.set_media_loaded(true);
            ui.set_status_text(
                match resume {
                    Some(us) => format!("Reprise à {}", crate::utils::format_time(us)),
                    None => format!("Lecture : {title}"),
                }
                .into(),
            );
            ui.set_audio_tracks(string_model(vec![]));
            ui.set_subtitle_tracks(string_model(vec!["Désactivés".into()]));
            ui.set_subtitle_track_index(0);
        }
        log::info!("ouverture de {source}");
    }

    /// Callback de livraison d'images : décodage → thread UI, avec
    /// régulation (jamais plus d'une image en attente côté UI).
    fn make_frame_sink(&self) -> FrameSink {
        let weak = self.ui.clone();
        let busy = Arc::clone(&self.ui_busy);
        Box::new(move |frame| {
            if busy.swap(true, Ordering::AcqRel) {
                return; // L'UI n'a pas fini la précédente : image sautée.
            }
            let weak = weak.clone();
            let busy_done = Arc::clone(&busy);
            let result = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    ui.set_video_frame(frame_to_image(&frame));
                }
                busy_done.store(false, Ordering::Release);
            });
            if result.is_err() {
                busy.store(false, Ordering::Release);
            }
        })
    }

    /// Arrête la session courante en mémorisant la position de reprise.
    fn stop_current(&mut self, remember: bool) {
        if let Some(engine) = self.engine.take() {
            if remember {
                if let Some(source) = &self.current_source {
                    self.settings.remember_position(
                        source,
                        engine.position_us(),
                        engine.duration_us(),
                    );
                }
            }
            if let Some(out) = &self.audio {
                out.detach();
            }
            drop(engine);
        }
        self.current_source = None;
        if let Some(ui) = self.ui.upgrade() {
            ui.set_has_video(false);
            ui.set_media_loaded(false);
            ui.set_playing(false);
            ui.set_progress(0.0);
            ui.set_position_text("0:00".into());
            ui.set_duration_text("0:00".into());
            ui.set_subtitle_text("".into());
        }
    }

    // ---- Contrôles -------------------------------------------------------

    pub fn play_pause(&mut self) {
        match &self.engine {
            Some(engine) => engine.toggle_pause(),
            None => {
                // Rien d'ouvert : lance la playlist.
                if self.playlist.current().is_none() {
                    self.playlist.advance();
                }
                self.open_current();
            }
        }
    }

    pub fn stop(&mut self) {
        self.stop_current(true);
        if let Some(ui) = self.ui.upgrade() {
            ui.set_status_text("Arrêté".into());
        }
    }

    pub fn seek_fraction(&mut self, fraction: f32) {
        if let Some(engine) = &self.engine {
            let duration = engine.duration_us();
            if duration > 0 {
                engine.seek((duration as f64 * fraction.clamp(0.0, 1.0) as f64) as i64);
            }
        }
    }

    pub fn seek_relative(&mut self, delta_secs: f32) {
        if let Some(engine) = &self.engine {
            engine.seek_relative(delta_secs as f64);
        }
    }

    pub fn next(&mut self) {
        if self.playlist.advance().is_some() {
            self.open_current();
        }
    }

    pub fn previous(&mut self) {
        if self.playlist.previous().is_some() {
            self.open_current();
        }
    }

    pub fn set_volume(&mut self, volume: f32) {
        self.settings.volume = volume.clamp(0.0, 1.25);
        if let Some(engine) = &self.engine {
            engine.set_volume(self.settings.volume);
        }
        if let Some(ui) = self.ui.upgrade() {
            ui.set_volume(self.settings.volume);
        }
    }

    pub fn toggle_mute(&mut self) {
        self.muted = !self.muted;
        if let Some(engine) = &self.engine {
            engine.set_muted(self.muted);
        }
        if let Some(ui) = self.ui.upgrade() {
            ui.set_muted(self.muted);
        }
    }

    pub fn set_speed_index(&mut self, index: i32) {
        let index = index.clamp(0, SPEEDS.len() as i32 - 1);
        self.speed_index = index;
        if let Some(engine) = &self.engine {
            engine.set_speed(SPEEDS[index as usize]);
        }
    }

    pub fn toggle_fullscreen(&mut self) {
        if let Some(ui) = self.ui.upgrade() {
            let fullscreen = !ui.get_fullscreen();
            ui.window().set_fullscreen(fullscreen);
            ui.set_fullscreen(fullscreen);
        }
    }

    pub fn take_screenshot(&mut self) {
        let Some(engine) = &self.engine else { return };
        let frame = engine.shared.last_frame.lock().unwrap().clone();
        let Some(frame) = frame else {
            self.set_status("Aucune image à capturer");
            return;
        };
        match crate::video::save_screenshot(&frame) {
            Ok(path) => self.set_status(&format!("Capture : {}", path.display())),
            Err(e) => self.set_status(&format!("Capture impossible : {e}")),
        }
    }

    pub fn toggle_theme(&mut self) {
        self.settings.dark_theme = !self.settings.dark_theme;
        if let Some(ui) = self.ui.upgrade() {
            ui.set_dark(self.settings.dark_theme);
        }
    }

    // ---- Playlist ----------------------------------------------------------

    /// Ajoute des sources (fichiers ou URL) ; lance la lecture si rien
    /// n'est en cours.
    pub fn add_sources(&mut self, sources: Vec<String>) {
        if sources.is_empty() {
            return;
        }
        let first_added = self.playlist.len();
        for source in sources {
            self.playlist.add(PlaylistItem::new(source));
        }
        self.refresh_playlist_model();
        if self.engine.is_none() {
            self.playlist.select(first_added);
            self.open_current();
        }
    }

    pub fn add_files_dialog(&mut self) {
        let files = rfd::FileDialog::new()
            .set_title("Ouvrir des médias")
            .add_filter("Médias", MEDIA_EXTENSIONS)
            .add_filter("Tous les fichiers", &["*"])
            .pick_files()
            .unwrap_or_default();
        self.add_sources(
            files
                .into_iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect(),
        );
    }

    pub fn open_url(&mut self, url: &str) {
        let url = url.trim();
        if url.is_empty() {
            return;
        }
        if !crate::streaming::is_url(url) {
            self.set_status("URL non reconnue (http, https, rtsp, udp…)");
            return;
        }
        let index = self.playlist.add(PlaylistItem::new(url));
        self.playlist.select(index);
        self.open_current();
    }

    pub fn playlist_activate(&mut self, index: usize) {
        if self.playlist.select(index).is_some() {
            self.open_current();
        }
    }

    pub fn playlist_remove(&mut self, index: usize) {
        let was_current = self.playlist.current_index() == Some(index);
        self.playlist.remove(index);
        if was_current {
            self.stop_current(true);
        }
        self.refresh_playlist_model();
    }

    pub fn playlist_shift(&mut self, index: usize, delta: i32) {
        self.playlist.shift(index, delta);
        self.refresh_playlist_model();
    }

    pub fn playlist_save_dialog(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .set_title("Enregistrer la playlist")
            .add_filter("Playlist M3U", &["m3u", "m3u8"])
            .set_file_name("playlist.m3u")
            .save_file()
        else {
            return;
        };
        match self.playlist.save_m3u(&path) {
            Ok(()) => self.set_status(&format!("Playlist enregistrée : {}", path.display())),
            Err(e) => self.set_status(&format!("Échec d'enregistrement : {e}")),
        }
    }

    pub fn playlist_load_dialog(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .set_title("Charger une playlist")
            .add_filter("Playlist M3U", &["m3u", "m3u8"])
            .pick_file()
        else {
            return;
        };
        match self.playlist.load_m3u(&path) {
            Ok(n) => {
                self.set_status(&format!("{n} entrées chargées"));
                self.stop_current(true);
                self.refresh_playlist_model();
            }
            Err(e) => self.set_status(&format!("Échec de chargement : {e}")),
        }
    }

    fn refresh_playlist_model(&self) {
        let Some(ui) = self.ui.upgrade() else { return };
        let current = self.playlist.current_index();
        let entries: Vec<PlaylistEntry> = self
            .playlist
            .items()
            .iter()
            .enumerate()
            .map(|(i, item)| PlaylistEntry {
                title: item.title.clone().into(),
                is_current: Some(i) == current,
            })
            .collect();
        ui.set_playlist_entries(ModelRc::from(Rc::new(VecModel::from(entries))));
    }

    // ---- Pistes & sous-titres ---------------------------------------------

    pub fn select_audio_track(&mut self, combo_index: i32) {
        if let (Some(engine), Some(&stream)) = (
            &self.engine,
            self.audio_track_streams.get(combo_index.max(0) as usize),
        ) {
            engine.select_audio_track(stream);
        }
    }

    pub fn select_subtitle_track(&mut self, combo_index: i32) {
        let Some(engine) = &self.engine else { return };
        if combo_index <= 0 {
            engine.select_subtitle_track(None);
        } else if let Some(&stream) = self.subtitle_track_streams.get(combo_index as usize - 1) {
            // Une piste embarquée remplace les sous-titres externes.
            *engine.shared.external_subtitles.lock().unwrap() = None;
            engine.select_subtitle_track(Some(stream));
        }
    }

    pub fn load_subtitle_dialog(&mut self) {
        let Some(engine) = &self.engine else {
            self.set_status("Ouvrez d'abord un média");
            return;
        };
        let Some(path) = rfd::FileDialog::new()
            .set_title("Charger des sous-titres")
            .add_filter("Sous-titres", SUBTITLE_EXTENSIONS)
            .pick_file()
        else {
            return;
        };
        match crate::subtitles::load_file(&path) {
            Ok(track) => {
                let count = track.len();
                *engine.shared.external_subtitles.lock().unwrap() = Some(track);
                self.set_status(&format!("{count} sous-titres chargés"));
            }
            Err(e) => self.set_status(&format!("Sous-titres illisibles : {e}")),
        }
    }

    pub fn adjust_subtitle_delay(&mut self, delta_secs: f32) {
        self.sub_delay_secs = (self.sub_delay_secs + delta_secs as f64).clamp(-30.0, 30.0);
        self.settings.subtitle_delay_secs = self.sub_delay_secs as f32;
        if let Some(engine) = &self.engine {
            engine.set_subtitle_delay(self.sub_delay_secs);
        }
        if let Some(ui) = self.ui.upgrade() {
            ui.set_sub_delay_text(format_delay(self.sub_delay_secs));
        }
    }

    // ---- Égaliseur --------------------------------------------------------

    /// Modifie le gain d'une bande de l'égaliseur (dB), l'applique en direct
    /// et le persiste.
    pub fn set_equalizer_band(&mut self, band: i32, gain_db: f32) {
        let band = band.max(0) as usize;
        if band >= 10 {
            return;
        }
        let gain = gain_db.clamp(-12.0, 12.0);
        self.settings.equalizer_gains[band] = gain;
        if let Some(engine) = &self.engine {
            engine.set_equalizer_band(band, gain);
        }
    }

    /// Remet l'égaliseur à plat (toutes les bandes à 0 dB).
    pub fn reset_equalizer(&mut self) {
        self.settings.equalizer_gains = [0.0; 10];
        if let Some(engine) = &self.engine {
            engine.set_equalizer([0.0; 10]);
        }
        if let Some(ui) = self.ui.upgrade() {
            ui.set_eq_gains(float_model(&self.settings.equalizer_gains));
        }
    }

    // ---- Rafraîchissement périodique ---------------------------------------

    /// Appelé ~10×/s par le minuteur Slint : synchronise l'interface avec
    /// l'état du moteur et gère l'enchaînement automatique de la playlist.
    pub fn tick(&mut self) {
        let Some(ui) = self.ui.upgrade() else { return };
        let Some(engine) = &self.engine else { return };
        // Copie locale pour libérer l'emprunt de `self.engine` (les méthodes
        // appelées plus bas reprennent `&mut self`).
        let shared = Arc::clone(&engine.shared);
        let duration = engine.duration_us();
        let mut position = engine.position_us();
        let paused = engine.is_paused();

        // Erreur fatale d'un thread → on l'affiche et on passe au suivant.
        if let Some(error) = shared.error.lock().unwrap().take() {
            ui.set_status_text(error.into());
        }

        if duration > 0 {
            position = position.min(duration);
        }

        ui.set_playing(!paused);
        ui.set_has_video(shared.has_video.load(Ordering::Relaxed));
        ui.set_seekable(duration > 0);
        ui.set_position_text(crate::utils::format_time(position).into());
        ui.set_duration_text(
            if duration > 0 {
                crate::utils::format_time(duration)
            } else {
                "direct".to_string()
            }
            .into(),
        );
        if duration > 0 {
            ui.set_progress((position as f32 / duration as f32).clamp(0.0, 1.0));
        }

        // Sous-titres (texte + style ASS : alignement, gras, italique,
        // couleur).
        let subtitle = shared.subtitle_at(position).unwrap_or_default();
        if !subtitle.is_empty() {
            let style = shared.subtitle_style_at(position);
            ui.set_subtitle_align(style.align as i32);
            ui.set_subtitle_bold(style.bold);
            ui.set_subtitle_italic(style.italic);
            ui.set_subtitle_color(
                style
                    .color
                    .map(slint_color)
                    .unwrap_or_else(|| slint::Color::from_rgb_u8(255, 255, 255)),
            );
        }
        if ui.get_subtitle_text() != subtitle.as_str() {
            ui.set_subtitle_text(subtitle.into());
        }

        self.refresh_track_lists(&ui);

        // Fin du média : enchaîne sur la playlist.
        if shared.playback_finished() {
            if let Some(source) = &self.current_source {
                // Lecture terminée : on oublie la position de reprise.
                self.settings
                    .remember_position(source, duration, duration.max(1));
            }
            if self.playlist.advance().is_some() {
                self.open_current();
            } else {
                self.stop_current(false);
                ui.set_status_text("Fin de la liste de lecture".into());
            }
        }
    }

    /// Met à jour les combos de pistes quand le demuxeur les a découvertes.
    fn refresh_track_lists(&mut self, ui: &MainWindow) {
        let Some(engine) = &self.engine else { return };

        let audio_tracks = engine.shared.audio_tracks.lock().unwrap().clone();
        if audio_tracks.len() != self.audio_track_streams.len() {
            self.audio_track_streams = audio_tracks.iter().map(|t| t.stream_index).collect();
            ui.set_audio_tracks(track_labels(&audio_tracks));
            ui.set_audio_track_index(0);
        }

        let sub_tracks = engine.shared.subtitle_tracks.lock().unwrap().clone();
        if sub_tracks.len() != self.subtitle_track_streams.len() {
            self.subtitle_track_streams = sub_tracks.iter().map(|t| t.stream_index).collect();
            let mut labels = vec!["Désactivés".to_string()];
            labels.extend(sub_tracks.iter().enumerate().map(|(i, t)| t.label(i)));
            ui.set_subtitle_tracks(string_model(labels));
            ui.set_subtitle_track_index(0);
        }
    }

    fn set_status(&self, message: &str) {
        if let Some(ui) = self.ui.upgrade() {
            ui.set_status_text(message.into());
        }
        log::info!("{message}");
    }

    /// À appeler avant de quitter : persiste position et paramètres.
    pub fn shutdown(&mut self) {
        self.stop_current(true);
        self.settings.save();
    }
}

/// Convertit une liste de chaînes en modèle Slint.
fn string_model(items: Vec<String>) -> ModelRc<SharedString> {
    ModelRc::from(Rc::new(VecModel::from(
        items
            .into_iter()
            .map(SharedString::from)
            .collect::<Vec<_>>(),
    )))
}

fn track_labels(tracks: &[TrackInfo]) -> ModelRc<SharedString> {
    string_model(tracks.iter().enumerate().map(|(i, t)| t.label(i)).collect())
}

/// Convertit un tableau de gains en modèle Slint (pour les sliders de l'EQ).
fn float_model(values: &[f32]) -> ModelRc<f32> {
    ModelRc::from(Rc::new(VecModel::from(values.to_vec())))
}

/// Convertit une couleur `0xRRGGBB` en couleur Slint.
fn slint_color(rgb: u32) -> slint::Color {
    slint::Color::from_rgb_u8(
        ((rgb >> 16) & 0xff) as u8,
        ((rgb >> 8) & 0xff) as u8,
        (rgb & 0xff) as u8,
    )
}

fn format_delay(secs: f64) -> SharedString {
    format!("{secs:+.1} s").into()
}
