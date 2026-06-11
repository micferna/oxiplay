//! Tests d'intégration du pipeline de lecture, sans interface graphique.
//!
//! Un petit fichier de test (mire vidéo + tonalité audio) est généré avec
//! l'outil `ffmpeg` en ligne de commande ; le moteur l'ouvre ensuite comme
//! le ferait l'application. Si `ffmpeg` n'est pas installé, les tests sont
//! ignorés silencieusement (utile pour les environnements minimaux).

use oxiplay::player::PlayerEngine;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Génère (une fois) un MP4 de 2 s : mire 320×240 à 10 i/s + sinus 440 Hz.
fn test_media() -> Option<PathBuf> {
    let path = std::env::temp_dir().join("oxiplay-integration-test.mp4");
    if path.exists() {
        return Some(path);
    }
    let status = Command::new("ffmpeg")
        .args([
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=2:size=320x240:rate=10",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=2",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-c:a",
            "aac",
            "-shortest",
        ])
        .arg(&path)
        .status()
        .ok()?;
    status.success().then_some(path)
}

/// Attend qu'une condition devienne vraie (timeout généreux pour la CI).
fn wait_for(what: &str, timeout: Duration, mut cond: impl FnMut() -> bool) {
    let start = Instant::now();
    while !cond() {
        assert!(
            start.elapsed() < timeout,
            "délai dépassé en attendant : {what}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn open_decode_seek_and_stop() {
    let Some(media) = test_media() else {
        eprintln!("ffmpeg introuvable : test d'intégration ignoré");
        return;
    };
    ffmpeg_the_third::init().unwrap();

    let frames = Arc::new(AtomicUsize::new(0));
    let sink_frames = Arc::clone(&frames);
    let engine = PlayerEngine::open(
        &media.to_string_lossy(),
        None, // pas de périphérique audio en CI
        Box::new(move |_frame| {
            sink_frames.fetch_add(1, Ordering::Relaxed);
        }),
        None,
    );

    // Métadonnées découvertes par le demuxeur.
    wait_for("la durée du média", Duration::from_secs(10), || {
        engine.duration_us() > 0
    });
    let duration = engine.duration_us();
    assert!(
        (1_500_000..=3_000_000).contains(&duration),
        "durée inattendue : {duration} µs"
    );
    wait_for("la découverte des pistes", Duration::from_secs(5), || {
        !engine.shared.audio_tracks.lock().unwrap().is_empty()
    });
    assert!(engine.shared.has_video.load(Ordering::Relaxed));

    // Des images sont décodées et présentées au rythme de l'horloge.
    wait_for("les premières images", Duration::from_secs(10), || {
        frames.load(Ordering::Relaxed) >= 3
    });

    // La position avance.
    wait_for("l'avancement de l'horloge", Duration::from_secs(5), || {
        engine.position_us() > 200_000
    });

    // Pause : l'horloge se fige.
    engine.set_paused(true);
    let pos = engine.position_us();
    std::thread::sleep(Duration::from_millis(150));
    assert!((engine.position_us() - pos).abs() < 1_000);

    // Seek en pause : aperçu immédiat de la nouvelle image.
    let before = frames.load(Ordering::Relaxed);
    engine.seek(1_500_000);
    wait_for("la position après seek", Duration::from_secs(5), || {
        (engine.position_us() - 1_500_000).abs() < 400_000
    });
    wait_for(
        "l'aperçu après seek en pause",
        Duration::from_secs(5),
        || frames.load(Ordering::Relaxed) > before,
    );

    // Vitesse + reprise : l'horloge repart.
    engine.set_speed(2.0);
    engine.set_paused(false);
    let pos = engine.position_us();
    wait_for("l'avancement à 2x", Duration::from_secs(5), || {
        engine.position_us() > pos + 100_000
    });

    // Une dernière image est disponible pour la capture d'écran.
    assert!(engine.shared.last_frame.lock().unwrap().is_some());

    // L'arrêt (drop) rejoint tous les threads sans blocage.
    let start = Instant::now();
    drop(engine);
    assert!(start.elapsed() < Duration::from_secs(3), "arrêt trop lent");
}

#[test]
fn playback_reaches_end() {
    let Some(media) = test_media() else {
        eprintln!("ffmpeg introuvable : test d'intégration ignoré");
        return;
    };
    ffmpeg_the_third::init().unwrap();

    let engine = PlayerEngine::open(
        &media.to_string_lossy(),
        None,
        Box::new(|_| {}),
        // Reprise quasi en fin de fichier pour un test rapide.
        Some(1_700_000),
    );
    wait_for("la fin de lecture", Duration::from_secs(15), || {
        engine.shared.playback_finished()
    });
}
