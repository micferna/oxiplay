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
    ffmpeg_next::init()?;
    // Réduit la verbosité des bibliothèques FFmpeg elles-mêmes.
    ffmpeg_next::util::log::set_level(ffmpeg_next::util::log::Level::Error);

    let main_window = MainWindow::new()?;
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

    app.borrow_mut().shutdown();
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

    on!(on_play_pause, |a| a.play_pause());
    on!(on_stop_playback, |a| a.stop());
    on!(on_seek_to, |a, fraction: f32| a.seek_fraction(fraction));
    on!(on_seek_relative, |a, secs: f32| a.seek_relative(secs));
    on!(on_previous_item, |a| a.previous());
    on!(on_next_item, |a| a.next());
    on!(on_open_files, |a| a.add_files_dialog());
    on!(on_open_url, |a, url: slint::SharedString| a.open_url(&url));
    on!(on_volume_changed, |a, v: f32| a.set_volume(v));
    on!(on_toggle_mute, |a| a.toggle_mute());
    on!(on_speed_selected, |a, index: i32| a.set_speed_index(index));
    on!(on_toggle_fullscreen, |a| a.toggle_fullscreen());
    on!(on_take_screenshot, |a| a.take_screenshot());
    on!(on_playlist_activate, |a, idx: i32| a
        .playlist_activate(idx.max(0) as usize));
    on!(on_playlist_remove, |a, idx: i32| a
        .playlist_remove(idx.max(0) as usize));
    on!(on_playlist_shift, |a, idx: i32, delta: i32| a
        .playlist_shift(idx.max(0) as usize, delta));
    on!(on_playlist_save, |a| a.playlist_save_dialog());
    on!(on_playlist_load, |a| a.playlist_load_dialog());
    on!(on_audio_track_selected, |a, idx: i32| a
        .select_audio_track(idx));
    on!(on_subtitle_track_selected, |a, idx: i32| a
        .select_subtitle_track(idx));
    on!(on_load_subtitle_file, |a| a.load_subtitle_dialog());
    on!(on_sub_delay_adjust, |a, delta: f32| a
        .adjust_subtitle_delay(delta));
    on!(on_toggle_theme, |a| a.toggle_theme());
}
