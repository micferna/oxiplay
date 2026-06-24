//! OxiPlay — lecteur multimédia multiplateforme en Rust.
//!
//! Point d'entrée : initialise FFmpeg et la journalisation, construit la
//! fenêtre Slint, relie les callbacks de l'interface à la couche
//! application, puis lance la boucle d'événements.
//!
//! Usage : `oxiplay [fichiers ou URL…]`

// Pas de console parasite sous Windows en build release.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use oxiplay::app::App;
use oxiplay::ui::MainWindow;
use slint::ComponentHandle;
use std::cell::RefCell;
use std::rc::Rc;

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    ffmpeg_the_third::init()?;
    // Réduit la verbosité des bibliothèques FFmpeg elles-mêmes.
    ffmpeg_the_third::util::log::set_level(ffmpeg_the_third::util::log::Level::Error);

    // Rendu vidéo GPU (feature `gpu`) : on force le backend wgpu pour pouvoir
    // partager son device. En cas d'échec (drivers, environnement sans GPU),
    // repli silencieux sur le backend par défaut — l'app démarre quand même.
    #[cfg(feature = "gpu")]
    {
        // R16Unorm (textures des plans HDR 10 bits / P010) requiert cette
        // feature de device — universelle sur GPU de bureau. Si l'adaptateur ne
        // la propose pas, la sélection wgpu échoue et on retombe en logiciel.
        let mut settings = slint::wgpu_28::WGPUSettings::default();
        settings.device_required_features |=
            slint::wgpu_28::wgpu::Features::TEXTURE_FORMAT_16BIT_NORM;
        match slint::BackendSelector::new()
            .require_wgpu_28(slint::wgpu_28::WGPUConfiguration::Automatic(settings))
            .select()
        {
            Ok(()) => log::info!("backend wgpu sélectionné"),
            Err(e) => log::warn!("backend wgpu indisponible, repli logiciel : {e}"),
        }
    }

    let main_window = MainWindow::new()?;

    // Capture le device/queue wgpu fournis par Slint pour brancher le pipeline
    // de rendu vidéo GPU. Si le backend retenu n'est pas wgpu (repli ci-dessus),
    // le notifier ne reçoit jamais `WGPU28` et le rendu reste logiciel.
    #[cfg(feature = "gpu")]
    if let Err(e) = main_window.window().set_rendering_notifier(|state, api| {
        if let (
            slint::RenderingState::RenderingSetup,
            slint::GraphicsAPI::WGPU28 { device, queue, .. },
        ) = (state, api)
        {
            oxiplay::render::init_renderer(device.clone(), queue.clone());
            log::info!("rendu vidéo GPU wgpu actif");
        }
    }) {
        log::warn!("notifier de rendu indisponible : {e}");
    }

    let init_settings = oxiplay::settings::Settings::load();

    // Restaure la taille de la fenêtre de la session précédente (taille logique ;
    // avant l'affichage, donc sans saut visible). La position n'est pas
    // restaurée (gérée par le compositeur, no-op sous Wayland).
    if let Some(g) = init_settings.window {
        main_window
            .window()
            .set_size(slint::LogicalSize::new(g.width as f32, g.height as f32));
    }

    // Langue de l'interface (traductions bundlées, voir build.rs). Le français
    // est la langue source : on ne sélectionne une traduction que pour les
    // autres langues. Appelé après la création de la fenêtre (contexte Slint
    // initialisé) mais avant `run()`, donc sans clignotement visible.
    let ui_language = init_settings.resolve_language();
    if ui_language == "en" {
        // "" = langue source (français) ; "en" = traduction bundlée.
        if let Err(e) = slint::select_bundled_translation("en") {
            log::debug!("traduction anglaise indisponible : {e}");
        }
    }
    main_window.set_ui_language(ui_language.into());

    let app = Rc::new(RefCell::new(App::new(main_window.as_weak())));

    wire_callbacks(&main_window, &app);

    // Minuteur de synchronisation interface ↔ moteur (10 Hz).
    let timer = slint::Timer::default();
    {
        let app = Rc::clone(&app);
        timer.start(
            slint::TimerMode::Repeated,
            std::time::Duration::from_millis(100),
            move || app.borrow_mut().tick(),
        );
    }

    // Fichiers passés en ligne de commande.
    let cli_sources: Vec<String> = std::env::args().skip(1).collect();
    if !cli_sources.is_empty() {
        app.borrow_mut().add_sources(cli_sources);
    }

    main_window.run()?;

    // La géométrie de la fenêtre est capturée en continu dans `tick` ; il ne
    // reste qu'à persister les réglages.
    app.borrow_mut().shutdown();

    // Avec le backend wgpu, la destruction du device Vulkan plante au teardown
    // sur certains pilotes (dialogue « application forcée à quitter » alors que
    // la lecture s'est déroulée normalement). L'état utilisateur étant déjà
    // persisté par `shutdown()` ci-dessus, on termine immédiatement le
    // processus sans dérouler les destructeurs graphiques problématiques.
    #[cfg(feature = "gpu")]
    std::process::exit(0);

    #[cfg(not(feature = "gpu"))]
    Ok(())
}

/// Relie chaque callback déclaré dans `main.slint` à la couche application.
fn wire_callbacks(ui: &MainWindow, app: &Rc<RefCell<App>>) {
    macro_rules! on {
        ($setter:ident, |$app:ident $(, $arg:ident : $ty:ty)*| $body:expr) => {{
            let app = Rc::clone(app);
            ui.$setter(move |$($arg : $ty),*| {
                let mut $app = app.borrow_mut();
                $body
            });
        }};
    }

    // Variante pour les sélecteurs de fichiers : on lance un dialogue
    // **asynchrone** sur l'événementiel (`slint::spawn_local`) au lieu d'un
    // dialogue bloquant qui gèlerait la fenêtre (« ne répond pas »). Le futur
    // capture l'`App` et applique le résultat une fois la sélection faite.
    macro_rules! on_dialog {
        ($setter:ident, |$app:ident| $fut:expr) => {{
            let app = Rc::clone(app);
            ui.$setter(move || {
                let $app = Rc::clone(&app);
                let _ = slint::spawn_local($fut);
            });
        }};
    }

    on!(on_play_pause, |a| a.play_pause());
    on!(on_stop_playback, |a| a.stop());
    on!(on_seek_to, |a, fraction: f32| a.seek_fraction(fraction));
    on!(on_seek_relative, |a, secs: f32| a.seek_relative(secs));
    on!(on_previous_item, |a| a.previous());
    on!(on_next_item, |a| a.next());
    on_dialog!(on_open_files, |app| async move {
        if let Some(files) = rfd::AsyncFileDialog::new()
            .set_title("Ouvrir des médias")
            .add_filter("Médias", oxiplay::app::MEDIA_EXTENSIONS)
            .add_filter("Tous les fichiers", &["*"])
            .pick_files()
            .await
        {
            let sources = files
                .iter()
                .map(|f| f.path().to_string_lossy().into_owned())
                .collect();
            app.borrow_mut().add_sources(sources);
        }
    });
    on_dialog!(on_open_bluray, |app| async move {
        if let Some(dir) = rfd::AsyncFileDialog::new()
            .set_title("Ouvrir un dossier ou disque Blu-ray (BDMV)")
            .pick_folder()
            .await
        {
            app.borrow_mut()
                .add_sources(vec![dir.path().to_string_lossy().into_owned()]);
        }
    });
    on!(on_open_url, |a, url: slint::SharedString| a.open_url(&url));
    on!(on_open_recent, |a, index: i32| a.open_recent(index));
    on!(on_volume_changed, |a, v: f32| a.set_volume(v));
    on!(on_toggle_mute, |a| a.toggle_mute());
    on!(on_speed_selected, |a, index: i32| a.set_speed_index(index));
    on!(on_toggle_fullscreen, |a| a.toggle_fullscreen());
    on!(on_take_screenshot, |a| a.take_screenshot());
    on!(on_cycle_rotation, |a| a.cycle_rotation());
    on!(on_image_adjust, |a, b: f32, c: f32, s: f32| a
        .set_image_adjust(b, c, s));
    on!(on_playlist_activate, |a, idx: i32| a
        .playlist_activate(idx.max(0) as usize));
    on!(on_playlist_remove, |a, idx: i32| a
        .playlist_remove(idx.max(0) as usize));
    on!(on_playlist_shift, |a, idx: i32, delta: i32| a
        .playlist_shift(idx.max(0) as usize, delta));
    on!(on_playlist_search_changed, |a, t: slint::SharedString| a
        .set_playlist_search(&t));
    on!(on_playlist_group_selected, |a, idx: i32| a
        .set_playlist_group(idx));
    on!(on_playlist_toggle_favorite, |a, idx: i32| a
        .toggle_favorite(idx.max(0) as usize));
    on!(on_playlist_toggle_favorites_filter, |a| a
        .toggle_favorites_filter());
    on_dialog!(on_playlist_save, |app| async move {
        if let Some(file) = rfd::AsyncFileDialog::new()
            .set_title("Enregistrer la playlist")
            .add_filter("Playlist M3U", &["m3u", "m3u8"])
            .set_file_name("playlist.m3u")
            .save_file()
            .await
        {
            app.borrow_mut().save_playlist_to(file.path().to_path_buf());
        }
    });
    on_dialog!(on_playlist_load, |app| async move {
        if let Some(file) = rfd::AsyncFileDialog::new()
            .set_title("Charger une playlist")
            .add_filter("Playlist M3U", &["m3u", "m3u8"])
            .pick_file()
            .await
        {
            app.borrow_mut()
                .load_playlist_from(file.path().to_path_buf());
        }
    });
    on!(on_audio_track_selected, |a, idx: i32| a
        .select_audio_track(idx));
    on!(on_audio_device_selected, |a, idx: i32| a
        .select_audio_device(idx));
    on!(on_subtitle_track_selected, |a, idx: i32| a
        .select_subtitle_track(idx));
    on!(on_cycle_audio_track, |a| a.cycle_audio_track());
    on!(on_cycle_subtitle_track, |a| a.cycle_subtitle_track());
    on_dialog!(on_load_subtitle_file, |app| async move {
        if let Some(file) = rfd::AsyncFileDialog::new()
            .set_title("Charger des sous-titres")
            .add_filter("Sous-titres", oxiplay::app::SUBTITLE_EXTENSIONS)
            .pick_file()
            .await
        {
            app.borrow_mut()
                .load_subtitle_path(file.path().to_path_buf());
        }
    });
    on!(on_search_online_subs, |a| a.search_online_subtitles());
    on!(on_sub_delay_adjust, |a, delta: f32| a
        .adjust_subtitle_delay(delta));
    on!(on_subtitle_scale_adjust, |a, delta: f32| a
        .adjust_subtitle_scale(delta));
    on!(on_set_subtitle_color, |a, code: i32| a
        .set_subtitle_color(code));
    on!(on_toggle_theme, |a| a.toggle_theme());
    on!(on_cycle_language, |a| a.cycle_language());
    on!(on_open_update, |a| a.open_update());
    on!(on_eq_band_changed, |a, band: i32, gain: f32| a
        .set_equalizer_band(band, gain));
    on!(on_eq_reset, |a| a.reset_equalizer());
    on!(on_eq_preset_selected, |a, index: i32| a
        .apply_eq_preset(index));
    on!(on_toggle_mini, |a| a.toggle_mini());
    on!(on_cycle_repeat, |a| a.cycle_repeat());
    on!(on_cycle_sleep_timer, |a| a.cycle_sleep_timer());
    on!(on_toggle_ab_loop, |a| a.toggle_ab_loop());
    on!(on_toggle_stats, |a| a.toggle_stats());
    on!(on_chapter_selected, |a, idx: i32| a.select_chapter(idx));
    on!(on_step_frame, |a, forward: bool| a.step_frame(forward));
    on!(on_audio_delay_adjust, |a, delta: f32| a
        .adjust_audio_delay(delta));
}
