//! Thread de décodage vidéo : paquets compressés → images RGBA8 horodatées.
//!
//! La conversion d'espace colorimétrique (YUV → RGBA) est faite ici par
//! libswscale, hors du thread d'interface, pour ne jamais bloquer l'UI.

use super::{ts_to_us, PacketMsg, VideoFrameMsg};
use crate::player::state::SharedState;
use crate::video::VideoFrameData;
use crossbeam_channel::{Receiver, RecvTimeoutError, SendTimeoutError, Sender};
use ffmpeg_the_third as ffmpeg;
use ffmpeg_the_third::software::scaling;
use std::sync::Arc;
use std::time::Duration;

/// État du convertisseur, recréé quand la géométrie ou le format change
/// (changement de résolution en cours de flux HLS, par exemple).
struct ScalerCache {
    scaler: scaling::Context,
    format: ffmpeg::format::Pixel,
    width: u32,
    height: u32,
}

/// Point d'entrée du thread de décodage vidéo.
pub fn run_video_decoder(
    shared: Arc<SharedState>,
    rx: Receiver<PacketMsg>,
    tx: Sender<VideoFrameMsg>,
) {
    let mut decoder: Option<ffmpeg::decoder::Video> = None;
    let mut time_base = ffmpeg::Rational::new(1, 1_000_000);
    let mut scaler: Option<ScalerCache> = None;
    let mut last_pts_us: i64 = 0;

    loop {
        if shared.should_stop() {
            return;
        }
        let msg = match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(msg) => msg,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return,
        };

        match msg {
            PacketMsg::Reconfigure {
                parameters,
                time_base: tb,
            } => {
                match ffmpeg::codec::context::Context::from_parameters(parameters)
                    .and_then(|ctx| ctx.decoder().video())
                {
                    Ok(d) => {
                        log::info!(
                            "décodeur vidéo prêt : {:?} {}x{}",
                            d.id(),
                            d.width(),
                            d.height()
                        );
                        decoder = Some(d);
                        time_base = tb;
                        scaler = None;
                    }
                    Err(e) => shared.set_error(format!("décodeur vidéo indisponible : {e}")),
                }
            }
            PacketMsg::Flush => {
                if let Some(d) = decoder.as_mut() {
                    d.flush();
                }
            }
            PacketMsg::Packet {
                packet,
                time_base: tb,
                generation,
            } => {
                // Paquet d'une génération périmée (seek depuis) : on le jette.
                if generation != shared.current_generation() {
                    continue;
                }
                time_base = tb;
                let Some(d) = decoder.as_mut() else { continue };
                if let Err(e) = d.send_packet(&packet) {
                    log::debug!("paquet vidéo rejeté : {e}");
                    continue;
                }
                drain_frames(
                    &shared,
                    d,
                    &tx,
                    &mut scaler,
                    time_base,
                    &mut last_pts_us,
                    generation,
                );
            }
            PacketMsg::Eof => {
                if let Some(d) = decoder.as_mut() {
                    let _ = d.send_eof();
                    let generation = shared.current_generation();
                    drain_frames(
                        &shared,
                        d,
                        &tx,
                        &mut scaler,
                        time_base,
                        &mut last_pts_us,
                        generation,
                    );
                }
                let _ = tx.send(VideoFrameMsg::Eof);
            }
        }
    }
}

/// Récupère toutes les images disponibles du décodeur et les transmet.
fn drain_frames(
    shared: &Arc<SharedState>,
    decoder: &mut ffmpeg::decoder::Video,
    tx: &Sender<VideoFrameMsg>,
    scaler: &mut Option<ScalerCache>,
    time_base: ffmpeg::Rational,
    last_pts_us: &mut i64,
    generation: u64,
) {
    let mut decoded = ffmpeg::frame::Video::empty();
    while decoder.receive_frame(&mut decoded).is_ok() {
        let mut frame = match convert_frame(&decoded, scaler, time_base, last_pts_us) {
            Ok(f) => f,
            Err(e) => {
                log::warn!("conversion d'image échouée : {e}");
                continue;
            }
        };
        composite_bitmap_subtitles(shared, &mut frame);
        let mut msg = Some(VideoFrameMsg::Frame {
            frame: Arc::new(frame),
            generation,
        });
        // Envoi bloquant mais interruptible (arrêt, seek).
        while let Some(m) = msg.take() {
            if shared.should_stop() || generation != shared.current_generation() {
                return;
            }
            match tx.send_timeout(m, Duration::from_millis(50)) {
                Ok(()) => {}
                Err(SendTimeoutError::Timeout(m)) => msg = Some(m),
                Err(SendTimeoutError::Disconnected(_)) => return,
            }
        }
    }
}

/// Incruste les sous-titres image (PGS/DVD) actifs sur l'image, en tenant
/// compte du décalage utilisateur des sous-titres.
fn composite_bitmap_subtitles(shared: &Arc<SharedState>, frame: &mut VideoFrameData) {
    let bitmaps = shared.bitmap_subtitles.lock().unwrap();
    if bitmaps.is_empty() {
        return;
    }
    let delay = shared
        .subtitle_delay_us
        .load(std::sync::atomic::Ordering::Relaxed);
    bitmaps.composite_active(
        &mut frame.pixels,
        frame.width,
        frame.height,
        frame.pts_us - delay,
    );
}

/// Convertit une image décodée (généralement YUV) en RGBA8 compact.
fn convert_frame(
    decoded: &ffmpeg::frame::Video,
    cache: &mut Option<ScalerCache>,
    time_base: ffmpeg::Rational,
    last_pts_us: &mut i64,
) -> anyhow::Result<VideoFrameData> {
    let (width, height) = (decoded.width(), decoded.height());
    anyhow::ensure!(width > 0 && height > 0, "image vide");
    // Garde-fou anti-OOM : une résolution démesurée annoncée par un fichier
    // piégé entraînerait une allocation RGBA gigantesque. 16384² couvre
    // largement la 8K/16K légitime (~1 Go en RGBA, déjà confortable).
    anyhow::ensure!(
        width <= 16_384 && height <= 16_384,
        "résolution rejetée : {width}x{height}"
    );

    // (Re)crée le scaler si le format d'entrée a changé.
    let needs_rebuild = !matches!(
        cache,
        Some(c) if c.format == decoded.format() && c.width == width && c.height == height
    );
    if needs_rebuild {
        let scaler = scaling::Context::get(
            decoded.format(),
            width,
            height,
            ffmpeg::format::Pixel::RGBA,
            width,
            height,
            scaling::Flags::BILINEAR,
        )?;
        *cache = Some(ScalerCache {
            scaler,
            format: decoded.format(),
            width,
            height,
        });
    }
    let cache = cache.as_mut().expect("scaler initialisé ci-dessus");

    let mut rgba = ffmpeg::frame::Video::empty();
    cache.scaler.run(decoded, &mut rgba)?;

    // Copie compacte ligne à ligne (le stride FFmpeg est souvent aligné).
    let stride = rgba.stride(0);
    let row_len = width as usize * 4;
    let data = rgba.data(0);
    let mut pixels = Vec::with_capacity(row_len * height as usize);
    for y in 0..height as usize {
        let start = y * stride;
        pixels.extend_from_slice(&data[start..start + row_len]);
    }

    // PTS : best effort, avec repli sur une cadence estimée.
    let pts_us = decoded
        .timestamp()
        .or(decoded.pts())
        .map(|ts| ts_to_us(ts, time_base))
        .unwrap_or(*last_pts_us + 33_333);
    *last_pts_us = pts_us;

    Ok(VideoFrameData {
        width,
        height,
        pixels,
        pts_us,
    })
}
