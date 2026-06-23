//! Couche application : relie le moteur de lecture, la playlist, les
//! paramètres persistants et l'interface Slint.
//!
//! Toutes les méthodes sont appelées depuis le thread d'interface (via les
//! callbacks Slint et le minuteur de rafraîchissement) ; seuls les
//! callbacks de livraison d'images traversent les threads.

use crate::audio::AudioOutput;
use crate::inhibit::Inhibitor;
use crate::media_controls::MediaKeys;
use crate::player::state::TrackInfo;
use crate::player::{AudioSink, FrameSink, PlayerEngine};
use crate::playlist::{Playlist, PlaylistItem, RepeatMode};
use crate::settings::{MediaState, Settings};
use crate::subtitles::SubtitleTrack;
use crate::ui::{frame_to_image, MainWindow, PlaylistEntry};
use crate::update::UpdateChecker;
use crossbeam_channel::{unbounded, Receiver, Sender};
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
    "flac", "wav", "ogg", "oga", "aac", "m4a", "opus", "wma", "iso",
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
    /// Nombre de chapitres actuellement reflétés dans l'UI (détection de
    /// changement de média).
    chapter_count: usize,
    sub_delay_secs: f64,
    muted: bool,
    speed_index: i32,
    /// Taille de fenêtre (logique) avant le passage en mini-lecteur.
    pre_mini_size: Option<(f32, f32)>,
    /// Mode de répétition courant (boucle off / liste / média).
    repeat_mode: RepeatMode,
    /// Décalage de synchronisation audio/vidéo (secondes).
    audio_delay_secs: f64,
    /// Inhibiteur de mise en veille / d'économiseur d'écran (actif en lecture).
    inhibitor: Inhibitor,
    /// Contrôles média du bureau (touches multimédia, MPRIS/SMTC/Now Playing).
    media_keys: MediaKeys,
    /// État pour le calcul du FPS du HUD de statistiques.
    last_stats_at: std::time::Instant,
    last_frames_presented: u64,
    last_frames_dropped: u64,
    /// Sélections de pistes à restaurer dès que le demuxeur a découvert les
    /// pistes du média rouvert (mémoire par fichier).
    pending_audio_track: Option<i32>,
    pending_subtitle_track: Option<i32>,
    /// Noms des périphériques de sortie audio (index de combo → nom).
    audio_device_names: Vec<String>,
    /// Vérificateur de mise à jour (lancé au démarrage selon les réglages).
    update_checker: UpdateChecker,
    /// URL de la release disponible, le cas échéant.
    update_url: Option<String>,
    /// Canal de livraison des sous-titres téléchargés en ligne (thread → UI).
    subtitle_dl_tx: Sender<Option<SubtitleTrack>>,
    subtitle_dl_rx: Receiver<Option<SubtitleTrack>>,
}

/// Préréglages d'égaliseur (gains dB, ordre [`crate::decoder::EQ_FREQUENCIES`] :
/// 31 62 125 250 500 1k 2k 4k 8k 16k). L'index 0 « Manuel » conserve les
/// réglages courants ; l'ordre doit suivre le ComboBox de `main.slint`.
const EQ_PRESETS: &[[f32; 10]] = &[
    [0.0; 10],                                             // Manuel (no-op)
    [0.0; 10],                                             // Plat
    [5.0, 4.0, 3.0, 1.0, -0.5, -0.5, 1.0, 3.0, 4.0, 4.5],  // Rock
    [-1.0, 0.0, 1.0, 2.5, 3.5, 3.5, 2.0, 0.5, -0.5, -1.0], // Pop
    [3.0, 2.0, 1.0, 2.0, -1.0, -1.0, 0.0, 1.0, 2.0, 3.0],  // Jazz
    [4.0, 3.0, 2.0, 1.0, -0.5, -0.5, 0.0, 1.5, 2.5, 3.5],  // Classique
    [6.0, 5.0, 4.0, 2.5, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0],    // Graves+
    [-2.0, -1.0, 0.0, 2.0, 4.0, 4.0, 3.0, 1.5, 0.5, 0.0],  // Voix
];

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
        let (subtitle_dl_tx, subtitle_dl_rx) = unbounded();
        let app = Self {
            ui,
            audio,
            engine: None,
            playlist: Playlist::default(),
            current_source: None,
            ui_busy: Arc::new(AtomicBool::new(false)),
            audio_track_streams: Vec::new(),
            subtitle_track_streams: Vec::new(),
            chapter_count: 0,
            sub_delay_secs: settings.subtitle_delay_secs as f64,
            muted: false,
            speed_index: SPEED_NORMAL_INDEX,
            pre_mini_size: None,
            repeat_mode: RepeatMode::Off,
            audio_delay_secs: settings.audio_delay_secs as f64,
            inhibitor: Inhibitor::new(),
            media_keys: MediaKeys::new(),
            last_stats_at: std::time::Instant::now(),
            last_frames_presented: 0,
            last_frames_dropped: 0,
            pending_audio_track: None,
            pending_subtitle_track: None,
            audio_device_names: AudioOutput::list_output_devices(),
            update_checker: if settings.check_updates {
                UpdateChecker::spawn()
            } else {
                UpdateChecker::disabled()
            },
            update_url: None,
            subtitle_dl_tx,
            subtitle_dl_rx,
            settings,
        };
        if let Some(ui) = app.ui.upgrade() {
            ui.set_volume(app.settings.volume);
            ui.set_dark(app.settings.dark_theme);
            ui.set_sub_delay_text(format_delay(app.sub_delay_secs));
            ui.set_audio_delay_text(format_delay(app.audio_delay_secs));
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
            ui.set_subtitle_scale(app.settings.subtitle_scale);
            ui.set_audio_devices(string_model(app.audio_device_names.clone()));
            app.refresh_recent(&ui);
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

        // Mémoire par fichier : restaure la vitesse et prépare la restauration
        // des pistes (appliquée dès que le demuxeur les a découvertes).
        let saved = self.settings.media_state(source);
        if let Some(st) = &saved {
            self.speed_index = st.speed_index.clamp(0, SPEEDS.len() as i32 - 1);
        }
        self.pending_audio_track = saved.as_ref().map(|s| s.audio_track);
        self.pending_subtitle_track = saved.as_ref().map(|s| s.subtitle_track);

        // Applique l'état utilisateur courant à la nouvelle session.
        engine.set_volume(self.settings.volume);
        engine.set_muted(self.muted);
        engine.set_speed(SPEEDS[self.speed_index as usize]);
        engine.set_subtitle_delay(self.sub_delay_secs);
        engine.set_audio_delay(self.audio_delay_secs);
        engine.set_equalizer(self.settings.equalizer_gains);

        self.engine = Some(engine);
        self.current_source = Some(source.to_string());
        self.settings.push_history(source);
        self.audio_track_streams.clear();
        self.subtitle_track_streams.clear();
        self.chapter_count = 0;
        self.media_keys.set_metadata(title, 0);

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
            ui.set_speed_index(self.speed_index);
            self.refresh_recent(&ui);
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
                    // Mémorise pistes + vitesse pour ce fichier.
                    if let Some(ui) = self.ui.upgrade() {
                        self.settings.remember_media_state(
                            source,
                            MediaState {
                                speed_index: self.speed_index,
                                audio_track: ui.get_audio_track_index(),
                                subtitle_track: ui.get_subtitle_track_index(),
                            },
                        );
                    }
                }
            }
            if let Some(out) = &self.audio {
                out.detach();
            }
            drop(engine);
        }
        self.current_source = None;
        // Plus de lecture : on lève l'inhibition de veille.
        self.inhibitor.set(false);
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

    /// Applique une commande reçue des contrôles média du bureau (touches
    /// multimédia, MPRIS…).
    fn apply_media_event(&mut self, event: souvlaki::MediaControlEvent) {
        use souvlaki::{MediaControlEvent as E, SeekDirection};
        match event {
            E::Play => match &self.engine {
                Some(e) => e.set_paused(false),
                None => self.play_pause(),
            },
            E::Pause => {
                if let Some(e) = &self.engine {
                    e.set_paused(true);
                }
            }
            E::Toggle => self.play_pause(),
            E::Next => self.next(),
            E::Previous => self.previous(),
            E::Stop => self.stop(),
            E::Seek(SeekDirection::Forward) => self.seek_relative(10.0),
            E::Seek(SeekDirection::Backward) => self.seek_relative(-10.0),
            _ => {}
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

    /// Saute au début d'un chapitre (sélection dans la liste déroulante).
    pub fn select_chapter(&mut self, index: i32) {
        if let Some(engine) = &self.engine {
            let target = engine
                .shared
                .chapters
                .lock()
                .unwrap()
                .get(index.max(0) as usize)
                .map(|c| c.start_us);
            if let Some(t) = target {
                engine.seek(t);
            }
        }
    }

    /// Avance ou recule d'une image. Met d'abord en pause (le pas-à-pas n'a de
    /// sens qu'à l'arrêt sur image).
    pub fn step_frame(&mut self, forward: bool) {
        if let Some(engine) = &self.engine {
            if !engine.is_paused() {
                engine.set_paused(true);
            }
            engine.step_frame(forward);
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

    /// Cycle la rotation d'affichage (0 → 90 → 180 → 270°).
    pub fn cycle_rotation(&mut self) {
        let Some(engine) = &self.engine else { return };
        let next = (engine.shared.rotation.load(Ordering::Relaxed) + 1) % 4;
        engine.shared.rotation.store(next, Ordering::Relaxed);
        // Applique tout de suite, même en pause (seek sur place → nouvel aperçu).
        if engine.is_paused() {
            engine.seek(engine.position_us());
        }
        self.set_status(match next {
            1 => "Rotation : 90°",
            2 => "Rotation : 180°",
            3 => "Rotation : 270°",
            _ => "Rotation : aucune",
        });
    }

    /// Applique les réglages d'image (luminosité −1..1, contraste 0..2,
    /// saturation 0..3) via le filtre `eq`. Effet immédiat, même en pause.
    pub fn set_image_adjust(&mut self, brightness: f32, contrast: f32, saturation: f32) {
        let Some(engine) = &self.engine else { return };
        let s = &engine.shared;
        s.brightness_milli.store(
            (brightness.clamp(-1.0, 1.0) * 1000.0) as i32,
            Ordering::Relaxed,
        );
        s.contrast_milli.store(
            (contrast.clamp(0.0, 2.0) * 1000.0) as i32,
            Ordering::Relaxed,
        );
        s.saturation_milli.store(
            (saturation.clamp(0.0, 3.0) * 1000.0) as i32,
            Ordering::Relaxed,
        );
        if engine.is_paused() {
            engine.seek(engine.position_us());
        }
    }

    /// Affiche/masque le HUD de statistiques (FPS, images sautées, A/V).
    pub fn toggle_stats(&mut self) {
        if let Some(ui) = self.ui.upgrade() {
            ui.set_stats_visible(!ui.get_stats_visible());
        }
    }

    /// Ouvre la page de la mise à jour disponible dans le navigateur.
    pub fn open_update(&mut self) {
        if let Some(url) = &self.update_url {
            crate::update::open_in_browser(url);
        }
    }

    /// Bascule le mini-lecteur : fenêtre compacte sans habillage (équivalent
    /// bureau du Picture-in-Picture). Slint n'expose pas l'« always-on-top »
    /// dans son API publique ; on fournit la fenêtre réduite.
    pub fn toggle_mini(&mut self) {
        let Some(ui) = self.ui.upgrade() else { return };
        let entering = !ui.get_mini();
        if entering {
            // Mémorise la taille courante pour la restaurer ensuite.
            let size = ui.window().size();
            let scale = ui.window().scale_factor().max(0.1);
            self.pre_mini_size = Some((size.width as f32 / scale, size.height as f32 / scale));
            if ui.get_fullscreen() {
                ui.window().set_fullscreen(false);
                ui.set_fullscreen(false);
            }
            ui.window().set_size(slint::LogicalSize::new(420.0, 248.0));
        } else if let Some((w, h)) = self.pre_mini_size.take() {
            ui.window().set_size(slint::LogicalSize::new(w, h));
        }
        ui.set_mini(entering);
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

    /// Bascule la langue de l'interface entre français et anglais, en direct
    /// (les libellés `@tr` se ré-évaluent immédiatement). Le choix est persisté.
    pub fn cycle_language(&mut self) {
        let next = if self.settings.resolve_language() == "en" {
            "fr"
        } else {
            "en"
        };
        self.settings.language = next.to_string();
        self.settings.save();
        // "" = langue source (français) ; "en" = traduction bundlée.
        let code = if next == "en" { "en" } else { "" };
        if let Err(e) = slint::select_bundled_translation(code) {
            log::warn!("changement de langue impossible : {e}");
        }
        if let Some(ui) = self.ui.upgrade() {
            ui.set_ui_language(next.into());
        }
    }

    /// Cycle le mode de répétition (Off → Tous → Un) et le reflète dans l'UI.
    pub fn cycle_repeat(&mut self) {
        self.repeat_mode = self.repeat_mode.cycled();
        if let Some(ui) = self.ui.upgrade() {
            ui.set_repeat_mode(self.repeat_mode.as_index());
        }
        self.set_status(match self.repeat_mode {
            RepeatMode::Off => "Répétition : désactivée",
            RepeatMode::All => "Répétition : toute la liste",
            RepeatMode::One => "Répétition : média courant",
        });
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

    /// Ouvre un dossier ou disque Blu-ray (structure BDMV). Le chemin est
    /// transformé en source `bluray:` à l'ouverture (voir
    /// [`crate::streaming::normalize_source`]). Les disques **chiffrés**
    /// (AACS, UHD 4K) ne sont pas pris en charge (clés non fournies).
    pub fn open_bluray_dialog(&mut self) {
        if let Some(dir) = rfd::FileDialog::new()
            .set_title("Ouvrir un dossier ou disque Blu-ray (BDMV)")
            .pick_folder()
        {
            self.add_sources(vec![dir.to_string_lossy().into_owned()]);
        }
    }

    pub fn open_url(&mut self, url: &str) {
        let url = url.trim();
        if url.is_empty() {
            return;
        }
        if !crate::streaming::is_url(url) && !url.starts_with("bluray:") {
            self.set_status("URL non reconnue (http, https, rtsp, udp, bluray…)");
            return;
        }
        let index = self.playlist.add(PlaylistItem::new(url));
        self.playlist.select(index);
        self.open_current();
    }

    /// Ouvre une entrée de l'historique de lecture (fichiers récents).
    pub fn open_recent(&mut self, index: i32) {
        let Some(source) = self.settings.history.get(index.max(0) as usize).cloned() else {
            return;
        };
        let idx = self.playlist.add(PlaylistItem::new(source));
        self.playlist.select(idx);
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

    /// Met à jour la liste déroulante des fichiers récents (libellés lisibles)
    /// depuis l'historique persistant.
    fn refresh_recent(&self, ui: &MainWindow) {
        let labels = self
            .settings
            .history
            .iter()
            .map(|s| crate::utils::display_name(s))
            .collect();
        ui.set_recent_files(string_model(labels));
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

    /// Change le périphérique de sortie audio. La sortie est reconstruite et,
    /// si une lecture est en cours, le média est rouvert à la même position
    /// (le décodeur audio se reconfigure à la fréquence du nouveau matériel).
    pub fn select_audio_device(&mut self, combo_index: i32) {
        let Some(name) = self
            .audio_device_names
            .get(combo_index.max(0) as usize)
            .cloned()
        else {
            return;
        };
        let was_playing = self.engine.is_some();
        self.stop_current(true);
        match AudioOutput::new_with_device(Some(&name)) {
            Ok(out) => {
                self.audio = Some(out);
                self.set_status(&format!("Sortie audio : {name}"));
            }
            Err(e) => self.set_status(&format!("Périphérique audio indisponible : {e}")),
        }
        if was_playing {
            self.open_current();
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

    /// Recherche et télécharge des sous-titres en ligne (OpenSubtitles) pour le
    /// média courant, en arrière-plan. Le résultat est livré via un canal,
    /// drainé dans `tick`.
    pub fn search_online_subtitles(&mut self) {
        let Some(source) = self.current_source.clone() else {
            self.set_status("Ouvrez d'abord un média");
            return;
        };
        let key = self.settings.opensubtitles_api_key.clone();
        if key.trim().is_empty() {
            self.set_status("Clé d'API OpenSubtitles non configurée (paramètres)");
            return;
        }
        let lang = self.settings.subtitle_language.clone();
        let query = crate::utils::display_name(&source);
        let tx = self.subtitle_dl_tx.clone();
        self.set_status("Recherche de sous-titres en ligne…");
        std::thread::spawn(move || {
            let track = crate::opensubtitles::find(&query, &lang, &key)
                .as_deref()
                .and_then(parse_subtitle_content);
            let _ = tx.send(track);
        });
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

    /// Ajuste la taille des sous-titres (× 0.5 à × 2.5), persistée.
    pub fn adjust_subtitle_scale(&mut self, delta: f32) {
        self.settings.subtitle_scale = (self.settings.subtitle_scale + delta).clamp(0.5, 2.5);
        if let Some(ui) = self.ui.upgrade() {
            ui.set_subtitle_scale(self.settings.subtitle_scale);
        }
    }

    /// Force une couleur de sous-titres (`0xRRGGBB`), ou suit le style ASS
    /// d'origine si `code < 0`. Persisté ; appliqué au prochain rafraîchissement.
    pub fn set_subtitle_color(&mut self, code: i32) {
        self.settings.subtitle_color = if code < 0 { None } else { Some(code as u32) };
    }

    /// Ajuste le décalage de synchronisation audio/vidéo (± secondes,
    /// positif = audio retardé par rapport à la vidéo), l'applique et le
    /// persiste.
    pub fn adjust_audio_delay(&mut self, delta_secs: f32) {
        self.audio_delay_secs = (self.audio_delay_secs + delta_secs as f64).clamp(-30.0, 30.0);
        self.settings.audio_delay_secs = self.audio_delay_secs as f32;
        if let Some(engine) = &self.engine {
            engine.set_audio_delay(self.audio_delay_secs);
        }
        if let Some(ui) = self.ui.upgrade() {
            ui.set_audio_delay_text(format_delay(self.audio_delay_secs));
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

    /// Applique un préréglage d'égaliseur (voir `EQ_PRESETS`). L'index 0
    /// « Manuel » conserve les réglages courants.
    pub fn apply_eq_preset(&mut self, index: i32) {
        if index <= 0 {
            return; // « Manuel » : ne touche pas aux gains actuels.
        }
        let Some(gains) = EQ_PRESETS.get(index as usize) else {
            return;
        };
        self.settings.equalizer_gains = *gains;
        if let Some(engine) = &self.engine {
            engine.set_equalizer(*gains);
        }
        if let Some(ui) = self.ui.upgrade() {
            ui.set_eq_gains(float_model(&self.settings.equalizer_gains));
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

        // Commandes des contrôles média du bureau (touches multimédia, MPRIS).
        for event in self.media_keys.poll() {
            self.apply_media_event(event);
        }

        // Mise à jour disponible ? (résultat du thread de vérification.)
        if let Some(info) = self.update_checker.poll() {
            self.update_url = Some(info.url);
            ui.set_update_version(info.version.clone().into());
            self.set_status(&format!("Mise à jour disponible : v{}", info.version));
        }

        // Sous-titres téléchargés en ligne (résultat du thread OpenSubtitles).
        if let Ok(result) = self.subtitle_dl_rx.try_recv() {
            match result {
                Some(track) => {
                    let n = track.len();
                    if let Some(engine) = &self.engine {
                        *engine.shared.external_subtitles.lock().unwrap() = Some(track);
                    }
                    self.set_status(&format!("{n} sous-titres téléchargés"));
                }
                None => self.set_status("Aucun sous-titre trouvé en ligne"),
            }
        }

        let Some(engine) = &self.engine else {
            // Rien en lecture : statut « arrêté » pour le bureau.
            self.media_keys.set_playback(false, true);
            return;
        };
        // Copie locale pour libérer l'emprunt de `self.engine` (les méthodes
        // appelées plus bas reprennent `&mut self`).
        let shared = Arc::clone(&engine.shared);
        let duration = engine.duration_us();
        let mut position = engine.position_us();
        let paused = engine.is_paused();
        // Reflète l'état de lecture dans les contrôles du bureau.
        self.media_keys.set_playback(!paused, false);

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
            // La couleur forcée par l'utilisateur prime sur celle du style ASS.
            ui.set_subtitle_color(
                self.settings
                    .subtitle_color
                    .or(style.color)
                    .map(slint_color)
                    .unwrap_or_else(|| slint::Color::from_rgb_u8(255, 255, 255)),
            );
        }
        if ui.get_subtitle_text() != subtitle.as_str() {
            ui.set_subtitle_text(subtitle.into());
        }

        // Empêche l'écran de s'éteindre pendant la lecture d'une vidéo.
        self.inhibitor
            .set(!paused && shared.has_video.load(Ordering::Relaxed));

        // HUD de statistiques : FPS calculé sur ~0,5 s glissant.
        let stats_dt = self.last_stats_at.elapsed().as_secs_f64();
        if stats_dt >= 0.5 {
            let presented = shared.frames_presented.load(Ordering::Relaxed);
            let dropped = shared.frames_dropped.load(Ordering::Relaxed);
            let fps = presented.saturating_sub(self.last_frames_presented) as f64 / stats_dt;
            let dropped_delta = dropped.saturating_sub(self.last_frames_dropped);
            self.last_frames_presented = presented;
            self.last_frames_dropped = dropped;
            self.last_stats_at = std::time::Instant::now();
            let av_ms = shared.last_av_delta_us.load(Ordering::Relaxed) as f64 / 1000.0;
            ui.set_stats_text(
                format!("FPS {fps:.1}   sautées {dropped} (+{dropped_delta})   A/V {av_ms:+.0} ms")
                    .into(),
            );
        }

        self.refresh_track_lists(&ui);

        // Fiche d'informations média (mise à jour quand elle change).
        let info = shared.media_info.lock().unwrap().clone();
        if !info.is_empty() && ui.get_media_info_text().as_str() != info {
            ui.set_media_info_text(info.into());
        }

        // Fin du média : répétition, ou enchaînement sur la playlist.
        if shared.playback_finished() {
            if let Some(source) = &self.current_source {
                // Lecture terminée : on oublie la position de reprise.
                self.settings
                    .remember_position(source, duration, duration.max(1));
            }
            match self.repeat_mode {
                // Rejoue le média courant.
                RepeatMode::One => self.open_current(),
                // Avance, en bouclant au début après la dernière entrée.
                RepeatMode::All => {
                    if self.playlist.advance().is_none() {
                        self.playlist.select(0);
                    }
                    self.open_current();
                }
                // Comportement par défaut : suivant, sinon arrêt.
                RepeatMode::Off => {
                    if self.playlist.advance().is_some() {
                        self.open_current();
                    } else {
                        self.stop_current(false);
                        ui.set_status_text("Fin de la liste de lecture".into());
                    }
                }
            }
        }
    }

    /// Met à jour les combos de pistes quand le demuxeur les a découvertes.
    fn refresh_track_lists(&mut self, ui: &MainWindow) {
        let Some(engine) = &self.engine else { return };

        let audio_tracks = engine.shared.audio_tracks.lock().unwrap().clone();
        if audio_tracks.len() != self.audio_track_streams.len() {
            self.audio_track_streams = audio_tracks.iter().map(|t| t.stream_index).collect();
            let n = self.audio_track_streams.len();
            ui.set_audio_tracks(track_labels(&audio_tracks));
            // Restaure la piste mémorisée pour ce fichier, sinon la première.
            let idx = self
                .pending_audio_track
                .take()
                .filter(|&i| i > 0 && (i as usize) < n)
                .unwrap_or(0);
            ui.set_audio_track_index(idx);
            if idx > 0 {
                if let Some(&stream) = self.audio_track_streams.get(idx as usize) {
                    engine.select_audio_track(stream);
                }
            }
        }

        let sub_tracks = engine.shared.subtitle_tracks.lock().unwrap().clone();
        if sub_tracks.len() != self.subtitle_track_streams.len() {
            self.subtitle_track_streams = sub_tracks.iter().map(|t| t.stream_index).collect();
            let n = self.subtitle_track_streams.len();
            let mut labels = vec!["Désactivés".to_string()];
            labels.extend(sub_tracks.iter().enumerate().map(|(i, t)| t.label(i)));
            ui.set_subtitle_tracks(string_model(labels));
            // Restaure les sous-titres mémorisés (combo : 0 = désactivés).
            let idx = self
                .pending_subtitle_track
                .take()
                .filter(|&i| i > 0 && (i as usize) <= n)
                .unwrap_or(0);
            ui.set_subtitle_track_index(idx);
            if idx > 0 {
                if let Some(&stream) = self.subtitle_track_streams.get(idx as usize - 1) {
                    *engine.shared.external_subtitles.lock().unwrap() = None;
                    engine.select_subtitle_track(Some(stream));
                }
            }
        }

        let chapters = engine.shared.chapters.lock().unwrap().clone();
        if chapters.len() != self.chapter_count {
            self.chapter_count = chapters.len();
            let labels = chapters
                .iter()
                .enumerate()
                .map(|(i, c)| {
                    format!(
                        "{}. {} ({})",
                        i + 1,
                        c.title,
                        crate::utils::format_time(c.start_us)
                    )
                })
                .collect();
            ui.set_chapters(string_model(labels));
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

/// Signature d'un parseur de sous-titres (SRT/ASS/VTT).
type SubParser = fn(&str) -> anyhow::Result<Vec<crate::subtitles::SubtitleCue>>;

/// Parse un contenu de sous-titres (essaie SRT, ASS puis VTT) en piste.
fn parse_subtitle_content(content: &str) -> Option<SubtitleTrack> {
    use crate::subtitles::{parse_ass, parse_srt, parse_vtt};
    let parsers: [SubParser; 3] = [parse_srt, parse_ass, parse_vtt];
    parsers.into_iter().find_map(|parse| {
        parse(content)
            .ok()
            .filter(|cues| !cues.is_empty())
            .map(SubtitleTrack::new)
    })
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
