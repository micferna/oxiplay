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
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Vitesses proposées — doit rester aligné avec le ComboBox de `main.slint`.
pub const SPEEDS: [f64; 9] = [0.25, 0.5, 0.75, 1.0, 1.25, 1.5, 2.0, 3.0, 4.0];
/// Index de la vitesse 1.00× dans [`SPEEDS`].
pub const SPEED_NORMAL_INDEX: i32 = 3;

/// Filtres de fichiers des boîtes de dialogue.
pub const MEDIA_EXTENSIONS: &[&str] = &[
    "mp4", "mkv", "avi", "mov", "webm", "mpg", "mpeg", "flv", "ts", "m2ts", "wmv", "ogv", "mp3",
    "flac", "wav", "ogg", "oga", "aac", "m4a", "opus", "wma", "iso",
];
pub const SUBTITLE_EXTENSIONS: &[&str] = &["srt", "ass", "ssa", "vtt"];

/// État applicatif principal (vivant sur le thread d'interface).
pub struct App {
    ui: Weak<MainWindow>,
    audio: Option<AudioOutput>,
    engine: Option<PlayerEngine>,
    playlist: Playlist,
    /// Filtre de recherche de la playlist (sous-chaîne du titre, insensible à
    /// la casse). Vide = pas de filtre.
    playlist_search: String,
    /// Catégorie/pays sélectionnée pour le filtre (vide = toutes).
    playlist_group: String,
    /// Filtre « favoris uniquement » actif.
    playlist_favorites_only: bool,
    /// Catégories distinctes présentes (pour mapper l'index du ComboBox).
    playlist_groups: Vec<String>,
    /// Indices d'items affichés (ligne visible → index réel), pour mapper les
    /// clics quand un filtre masque des entrées.
    playlist_visible: Vec<usize>,
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
    /// Index de playlist de la dernière chaîne/média quitté, pour le « zap
    /// retour » (bascule façon télécommande TV). `None` au démarrage.
    last_channel: Option<usize>,
    /// Positions (µs) des marque-pages du média courant, dans l'ordre du modèle
    /// affiché (pour relier l'index de l'UI à une position de seek).
    bookmark_positions: Vec<i64>,
    /// Décalage de synchronisation audio/vidéo (secondes).
    audio_delay_secs: f64,
    /// Minuteur de veille : échéance après laquelle la lecture est mise en
    /// pause (`None` = désactivé), et durée courante en minutes (pour le cycle).
    sleep_deadline: Option<std::time::Instant>,
    sleep_minutes: u32,
    /// Boucle A-B : bornes (µs) d'un segment à répéter. Les deux posées → la
    /// lecture reboucle de B vers A.
    loop_a: Option<i64>,
    loop_b: Option<i64>,
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
    /// Canal de livraison des playlists M3U distantes (thread réseau → UI).
    m3u_tx: Sender<M3uFetch>,
    m3u_rx: Receiver<M3uFetch>,
    /// Logos de chaînes chargés (URL → image), cache mémoire.
    logo_cache: HashMap<String, slint::Image>,
    /// Logos en cours de téléchargement (évite les doublons).
    logo_pending: HashSet<String>,
    /// Demandes de téléchargement de logo (UI → worker réseau).
    logo_req_tx: Sender<String>,
    /// Logos téléchargés (worker → UI) : (URL, chemin du cache disque).
    logo_res_rx: Receiver<(String, PathBuf)>,
}

/// Résultat de la récupération d'une URL `.m3u`/`.m3u8` en arrière-plan.
enum M3uFetch {
    /// Vrai flux HLS : à lire directement (l'URL d'origine).
    Stream(String),
    /// Annuaire de chaînes : à charger comme entrées de playlist.
    Channels(Vec<PlaylistItem>),
    /// Échec réseau ou contenu illisible.
    Failed(String),
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
        let (m3u_tx, m3u_rx) = unbounded();
        let (logo_req_tx, logo_req_rx) = unbounded::<String>();
        let (logo_res_tx, logo_res_rx) = unbounded::<(String, PathBuf)>();
        spawn_logo_worker(logo_req_rx, logo_res_tx);
        let app = Self {
            ui,
            audio,
            engine: None,
            playlist: Playlist::default(),
            playlist_search: String::new(),
            playlist_group: String::new(),
            playlist_favorites_only: false,
            playlist_groups: Vec::new(),
            playlist_visible: Vec::new(),
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
            last_channel: None,
            bookmark_positions: Vec::new(),
            audio_delay_secs: settings.audio_delay_secs as f64,
            sleep_deadline: None,
            sleep_minutes: 0,
            loop_a: None,
            loop_b: None,
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
            m3u_tx,
            m3u_rx,
            logo_cache: HashMap::new(),
            logo_pending: HashSet::new(),
            logo_req_tx,
            logo_res_rx,
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
            ui.set_normalize(app.settings.normalize_audio);
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
        // La boucle A-B ne vaut que pour le média courant.
        self.loop_a = None;
        self.loop_b = None;
        self.refresh_ab_loop_indicator();
        // Le zoom/déplacement et le mode d'image (transformations d'affichage)
        // repartent à neuf.
        if let Some(ui) = self.ui.upgrade() {
            ui.set_video_zoom(1.0);
            ui.set_video_pan_x(0.0);
            ui.set_video_pan_y(0.0);
            ui.set_aspect_mode(0);
        }

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
        engine.shared.set_normalize(self.settings.normalize_audio);

        // Sous-titres « sidecar » : un .srt/.ass portant le même nom que le
        // média (éventuellement suffixé par la langue préférée) est chargé
        // automatiquement comme piste externe.
        if let Some(path) = find_sidecar_subtitle(source, &self.settings.subtitle_language) {
            match crate::subtitles::load_file(&path) {
                Ok(track) => {
                    let n = track.len();
                    *engine.shared.external_subtitles.lock().unwrap() = Some(track);
                    log::info!("sous-titres sidecar : {} ({n} répliques)", path.display());
                }
                Err(e) => log::debug!("sidecar illisible ({}) : {e}", path.display()),
            }
        }

        self.engine = Some(engine);
        self.current_source = Some(source.to_string());
        self.settings.push_history(source);
        self.refresh_bookmarks_ui();
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
    /// Pose ou retire un marque-page à la position courante (bascule). Persisté
    /// par média et reflété dans le menu déroulant des marque-pages.
    pub fn toggle_bookmark(&mut self) {
        let Some(engine) = &self.engine else {
            self.set_status("Aucun média ouvert");
            return;
        };
        let pos = engine.position_us();
        let Some(source) = self.current_source.clone() else {
            return;
        };
        let added = self.settings.toggle_bookmark(&source, pos);
        self.refresh_bookmarks_ui();
        self.set_status(if added {
            "Marque-page posé"
        } else {
            "Marque-page retiré"
        });
    }

    /// Saute au marque-page d'index `index` du média courant.
    pub fn jump_bookmark(&mut self, index: i32) {
        let Some(&pos) = self.bookmark_positions.get(index.max(0) as usize) else {
            return;
        };
        if let Some(engine) = &self.engine {
            engine.seek(pos);
        }
    }

    /// Recharge la liste des marque-pages du média courant dans l'UI.
    fn refresh_bookmarks_ui(&mut self) {
        let positions = self
            .current_source
            .as_deref()
            .map(|s| self.settings.bookmarks_for(s))
            .unwrap_or_default();
        let labels = positions
            .iter()
            .enumerate()
            .map(|(i, &p)| format!("🔖 {} · {}", i + 1, crate::utils::format_time(p)))
            .collect();
        self.bookmark_positions = positions;
        if let Some(ui) = self.ui.upgrade() {
            ui.set_bookmarks(string_model(labels));
        }
    }

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
        let leaving = self.playlist.current_index();
        if self.playlist.advance().is_some() {
            self.last_channel = leaving;
            self.open_current();
        }
    }

    pub fn previous(&mut self) {
        let leaving = self.playlist.current_index();
        if self.playlist.previous().is_some() {
            self.last_channel = leaving;
            self.open_current();
        }
    }

    /// « Zap retour » : rebascule instantanément sur la dernière chaîne/média
    /// quitté (façon touche « précédent » d'une télécommande). Échange les deux
    /// pour pouvoir faire des allers-retours.
    pub fn zap_back(&mut self) {
        let Some(target) = self.last_channel else {
            self.set_status("Aucune chaîne précédente");
            return;
        };
        if target >= self.playlist.len() {
            self.last_channel = None;
            return;
        }
        let leaving = self.playlist.current_index();
        if self.playlist.select(target).is_some() {
            self.last_channel = leaving;
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

    /// (Dés)active la normalisation du volume (loudness). Persistée et
    /// appliquée à la session courante comme aux médias suivants.
    pub fn toggle_normalize(&mut self) {
        let on = !self.settings.normalize_audio;
        self.settings.normalize_audio = on;
        if let Some(engine) = &self.engine {
            engine.shared.set_normalize(on);
        }
        if let Some(ui) = self.ui.upgrade() {
            ui.set_normalize(on);
        }
        self.set_status(if on {
            "Normalisation du volume : activée"
        } else {
            "Normalisation du volume : désactivée"
        });
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

    /// Applique les améliorations d'image : correction gamma (0.1..4, 1 = neutre)
    /// via `eq`, netteté (`unsharp`, 0..3) et débruitage (`hqdn3d`, 0..2). Effet
    /// immédiat, même en pause.
    pub fn set_video_enhance(&mut self, gamma: f32, sharpen: f32, denoise: f32) {
        let Some(engine) = &self.engine else { return };
        let s = &engine.shared;
        s.gamma_milli
            .store((gamma.clamp(0.1, 4.0) * 1000.0) as i32, Ordering::Relaxed);
        s.sharpen_milli
            .store((sharpen.clamp(0.0, 3.0) * 1000.0) as i32, Ordering::Relaxed);
        s.denoise_milli
            .store((denoise.clamp(0.0, 2.0) * 1000.0) as i32, Ordering::Relaxed);
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

    /// (Dés)active la lecture aléatoire et le reflète dans l'UI. L'auto-avance
    /// de fin de média (`tick`) et les boutons précédent/suivant passent par
    /// `Playlist::advance`/`previous`, qui respectent déjà ce mode.
    pub fn toggle_shuffle(&mut self) {
        let on = !self.playlist.shuffle();
        self.playlist.set_shuffle(on);
        if let Some(ui) = self.ui.upgrade() {
            ui.set_shuffle(on);
        }
        self.set_status(if on {
            "Lecture aléatoire : activée"
        } else {
            "Lecture aléatoire : désactivée"
        });
    }

    /// Cycle le minuteur de veille : off → 15 → 30 → 60 → 90 min → off. À
    /// expiration (vérifiée dans `tick`), la lecture est mise en pause.
    pub fn cycle_sleep_timer(&mut self) {
        self.sleep_minutes = match self.sleep_minutes {
            0 => 15,
            15 => 30,
            30 => 60,
            60 => 90,
            _ => 0,
        };
        self.sleep_deadline = (self.sleep_minutes > 0).then(|| {
            std::time::Instant::now()
                + std::time::Duration::from_secs(self.sleep_minutes as u64 * 60)
        });
        if let Some(ui) = self.ui.upgrade() {
            ui.set_sleep_label(self.sleep_label().into());
        }
        if self.sleep_minutes == 0 {
            self.set_status("Minuteur de veille désactivé");
        } else {
            self.set_status(&format!("Minuteur de veille : {} min", self.sleep_minutes));
        }
    }

    /// Libellé court du minuteur (« Off » ou « 30 min »).
    fn sleep_label(&self) -> String {
        if self.sleep_minutes == 0 {
            "Off".to_string()
        } else {
            format!("{} min", self.sleep_minutes)
        }
    }

    /// Cycle la boucle A-B : pose A → pose B → efface. Quand A et B sont posés,
    /// `tick` reboucle de B vers A.
    pub fn toggle_ab_loop(&mut self) {
        let Some(engine) = &self.engine else {
            self.set_status("Ouvrez d'abord un média");
            return;
        };
        let pos = engine.position_us();
        match (self.loop_a, self.loop_b) {
            (None, _) => {
                self.loop_a = Some(pos);
                self.loop_b = None;
                self.set_status("Boucle A-B : point A posé (rappuyez pour B)");
            }
            (Some(a), None) if pos > a + 500_000 => {
                self.loop_b = Some(pos);
                self.set_status("Boucle A-B activée");
            }
            (Some(_), None) => {
                self.set_status("Boucle A-B : B doit être après A");
            }
            _ => {
                self.loop_a = None;
                self.loop_b = None;
                self.set_status("Boucle A-B désactivée");
            }
        }
        self.refresh_ab_loop_indicator();
    }

    /// Reflète l'état de la boucle A-B dans l'UI (0 = off, 1 = A posé, 2 = active).
    fn refresh_ab_loop_indicator(&self) {
        if let Some(ui) = self.ui.upgrade() {
            let state = match (self.loop_a, self.loop_b) {
                (Some(_), Some(_)) => 2,
                (Some(_), None) => 1,
                _ => 0,
            };
            ui.set_ab_loop(state);
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
        self.refresh_playlist_groups();
        self.refresh_playlist_model();
        if self.engine.is_none() {
            self.playlist.select(first_added);
            self.open_current();
        }
    }

    // Les sélecteurs de fichiers sont ouverts en mode **asynchrone** depuis le
    // câblage (`main.rs`, via `slint::spawn_local`) pour ne pas bloquer
    // l'événementiel — un dialogue synchrone gèlerait la fenêtre (« ne répond
    // pas »). Files & Blu-ray retombent sur [`Self::add_sources`] ; les autres
    // sélections sont livrées aux handlers `*_path` / `*_to` ci-dessous.

    pub fn open_url(&mut self, url: &str) {
        let url = url.trim();
        if url.is_empty() {
            return;
        }
        if !crate::streaming::is_url(url) && !url.starts_with("bluray:") {
            self.set_status("URL non reconnue (http, https, rtsp, udp, bluray…)");
            return;
        }
        // Un `.m3u`/`.m3u8` peut être soit un flux HLS unique, soit un annuaire
        // de chaînes IPTV (des centaines/milliers d'entrées) — l'extension ne
        // tranche pas. On récupère le contenu en arrière-plan pour décider.
        if url.to_ascii_lowercase().contains(".m3u") {
            self.fetch_m3u(url.to_string());
            return;
        }
        let index = self.playlist.add(PlaylistItem::new(url));
        self.playlist.select(index);
        self.open_current();
    }

    /// Récupère une playlist `.m3u`/`.m3u8` distante sur un thread réseau, puis
    /// décide (dans `tick`) s'il s'agit d'un flux à lire ou d'un annuaire de
    /// chaînes à charger. Ne bloque jamais l'interface.
    fn fetch_m3u(&mut self, url: String) {
        self.set_status("Récupération de la playlist…");
        let tx = self.m3u_tx.clone();
        std::thread::spawn(move || {
            let result = match crate::streaming::fetch_text(&url) {
                Ok(content) if crate::streaming::looks_like_hls(&content) => M3uFetch::Stream(url),
                Ok(content) => {
                    let items = crate::playlist::parse_m3u_content(&content, None);
                    if items.is_empty() {
                        // Aucune entrée reconnue : on laisse FFmpeg tenter l'URL.
                        M3uFetch::Stream(url)
                    } else {
                        M3uFetch::Channels(items)
                    }
                }
                Err(e) => M3uFetch::Failed(e.to_string()),
            };
            let _ = tx.send(result);
        });
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
        let Some(&real) = self.playlist_visible.get(index) else {
            return;
        };
        let leaving = self.playlist.current_index();
        if real != leaving.unwrap_or(usize::MAX) && self.playlist.select(real).is_some() {
            self.last_channel = leaving;
            self.open_current();
        }
    }

    pub fn playlist_remove(&mut self, index: usize) {
        let Some(&real) = self.playlist_visible.get(index) else {
            return;
        };
        let was_current = self.playlist.current_index() == Some(real);
        self.playlist.remove(real);
        if was_current {
            self.stop_current(true);
        }
        self.refresh_playlist_groups();
        self.refresh_playlist_model();
    }

    pub fn playlist_shift(&mut self, index: usize, delta: i32) {
        let Some(&real) = self.playlist_visible.get(index) else {
            return;
        };
        self.playlist.shift(real, delta);
        self.refresh_playlist_model();
    }

    /// Enregistre la playlist au chemin choisi (sélection faite en amont).
    pub fn save_playlist_to(&mut self, path: std::path::PathBuf) {
        match self.playlist.save_m3u(&path) {
            Ok(()) => self.set_status(&format!("Playlist enregistrée : {}", path.display())),
            Err(e) => self.set_status(&format!("Échec d'enregistrement : {e}")),
        }
    }

    /// Charge une playlist M3U depuis le chemin choisi (sélection faite en amont).
    pub fn load_playlist_from(&mut self, path: std::path::PathBuf) {
        match self.playlist.load_m3u(&path) {
            Ok(n) => {
                self.set_status(&format!("{n} entrées chargées"));
                self.stop_current(true);
                self.refresh_playlist_groups();
                self.refresh_playlist_model();
            }
            Err(e) => self.set_status(&format!("Échec de chargement : {e}")),
        }
    }

    /// Reconstruit la liste affichée en appliquant le filtre courant (recherche
    /// + catégorie) et mémorise la correspondance ligne visible → index réel.
    fn refresh_playlist_model(&mut self) {
        let Some(ui) = self.ui.upgrade() else { return };
        let current = self.playlist.current_index();
        let search = self.playlist_search.to_lowercase();
        let mut entries = Vec::new();
        let mut visible = Vec::new();
        // Logos à télécharger (URLs des entrées visibles non encore en cache),
        // plafonnés pour ne pas marteler le réseau sur un gros annuaire.
        let mut to_fetch: Vec<String> = Vec::new();
        for (i, item) in self.playlist.items().iter().enumerate() {
            let favorite = self.settings.is_favorite(&item.source);
            let by_group = self.playlist_group.is_empty() || item.group == self.playlist_group;
            let by_search = search.is_empty() || item.title.to_lowercase().contains(&search);
            let by_fav = !self.playlist_favorites_only || favorite;
            if by_group && by_search && by_fav {
                let logo = if item.logo.is_empty() {
                    slint::Image::default()
                } else if let Some(img) = self.logo_cache.get(&item.logo) {
                    img.clone()
                } else {
                    if to_fetch.len() < 200 && !self.logo_pending.contains(&item.logo) {
                        to_fetch.push(item.logo.clone());
                    }
                    slint::Image::default()
                };
                entries.push(PlaylistEntry {
                    title: item.title.clone().into(),
                    is_current: Some(i) == current,
                    is_favorite: favorite,
                    logo,
                });
                visible.push(i);
            }
        }
        self.playlist_visible = visible;
        ui.set_playlist_entries(ModelRc::from(Rc::new(VecModel::from(entries))));
        // Lance les téléchargements de logos manquants (le worker écrit dans le
        // cache disque, `tick` charge l'image et rafraîchit la liste).
        for url in to_fetch {
            if self.logo_pending.insert(url.clone()) {
                let _ = self.logo_req_tx.send(url);
            }
        }
    }

    /// (Dé)marque l'entrée affichée en favori et persiste le choix.
    pub fn toggle_favorite(&mut self, index: usize) {
        let Some(&real) = self.playlist_visible.get(index) else {
            return;
        };
        let Some(item) = self.playlist.items().get(real) else {
            return;
        };
        let source = item.source.clone();
        self.settings.toggle_favorite(&source);
        self.settings.save();
        self.refresh_playlist_model();
    }

    /// Bascule le filtre « favoris uniquement ».
    pub fn toggle_favorites_filter(&mut self) {
        self.playlist_favorites_only = !self.playlist_favorites_only;
        if let Some(ui) = self.ui.upgrade() {
            ui.set_favorites_only(self.playlist_favorites_only);
        }
        self.refresh_playlist_model();
    }

    /// Recalcule les catégories distinctes (pour le ComboBox de filtre). À
    /// appeler quand le contenu de la playlist change, pas à chaque frappe (la
    /// reconstruction du modèle réinitialiserait la sélection du ComboBox).
    fn refresh_playlist_groups(&mut self) {
        let mut groups: Vec<String> = self
            .playlist
            .items()
            .iter()
            .map(|it| it.group.clone())
            .filter(|g| !g.is_empty())
            .collect();
        groups.sort();
        groups.dedup();
        self.playlist_groups = groups.clone();
        if let Some(ui) = self.ui.upgrade() {
            let mut model = vec!["Toutes les catégories".to_string()];
            model.extend(groups);
            ui.set_playlist_groups(string_model(model));
        }
    }

    /// Met à jour le filtre de recherche (depuis le champ texte).
    pub fn set_playlist_search(&mut self, text: &str) {
        self.playlist_search = text.to_string();
        self.refresh_playlist_model();
    }

    /// Sélectionne la catégorie de filtre (index 0 = toutes).
    pub fn set_playlist_group(&mut self, index: i32) {
        self.playlist_group = if index <= 0 {
            String::new()
        } else {
            self.playlist_groups
                .get(index as usize - 1)
                .cloned()
                .unwrap_or_default()
        };
        self.refresh_playlist_model();
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

    /// Passe à la piste audio suivante (cyclique). Sans effet s'il y en a ≤ 1.
    pub fn cycle_audio_track(&mut self) {
        let count = self.audio_track_streams.len() as i32;
        if count <= 1 {
            return;
        }
        let Some(ui) = self.ui.upgrade() else { return };
        let next = (ui.get_audio_track_index() + 1).rem_euclid(count);
        ui.set_audio_track_index(next);
        self.select_audio_track(next);
        self.set_status(&format!("Piste audio {}/{}", next + 1, count));
    }

    /// Passe au sous-titre suivant (cyclique, « désactivés » inclus).
    pub fn cycle_subtitle_track(&mut self) {
        let count = self.subtitle_track_streams.len() as i32 + 1; // +1 = désactivés
        if count <= 1 {
            return; // aucune piste de sous-titres
        }
        let Some(ui) = self.ui.upgrade() else { return };
        let next = (ui.get_subtitle_track_index() + 1).rem_euclid(count);
        ui.set_subtitle_track_index(next);
        self.select_subtitle_track(next);
        if next == 0 {
            self.set_status("Sous-titres désactivés");
        } else {
            self.set_status(&format!("Sous-titres : piste {}/{}", next, count - 1));
        }
    }

    /// Charge des sous-titres externes depuis le chemin choisi.
    pub fn load_subtitle_path(&mut self, path: std::path::PathBuf) {
        let Some(engine) = &self.engine else {
            self.set_status("Ouvrez d'abord un média");
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

        // Mémorise la géométrie courante (uniquement en mode fenêtré normal :
        // ni mini-lecteur, ni plein écran, dont la taille n'est pas à retenir).
        // Capturée ici plutôt qu'après `run()`, où la taille redevient périmée.
        if !ui.get_mini() && !ui.get_fullscreen() {
            // `reported-width/height` = taille logique réelle (cf. main.slint).
            let w = ui.get_reported_width().round() as u32;
            let h = ui.get_reported_height().round() as u32;
            self.remember_window_geometry(w, h);
        }

        // Minuteur de veille : met la lecture en pause à expiration.
        if self
            .sleep_deadline
            .is_some_and(|d| std::time::Instant::now() >= d)
        {
            self.sleep_deadline = None;
            self.sleep_minutes = 0;
            if let Some(engine) = &self.engine {
                engine.set_paused(true);
            }
            ui.set_sleep_label("Off".into());
            self.set_status("Minuteur de veille écoulé — lecture en pause");
        }

        // Boucle A-B : reboucle de B vers A.
        if let (Some(a), Some(b)) = (self.loop_a, self.loop_b) {
            if let Some(engine) = &self.engine {
                if engine.position_us() >= b {
                    engine.seek(a);
                }
            }
        }

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

        // Playlist M3U distante récupérée : flux unique → on lit ; annuaire de
        // chaînes → on charge tout et on lance la première.
        if let Ok(result) = self.m3u_rx.try_recv() {
            match result {
                M3uFetch::Stream(url) => {
                    let index = self.playlist.add(PlaylistItem::new(url));
                    self.playlist.select(index);
                    self.open_current();
                }
                M3uFetch::Channels(items) => {
                    let count = items.len();
                    let first = self.playlist.len();
                    for item in items {
                        self.playlist.add(item);
                    }
                    self.refresh_playlist_groups();
                    self.refresh_playlist_model();
                    // Montre la liste des chaînes pour que l'utilisateur navigue.
                    if let Some(ui) = self.ui.upgrade() {
                        ui.set_playlist_visible(true);
                    }
                    self.set_status(&format!("{count} chaînes chargées"));
                    if self.playlist.select(first).is_some() {
                        self.open_current();
                    }
                }
                M3uFetch::Failed(e) => self.set_status(&format!("Playlist illisible : {e}")),
            }
        }

        // Logos de chaînes téléchargés : on charge l'image (décodage côté UI) et
        // on rafraîchit la liste une fois si au moins un est arrivé.
        let mut got_logo = false;
        while let Ok((url, path)) = self.logo_res_rx.try_recv() {
            self.logo_pending.remove(&url);
            if let Ok(img) = slint::Image::load_from_path(&path) {
                self.logo_cache.insert(url, img);
                got_logo = true;
            }
        }
        if got_logo {
            self.refresh_playlist_model();
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
    /// Mémorise la géométrie de la fenêtre (sauvegardée par `shutdown`).
    pub fn remember_window_geometry(&mut self, width: u32, height: u32) {
        // Ignore les tailles aberrantes (fenêtre minimisée → 0, ou démesurée).
        if (200..=16_384).contains(&width) && (150..=16_384).contains(&height) {
            self.settings.window = Some(crate::settings::WindowGeometry { width, height });
        }
    }

    pub fn shutdown(&mut self) {
        self.stop_current(true);
        self.settings.save();
    }
}

/// Cherche un fichier de sous-titres « sidecar » à côté d'un média **local** :
/// `<nom>.<ext>` ou `<nom>.<langue>.<ext>` (ext dans [`SUBTITLE_EXTENSIONS`]).
/// Priorité : langue préférée > nom exact > toute autre langue. Renvoie `None`
/// pour les URL/flux ou si rien n'est trouvé.
fn find_sidecar_subtitle(source: &str, prefer_lang: &str) -> Option<std::path::PathBuf> {
    let path = std::path::Path::new(source);
    if !path.is_file() {
        return None; // fichiers locaux uniquement (pas d'URL, flux, bluray:)
    }
    let dir = path.parent()?;
    let stem = path.file_stem()?.to_str()?;
    let lang_prefix = format!("{stem}.");

    let mut exact = None; // <nom>.<ext>
    let mut lang_match = None; // <nom>.<langue préférée>.<ext>
    let mut any_lang = None; // <nom>.<autre langue>.<ext>
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let p = entry.path();
        let is_sub = p
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .is_some_and(|e| SUBTITLE_EXTENSIONS.contains(&e.as_str()));
        if !is_sub {
            continue;
        }
        let Some(name_stem) = p.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if name_stem == stem {
            exact = Some(p);
        } else if let Some(lang) = name_stem.strip_prefix(&lang_prefix) {
            if lang.eq_ignore_ascii_case(prefer_lang) {
                lang_match = Some(p);
            } else if any_lang.is_none() {
                any_lang = Some(p);
            }
        }
    }
    lang_match.or(exact).or(any_lang)
}

/// Dossier de cache disque des logos de chaînes.
fn logo_cache_dir() -> Option<PathBuf> {
    Some(dirs::cache_dir()?.join("oxiplay").join("logos"))
}

/// Nom de fichier de cache déterministe pour une URL de logo.
fn logo_filename(url: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    url.hash(&mut h);
    format!("{:016x}", h.finish())
}

/// Télécharge un logo (plafonné à 4 Mio) dans le fichier de cache `path`.
fn download_logo(url: &str, path: &std::path::Path) -> anyhow::Result<()> {
    use std::io::Read;
    let mut buf = Vec::new();
    ureq::get(url)
        .timeout(std::time::Duration::from_secs(10))
        .call()?
        .into_reader()
        .take(4 * 1024 * 1024)
        .read_to_end(&mut buf)?;
    anyhow::ensure!(!buf.is_empty(), "logo vide");
    std::fs::write(path, &buf)?;
    Ok(())
}

/// Thread de fond : télécharge les logos demandés vers le cache disque (en les
/// sautant s'ils y sont déjà) et renvoie le chemin au thread d'interface, qui
/// se charge du décodage (`slint::Image` n'est pas `Send`).
fn spawn_logo_worker(req: Receiver<String>, res: Sender<(String, PathBuf)>) {
    let dir = logo_cache_dir();
    if let Some(dir) = &dir {
        let _ = std::fs::create_dir_all(dir);
    }
    std::thread::Builder::new()
        .name("oxiplay-logos".into())
        .spawn(move || {
            let Some(dir) = dir else { return };
            while let Ok(url) = req.recv() {
                let path = dir.join(logo_filename(&url));
                if path.exists() || download_logo(&url, &path).is_ok() {
                    let _ = res.send((url, path));
                }
            }
        })
        .ok();
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

#[cfg(test)]
mod tests {
    use super::find_sidecar_subtitle;

    #[test]
    fn sidecar_priority_and_matching() {
        let dir = std::env::temp_dir().join("oxiplay-sidecar-test");
        std::fs::create_dir_all(&dir).unwrap();
        let media = dir.join("film.mkv");
        std::fs::write(&media, b"x").unwrap();
        std::fs::write(dir.join("film.srt"), b"").unwrap();
        std::fs::write(dir.join("film.en.srt"), b"").unwrap();
        std::fs::write(dir.join("film.fr.ass"), b"").unwrap();

        // Langue préférée « fr » → film.fr.ass l'emporte sur l'exact.
        let got = find_sidecar_subtitle(media.to_str().unwrap(), "fr").unwrap();
        assert_eq!(got.file_name().unwrap(), "film.fr.ass");
        // Langue absente (« de ») → repli sur le nom exact film.srt.
        let got = find_sidecar_subtitle(media.to_str().unwrap(), "de").unwrap();
        assert_eq!(got.file_name().unwrap(), "film.srt");
        // Une URL n'a pas de sidecar.
        assert!(find_sidecar_subtitle("https://ex.com/live.m3u8", "fr").is_none());

        std::fs::remove_dir_all(&dir).ok();
    }
}
