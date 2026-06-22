//! Thread de décodage vidéo : paquets compressés → images RGBA8 horodatées.
//!
//! La conversion d'espace colorimétrique (YUV → RGBA) est faite ici par
//! libswscale, hors du thread d'interface, pour ne jamais bloquer l'UI.

use super::hwaccel::{self, HwAccel};
use super::video_filter::VideoFilter;
use super::{ts_to_us, PacketMsg, VideoFrameMsg};
use crate::player::state::SharedState;
use crate::video::VideoFrameData;
use crossbeam_channel::{Receiver, RecvTimeoutError, SendTimeoutError, Sender};
use ffmpeg_the_third as ffmpeg;
use ffmpeg_the_third::ffi;
use ffmpeg_the_third::software::scaling;
use std::sync::atomic::Ordering;
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
    let mut hw: Option<HwAccel> = None;
    let mut time_base = ffmpeg::Rational::new(1, 1_000_000);
    let mut scaler: Option<ScalerCache> = None;
    let mut vfilter: Option<VideoFilter> = None;
    // Désentrelacement automatique des flux entrelacés, désactivable.
    let deint_enabled = std::env::var_os("OXIPLAY_NO_DEINTERLACE").is_none();
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
                hw = None;
                let opened = ffmpeg::codec::context::Context::from_parameters(parameters).and_then(
                    |mut ctx| {
                        // Active l'accélération matérielle (si activée et
                        // disponible) avant l'ouverture du décodeur ; les
                        // réglages restent posés sur le même AVCodecContext.
                        if shared.hwaccel_enabled.load(Ordering::Relaxed) {
                            hw = unsafe { hwaccel::setup(ctx.as_mut_ptr()) };
                        }
                        ctx.decoder().video()
                    },
                );
                match opened {
                    Ok(d) => {
                        log::info!(
                            "décodeur vidéo prêt : {:?} {}x{} ({})",
                            d.id(),
                            d.width(),
                            d.height(),
                            hw.as_ref().map(|h| h.name).unwrap_or("logiciel"),
                        );
                        decoder = Some(d);
                        time_base = tb;
                        scaler = None;
                        vfilter = None;
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
                    hw.as_ref(),
                    &tx,
                    &mut scaler,
                    &mut vfilter,
                    deint_enabled,
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
                        hw.as_ref(),
                        &tx,
                        &mut scaler,
                        &mut vfilter,
                        deint_enabled,
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
#[allow(clippy::too_many_arguments)]
fn drain_frames(
    shared: &Arc<SharedState>,
    decoder: &mut ffmpeg::decoder::Video,
    hw: Option<&HwAccel>,
    tx: &Sender<VideoFrameMsg>,
    scaler: &mut Option<ScalerCache>,
    vfilter: &mut Option<VideoFilter>,
    deint_enabled: bool,
    time_base: ffmpeg::Rational,
    last_pts_us: &mut i64,
    generation: u64,
) {
    let mut decoded = ffmpeg::frame::Video::empty();
    while decoder.receive_frame(&mut decoded).is_ok() {
        // Trame GPU : rapatriement en mémoire système pour le pipeline RGBA.
        let software;
        let frame_ref = match hw {
            Some(h) if decoded.format() == h.hw_pixel() => match hwaccel::transfer(&decoded) {
                Some(sw) => {
                    software = sw;
                    &software
                }
                None => {
                    log::warn!("rapatriement de trame matérielle échoué");
                    continue;
                }
            },
            _ => &decoded,
        };

        // Filtres vidéo (désentrelacement entrelacé + rotation) : on ne
        // traverse le graphe que si au moins un filtre est requis ; le
        // contenu progressif non tourné passe directement vers le scaler.
        let rotation = shared.rotation.load(Ordering::Relaxed);
        let spec = filter_spec(
            rotation,
            shared.brightness_milli.load(Ordering::Relaxed),
            shared.contrast_milli.load(Ordering::Relaxed),
            shared.saturation_milli.load(Ordering::Relaxed),
            deint_enabled,
            frame_ref.is_interlaced(),
        );
        let keep = match spec {
            Some(spec) => match filter_and_emit(
                shared,
                frame_ref,
                tx,
                scaler,
                vfilter,
                &spec,
                time_base,
                last_pts_us,
                generation,
            ) {
                Ok(k) => k,
                Err(e) => {
                    log::warn!("filtre vidéo échoué : {e}");
                    emit_frame(
                        shared,
                        frame_ref,
                        tx,
                        scaler,
                        time_base,
                        last_pts_us,
                        generation,
                    )
                }
            },
            None => {
                // Aucun filtre nécessaire : libère un graphe devenu inutile.
                if vfilter.is_some() {
                    *vfilter = None;
                }
                emit_frame(
                    shared,
                    frame_ref,
                    tx,
                    scaler,
                    time_base,
                    last_pts_us,
                    generation,
                )
            }
        };
        if !keep {
            return;
        }
    }
}

/// Construit la spec de filtres vidéo (désentrelacement + rotation + réglages
/// d'image) pour la trame courante, ou `None` si aucun filtre n'est requis.
/// `yadif=deint=1` laisse passer le progressif, on peut donc l'inclure dès
/// qu'on filtre déjà pour la rotation.
fn filter_spec(
    rotation: u8,
    brightness_milli: i32,
    contrast_milli: i32,
    saturation_milli: i32,
    deint_enabled: bool,
    interlaced: bool,
) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if deint_enabled && (interlaced || rotation != 0) {
        parts.push("yadif=mode=0:parity=-1:deint=1".to_string());
    }
    match rotation {
        1 => parts.push("transpose=1".to_string()), // 90° horaire
        2 => parts.push("transpose=1,transpose=1".to_string()), // 180°
        3 => parts.push("transpose=2".to_string()), // 270° (90° anti-horaire)
        _ => {}
    }
    if brightness_milli != 0 || contrast_milli != 1000 || saturation_milli != 1000 {
        parts.push(format!(
            "eq=brightness={:.3}:contrast={:.3}:saturation={:.3}",
            brightness_milli as f64 / 1000.0,
            contrast_milli as f64 / 1000.0,
            saturation_milli as f64 / 1000.0,
        ));
    }
    (!parts.is_empty()).then(|| parts.join(","))
}

/// Convertit une trame logicielle en RGBA, incruste les sous-titres image et
/// l'envoie au présentateur. Renvoie `false` si le thread doit cesser de
/// drainer (arrêt, seek périmant la génération, ou canal fermé).
fn emit_frame(
    shared: &Arc<SharedState>,
    frame_ref: &ffmpeg::frame::Video,
    tx: &Sender<VideoFrameMsg>,
    scaler: &mut Option<ScalerCache>,
    time_base: ffmpeg::Rational,
    last_pts_us: &mut i64,
    generation: u64,
) -> bool {
    let mut frame = match convert_frame(frame_ref, scaler, time_base, last_pts_us) {
        Ok(f) => f,
        Err(e) => {
            log::warn!("conversion d'image échouée : {e}");
            return true; // trame ignorée, on continue à drainer
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
            return false;
        }
        match tx.send_timeout(m, Duration::from_millis(50)) {
            Ok(()) => {}
            Err(SendTimeoutError::Timeout(m)) => msg = Some(m),
            Err(SendTimeoutError::Disconnected(_)) => return false,
        }
    }
    true
}

/// Filtre une trame via le graphe vidéo (`spec` ; construit/réutilisé à la
/// volée selon la spec et la géométrie) et émet chaque image produite. Renvoie
/// `false` pour stopper le drainage (propagé depuis [`emit_frame`]).
#[allow(clippy::too_many_arguments)]
fn filter_and_emit(
    shared: &Arc<SharedState>,
    frame_ref: &ffmpeg::frame::Video,
    tx: &Sender<VideoFrameMsg>,
    scaler: &mut Option<ScalerCache>,
    vfilter: &mut Option<VideoFilter>,
    spec: &str,
    time_base: ffmpeg::Rational,
    last_pts_us: &mut i64,
    generation: u64,
) -> anyhow::Result<bool> {
    let (width, height) = (frame_ref.width(), frame_ref.height());
    let format = frame_ref.format();
    let needs_rebuild = !matches!(
        vfilter,
        Some(f) if f.spec == spec && f.format == format && f.width == width && f.height == height
    );
    if needs_rebuild {
        *vfilter = Some(VideoFilter::new(
            spec,
            format,
            width,
            height,
            time_base,
            frame_ref.aspect_ratio(),
        )?);
    }
    let filter = vfilter.as_mut().expect("filtre vidéo initialisé ci-dessus");

    let mut keep = true;
    filter.process(frame_ref, |filtered| {
        if keep {
            keep = emit_frame(
                shared,
                filtered,
                tx,
                scaler,
                time_base,
                last_pts_us,
                generation,
            );
        }
    })?;
    Ok(keep)
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

/// Renseigne swscale sur l'espace et la plage colorimétriques de la source.
///
/// Par défaut, swscale applique les coefficients **BT.601**, ce qui décale
/// les couleurs de tout le contenu HD (BT.709) et HDR (BT.2020). On choisit
/// les coefficients selon l'espace signalé par le décodeur (avec un repli
/// heuristique sur la résolution : SD → 601, HD/UHD → 709), et la plage
/// (limitée 16–235 par défaut, complète 0–255 si « JPEG »).
fn apply_colorspace(
    scaler: &mut scaling::Context,
    space: ffmpeg::color::Space,
    range: ffmpeg::color::Range,
    height: u32,
) {
    use ffmpeg::color::{Range, Space};
    let cs = match space {
        Space::BT709 => ffi::SWS_CS_ITU709,
        Space::BT2020NCL | Space::BT2020CL => ffi::SWS_CS_BT2020,
        Space::FCC => ffi::SWS_CS_FCC,
        Space::SMPTE240M => ffi::SWS_CS_SMPTE240M,
        Space::BT470BG | Space::SMPTE170M => ffi::SWS_CS_ITU601,
        // Inconnu : convention des lecteurs — SD (≤ 576 lignes) en BT.601,
        // au-delà en BT.709.
        _ => {
            if height <= 576 {
                ffi::SWS_CS_ITU601
            } else {
                ffi::SWS_CS_ITU709
            }
        }
    };
    let src_range = if matches!(range, Range::JPEG) { 1 } else { 0 };

    // SAFETY : le SwsContext vient d'être créé (non nul) ; sws_getCoefficients
    // renvoie une table statique de 4 entiers pour tout SWS_CS_* valide.
    unsafe {
        let inv = ffi::sws_getCoefficients(cs);
        let table = ffi::sws_getCoefficients(ffi::SWS_CS_DEFAULT);
        ffi::sws_setColorspaceDetails(
            scaler.as_mut_ptr(),
            inv,
            src_range,
            table,
            1,       // sortie RGB : plage complète
            0,       // luminosité
            1 << 16, // contraste (1.0 en virgule fixe 16.16)
            1 << 16, // saturation
        );
    }
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
        let mut scaler = scaling::Context::get(
            decoded.format(),
            width,
            height,
            ffmpeg::format::Pixel::RGBA,
            width,
            height,
            scaling::Flags::BILINEAR,
        )?;
        // Sans ceci, swscale suppose BT.601 : couleurs fausses en HD (BT.709)
        // et HDR (BT.2020). On lui donne les bons coefficients et la bonne
        // plage selon ce que le décodeur a signalé.
        apply_colorspace(
            &mut scaler,
            decoded.color_space(),
            decoded.color_range(),
            height,
        );
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
