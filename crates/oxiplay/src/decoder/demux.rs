//! Thread de demuxage : lit les paquets du conteneur (fichier ou flux
//! réseau), les route vers les décodeurs vidéo/audio, traite les commandes
//! (seek, changement de piste) et décode les sous-titres embarqués.

use super::{ts_to_us, DemuxCommand, PacketMsg};
use crate::audio::AudioQueue;
use crate::player::state::{ChapterInfo, SharedState, TrackInfo};
use crate::subtitles::{embedded_ass_to_styled, CueStyle, SubtitleCue, SubtitleTrack};
use crossbeam_channel::{Receiver, SendTimeoutError, Sender};
use ffmpeg_the_third as ffmpeg;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

/// Paramètres de lancement du thread de demuxage.
pub struct DemuxConfig {
    /// Chemin local ou URL.
    pub source: String,
    /// Position de départ (reprise de lecture), en µs.
    pub start_at_us: Option<i64>,
    /// Faux si aucun périphérique audio n'est disponible : l'audio est
    /// alors entièrement ignoré.
    pub audio_enabled: bool,
}

/// Durée de sous-titre par défaut quand le conteneur ne la fournit pas.
const DEFAULT_SUB_DURATION_US: i64 = 3_000_000;

struct DemuxState {
    shared: Arc<SharedState>,
    cmd_rx: Receiver<DemuxCommand>,
    video_tx: Sender<PacketMsg>,
    audio_tx: Sender<PacketMsg>,
    audio_queue: Option<Arc<AudioQueue>>,
    video_stream: Option<usize>,
    audio_stream: Option<usize>,
    subtitle_stream: Option<usize>,
    subtitle_decoder: Option<ffmpeg::decoder::Subtitle>,
    seek_requested: Option<i64>,
}

/// Point d'entrée du thread de demuxage.
pub fn run_demux(
    config: DemuxConfig,
    shared: Arc<SharedState>,
    cmd_rx: Receiver<DemuxCommand>,
    video_tx: Sender<PacketMsg>,
    audio_tx: Sender<PacketMsg>,
    audio_queue: Option<Arc<AudioQueue>>,
) {
    if let Err(e) = demux_loop(config, &shared, cmd_rx, video_tx, audio_tx, audio_queue) {
        shared.set_error(format!("erreur de lecture : {e}"));
        // Termine proprement la session : l'UI verra l'erreur.
        shared.demux_eof.store(true, Ordering::Relaxed);
        shared.video_done.store(true, Ordering::Relaxed);
        shared.audio_done.store(true, Ordering::Relaxed);
    }
}

fn demux_loop(
    config: DemuxConfig,
    shared: &Arc<SharedState>,
    cmd_rx: Receiver<DemuxCommand>,
    video_tx: Sender<PacketMsg>,
    audio_tx: Sender<PacketMsg>,
    audio_queue: Option<Arc<AudioQueue>>,
) -> anyhow::Result<()> {
    // Normalise la source (dossier BDMV / image .iso → protocole bluray:).
    let source = crate::streaming::normalize_source(&config.source);
    // Page web (YouTube, Vimeo…) → URL de flux directe via yt-dlp (best
    // effort ; conserve l'URL d'origine si yt-dlp est absent ou échoue).
    let source = if crate::ytdlp::should_resolve(&source) {
        crate::ytdlp::resolve(&source).unwrap_or(source)
    } else {
        source
    };
    let kind = crate::streaming::classify(&source);
    let options = crate::streaming::demux_options(kind);
    let mut ictx = ffmpeg::format::input_with_dictionary(std::path::Path::new(&source), options)?;

    let duration_us = if ictx.duration() > 0 {
        // `Input::duration` est en unités AV_TIME_BASE (µs).
        ictx.duration()
    } else {
        0
    };
    shared.duration_us.store(duration_us, Ordering::Relaxed);

    let mut st = DemuxState {
        shared: Arc::clone(shared),
        cmd_rx,
        video_tx,
        audio_tx,
        audio_queue,
        video_stream: None,
        audio_stream: None,
        subtitle_stream: None,
        subtitle_decoder: None,
        seek_requested: None,
    };

    discover_streams(&mut st, &ictx, config.audio_enabled);

    // Configure les décodeurs avant le premier paquet.
    if let Some(idx) = st.video_stream {
        send_reconfigure(&st.video_tx, &ictx, idx)?;
    }
    if let Some(idx) = st.audio_stream {
        send_reconfigure(&st.audio_tx, &ictx, idx)?;
    }

    // Reprise de lecture éventuelle.
    if let Some(start) = config.start_at_us {
        st.seek_requested = Some(start);
    }

    let mut eof = false;
    loop {
        if st.shared.should_stop() {
            return Ok(());
        }

        // Commandes en attente (non bloquant).
        while let Ok(cmd) = st.cmd_rx.try_recv() {
            handle_command(&mut st, &ictx, cmd)?;
        }

        if let Some(target) = st.seek_requested.take() {
            perform_seek(&mut st, &mut ictx, target)?;
            eof = false;
        }

        if eof {
            // Plus rien à lire : on attend d'éventuelles commandes (seek).
            match st.cmd_rx.recv_timeout(Duration::from_millis(100)) {
                Ok(cmd) => handle_command(&mut st, &ictx, cmd)?,
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return Ok(()),
            }
            continue;
        }

        // Lit le paquet suivant. L'itérateur est faillible : une erreur de
        // lecture transitoire (réseau) est journalisée et on réessaie ; le
        // timeout d'I/O évite tout blocage indéfini.
        match ictx.packets().next() {
            Some(Ok((stream, packet))) => {
                route_packet(&mut st, stream, packet)?;
            }
            Some(Err(e)) => {
                log::debug!("lecture de paquet échouée : {e}");
            }
            None => {
                eof = true;
                st.shared.demux_eof.store(true, Ordering::Relaxed);
                let _ = st.video_tx.send(PacketMsg::Eof);
                let _ = st.audio_tx.send(PacketMsg::Eof);
            }
        }
    }
}

/// Inventorie les flux du conteneur : meilleurs flux vidéo/audio par
/// défaut, et listes de pistes pour l'interface.
fn discover_streams(
    st: &mut DemuxState,
    ictx: &ffmpeg::format::context::Input,
    audio_enabled: bool,
) {
    use ffmpeg::media::Type;

    let best_video = ictx.streams().best(Type::Video).map(|s| s.index());
    // Ignore les pochettes d'album (flux vidéo « attached picture »).
    st.video_stream = best_video.filter(|&idx| {
        let stream = ictx.stream(idx).unwrap();
        !stream
            .disposition()
            .contains(ffmpeg::format::stream::Disposition::ATTACHED_PIC)
    });
    st.audio_stream = if audio_enabled {
        ictx.streams().best(Type::Audio).map(|s| s.index())
    } else {
        None
    };

    let mut audio_tracks = Vec::new();
    let mut subtitle_tracks = Vec::new();
    for stream in ictx.streams() {
        let params = stream.parameters();
        let medium = params.medium();
        if medium != Type::Audio && medium != Type::Subtitle {
            continue;
        }
        let meta = stream.metadata();
        let info = TrackInfo {
            stream_index: stream.index(),
            language: meta.get("language").map(str::to_string),
            title: meta.get("title").map(str::to_string),
            codec: format!("{:?}", params.id()).to_lowercase(),
        };
        match medium {
            Type::Audio => audio_tracks.push(info),
            Type::Subtitle => subtitle_tracks.push(info),
            _ => unreachable!(),
        }
    }

    st.shared
        .has_video
        .store(st.video_stream.is_some(), Ordering::Relaxed);
    st.shared
        .has_audio
        .store(st.audio_stream.is_some(), Ordering::Relaxed);
    *st.shared.audio_tracks.lock().unwrap() = audio_tracks;
    *st.shared.subtitle_tracks.lock().unwrap() = subtitle_tracks;

    // Durée d'une image (pour l'avance image par image), depuis la cadence
    // moyenne du flux vidéo.
    if let Some(idx) = st.video_stream {
        if let Some(stream) = ictx.stream(idx) {
            let fps = stream.avg_frame_rate();
            let (num, den) = (fps.numerator() as i64, fps.denominator() as i64);
            if num > 0 && den > 0 {
                st.shared
                    .frame_duration_us
                    .store(den * 1_000_000 / num, Ordering::Relaxed);
            }
        }
    }

    // Chapitres (points de navigation) : titre depuis les métadonnées, sinon
    // « Chapitre N ».
    let mut chapters = Vec::new();
    for (i, chapter) in ictx.chapters().enumerate() {
        let start_us = ts_to_us(chapter.start(), chapter.time_base());
        let title = chapter
            .metadata()
            .get("title")
            .map(str::to_string)
            .unwrap_or_else(|| format!("Chapitre {}", i + 1));
        chapters.push(ChapterInfo { start_us, title });
    }
    *st.shared.chapters.lock().unwrap() = chapters;

    // Fiche d'informations média (affichée à la demande).
    let duration_us = st.shared.duration_us.load(Ordering::Relaxed);
    *st.shared.media_info.lock().unwrap() =
        build_media_info(ictx, st.video_stream, st.audio_stream, duration_us);

    log::info!(
        "flux découverts : vidéo={:?} audio={:?} ({} pistes audio, {} pistes st)",
        st.video_stream,
        st.audio_stream,
        st.shared.audio_tracks.lock().unwrap().len(),
        st.shared.subtitle_tracks.lock().unwrap().len(),
    );
}

/// Envoie la configuration d'un décodeur (paramètres + time base du flux).
fn send_reconfigure(
    tx: &Sender<PacketMsg>,
    ictx: &ffmpeg::format::context::Input,
    stream_index: usize,
) -> anyhow::Result<()> {
    let stream = ictx
        .stream(stream_index)
        .ok_or_else(|| anyhow::anyhow!("flux {stream_index} introuvable"))?;
    tx.send(PacketMsg::Reconfigure {
        parameters: owned_parameters(&stream.parameters()),
        time_base: stream.time_base(),
    })
    .map_err(|_| anyhow::anyhow!("décodeur arrêté"))
}

/// Copie en profondeur des paramètres de codec empruntés vers une valeur
/// possédée, transmissible à un thread de décodage.
///
/// `stream.parameters()` renvoie une vue empruntée (`ParametersRef`) liée au
/// conteneur ; le pipeline a besoin d'une copie indépendante.
fn owned_parameters(
    params: &ffmpeg::codec::parameters::ParametersRef,
) -> ffmpeg::codec::Parameters {
    let mut owned = ffmpeg::codec::Parameters::new();
    // SAFETY : les deux pointeurs proviennent d'allocations FFmpeg valides et
    // non nulles ; `avcodec_parameters_copy` réalise une copie profonde.
    unsafe {
        ffmpeg::ffi::avcodec_parameters_copy(owned.as_mut_ptr(), params.as_ptr());
    }
    owned
}

/// Construit la fiche d'informations média (texte multi-lignes : conteneur,
/// codecs, résolution, HDR, débits, durée) affichée à la demande dans l'UI.
fn build_media_info(
    ictx: &ffmpeg::format::context::Input,
    video: Option<usize>,
    audio: Option<usize>,
    duration_us: i64,
) -> String {
    let mut lines = vec![format!("Conteneur : {}", ictx.format().description())];

    if let Some(stream) = video.and_then(|i| ictx.stream(i)) {
        let params = stream.parameters();
        let codec = format!("{:?}", params.id()).to_lowercase();
        // SAFETY : le codecpar est valide tant que le conteneur vit ; on lit
        // seulement des champs scalaires documentés, en lecture seule.
        let (w, h, br, trc) = unsafe {
            let p = params.as_ptr();
            ((*p).width, (*p).height, (*p).bit_rate, (*p).color_trc)
        };
        let fps = stream.avg_frame_rate();
        let fps_str = if fps.numerator() > 0 && fps.denominator() > 0 {
            format!(
                ", {:.2} i/s",
                fps.numerator() as f64 / fps.denominator() as f64
            )
        } else {
            String::new()
        };
        lines.push(format!(
            "Vidéo : {codec}, {w}×{h}{fps_str}, {}",
            fmt_bitrate(br)
        ));
        let hdr = trc == ffmpeg::ffi::AVColorTransferCharacteristic::SMPTE2084
            || trc == ffmpeg::ffi::AVColorTransferCharacteristic::ARIB_STD_B67;
        if hdr {
            lines.push("Dynamique : HDR (PQ/HLG)".to_string());
        }
    }

    if let Some(stream) = audio.and_then(|i| ictx.stream(i)) {
        let params = stream.parameters();
        let codec = format!("{:?}", params.id()).to_lowercase();
        let (chans, rate, br) = unsafe {
            let p = params.as_ptr();
            ((*p).ch_layout.nb_channels, (*p).sample_rate, (*p).bit_rate)
        };
        lines.push(format!(
            "Audio : {codec}, {}, {rate} Hz, {}",
            channel_label(chans),
            fmt_bitrate(br)
        ));
    }

    if duration_us > 0 {
        lines.push(format!(
            "Durée : {}",
            crate::utils::format_time(duration_us)
        ));
    }

    lines.join("\n")
}

/// Formate un débit binaire (≤ 0 = inconnu).
fn fmt_bitrate(bits_per_sec: i64) -> String {
    if bits_per_sec <= 0 {
        "débit inconnu".to_string()
    } else if bits_per_sec >= 1_000_000 {
        format!("{:.1} Mb/s", bits_per_sec as f64 / 1_000_000.0)
    } else {
        format!("{} kb/s", bits_per_sec / 1000)
    }
}

/// Libellé lisible d'un nombre de canaux audio.
fn channel_label(channels: i32) -> String {
    match channels {
        1 => "mono".to_string(),
        2 => "stéréo".to_string(),
        6 => "5.1".to_string(),
        8 => "7.1".to_string(),
        n => format!("{n} canaux"),
    }
}

fn handle_command(
    st: &mut DemuxState,
    ictx: &ffmpeg::format::context::Input,
    cmd: DemuxCommand,
) -> anyhow::Result<()> {
    match cmd {
        DemuxCommand::Seek(target_us) => {
            // Les seeks rapprochés (glissement de la barre) sont fusionnés :
            // seule la dernière cible compte.
            st.seek_requested = Some(target_us);
        }
        DemuxCommand::SelectAudioTrack(stream_index) => {
            if st.audio_stream != Some(stream_index) {
                st.audio_stream = Some(stream_index);
                if let Some(q) = &st.audio_queue {
                    q.clear();
                }
                send_reconfigure(&st.audio_tx, ictx, stream_index)?;
            }
        }
        DemuxCommand::SelectSubtitleTrack(selection) => {
            st.subtitle_stream = selection;
            st.subtitle_decoder = None;
            *st.shared.embedded_subtitles.lock().unwrap() = SubtitleTrack::default();
            st.shared.bitmap_subtitles.lock().unwrap().clear();
            if let Some(idx) = selection {
                if let Some(stream) = ictx.stream(idx) {
                    match ffmpeg::codec::context::Context::from_parameters(stream.parameters())
                        .and_then(|ctx| ctx.decoder().subtitle())
                    {
                        Ok(decoder) => st.subtitle_decoder = Some(decoder),
                        Err(e) => log::warn!("décodeur de sous-titres indisponible : {e}"),
                    }
                }
            }
        }
    }
    Ok(())
}

fn perform_seek(
    st: &mut DemuxState,
    ictx: &mut ffmpeg::format::context::Input,
    target_us: i64,
) -> anyhow::Result<()> {
    let duration = st.shared.duration_us.load(Ordering::Relaxed);
    let target = if duration > 0 {
        target_us.clamp(0, duration)
    } else {
        target_us.max(0)
    };

    // Invalide tout ce qui est déjà dans le pipeline.
    st.shared.generation.fetch_add(1, Ordering::AcqRel);
    if let Some(q) = &st.audio_queue {
        q.clear();
    }

    // `seek` prend des unités AV_TIME_BASE (µs). La plage `..=target`
    // autorise la keyframe située à la cible ou juste avant (le fork
    // ramène une borne exclue à `target-1`, ce qui peut vider la plage).
    if let Err(e) = ictx.seek(target, ..=target) {
        log::warn!("seek vers {target} µs impossible : {e}");
        return Ok(());
    }
    log::debug!("seek effectué vers {target} µs");

    st.shared.demux_eof.store(false, Ordering::Relaxed);
    st.shared.video_done.store(false, Ordering::Relaxed);
    st.shared.audio_done.store(false, Ordering::Relaxed);
    st.shared.bitmap_subtitles.lock().unwrap().clear();
    st.shared.clock.set_position(target);

    let _ = st.video_tx.send(PacketMsg::Flush);
    let _ = st.audio_tx.send(PacketMsg::Flush);
    // Reconfigure systématiquement les décodeurs : couvre aussi le cas
    // d'un changement de piste reçu pendant une contre-pression.
    if let Some(idx) = st.video_stream {
        send_reconfigure(&st.video_tx, ictx, idx)?;
    }
    if let Some(idx) = st.audio_stream {
        send_reconfigure(&st.audio_tx, ictx, idx)?;
    }
    Ok(())
}

/// Route un paquet vers le bon consommateur.
fn route_packet(
    st: &mut DemuxState,
    stream: ffmpeg::format::stream::Stream,
    packet: ffmpeg::Packet,
) -> anyhow::Result<()> {
    let index = stream.index();
    let time_base = stream.time_base();
    let generation = st.shared.current_generation();

    if Some(index) == st.video_stream {
        send_packet(st, true, packet, time_base, generation)
    } else if Some(index) == st.audio_stream {
        send_packet(st, false, packet, time_base, generation)
    } else if Some(index) == st.subtitle_stream {
        decode_subtitle(st, &packet, time_base);
        Ok(())
    } else {
        Ok(())
    }
}

/// Envoi avec contre-pression *interruptible* : pendant que le canal est
/// plein (pipeline saturé, ou lecture en pause), les commandes — seek
/// notamment — restent traitées.
fn send_packet(
    st: &mut DemuxState,
    to_video: bool,
    packet: ffmpeg::Packet,
    time_base: ffmpeg::Rational,
    generation: u64,
) -> anyhow::Result<()> {
    let mut msg = Some(PacketMsg::Packet {
        packet,
        time_base,
        generation,
    });
    while let Some(m) = msg.take() {
        if st.shared.should_stop() {
            return Ok(());
        }
        let tx = if to_video { &st.video_tx } else { &st.audio_tx };
        match tx.send_timeout(m, Duration::from_millis(50)) {
            Ok(()) => {}
            Err(SendTimeoutError::Timeout(m)) => {
                // Canal plein : traite les commandes puis réessaie.
                while let Ok(cmd) = st.cmd_rx.try_recv() {
                    // Le seek sera exécuté par la boucle principale ; le
                    // paquet courant devient périmé, on l'abandonne.
                    let is_seek = matches!(cmd, DemuxCommand::Seek(_));
                    // NB : handle_command n'a pas besoin d'ictx ici, on
                    // mémorise seulement la commande de seek.
                    if let DemuxCommand::Seek(t) = cmd {
                        st.seek_requested = Some(t);
                    } else {
                        // Les changements de piste sont rares : on les
                        // remet en file pour la boucle principale… qui ne
                        // peut pas les recevoir (try_recv les a consommés).
                        // On les traite donc immédiatement via un mini
                        // traitement local.
                        apply_track_command_inline(st, cmd);
                    }
                    if is_seek {
                        return Ok(());
                    }
                }
                msg = Some(m);
            }
            Err(SendTimeoutError::Disconnected(_)) => return Ok(()),
        }
    }
    Ok(())
}

/// Traitement minimal des commandes de piste lorsque la boucle principale
/// est bloquée en contre-pression (sans accès à `ictx` : la reconfiguration
/// complète sera faite par la prochaine itération de la boucle principale).
fn apply_track_command_inline(st: &mut DemuxState, cmd: DemuxCommand) {
    match cmd {
        DemuxCommand::SelectAudioTrack(idx) => {
            st.audio_stream = Some(idx);
            if let Some(q) = &st.audio_queue {
                q.clear();
            }
            // Marque la reconfiguration comme nécessaire via un seek sur
            // place : simple et robuste.
            let pos = st.shared.clock.now_us();
            st.seek_requested = Some(pos);
        }
        DemuxCommand::SelectSubtitleTrack(sel) => {
            st.subtitle_stream = sel;
            st.subtitle_decoder = None;
        }
        DemuxCommand::Seek(t) => st.seek_requested = Some(t),
    }
}

/// Décode un paquet de sous-titres embarqué et publie les répliques.
fn decode_subtitle(st: &mut DemuxState, packet: &ffmpeg::Packet, time_base: ffmpeg::Rational) {
    let Some(decoder) = st.subtitle_decoder.as_mut() else {
        return;
    };
    let mut subtitle = ffmpeg::Subtitle::new();
    match decoder.decode(packet, &mut subtitle) {
        Ok(true) => {}
        Ok(false) => return,
        Err(e) => {
            log::debug!("décodage de sous-titre échoué : {e}");
            return;
        }
    }

    let start_us = packet.pts().map(|p| ts_to_us(p, time_base)).unwrap_or(-1);
    if start_us < 0 {
        return;
    }
    let duration_us = if packet.duration() > 0 {
        ts_to_us(packet.duration(), time_base)
    } else {
        DEFAULT_SUB_DURATION_US
    };

    let end_us = start_us + duration_us;
    let mut text_parts: Vec<String> = Vec::new();
    let mut style = CueStyle::default();
    for rect in subtitle.rects() {
        match rect {
            ffmpeg::subtitle::Rect::Text(t) => text_parts.push(t.get().to_string()),
            ffmpeg::subtitle::Rect::Ass(a) => {
                let (text, parsed) = embedded_ass_to_styled(a.get());
                style = parsed;
                text_parts.push(text);
            }
            // Sous-titres image (PGS/DVD) : convertis en RGBA et incrustés
            // sur la vidéo (voir crate::subtitles::bitmap).
            ffmpeg::subtitle::Rect::Bitmap(b) => {
                if let Some(sub) = bitmap_rect_to_subtitle(&b, start_us, end_us) {
                    st.shared.bitmap_subtitles.lock().unwrap().insert(sub);
                }
            }
            ffmpeg::subtitle::Rect::None(_) => {}
        }
    }
    let text = text_parts.join("\n").trim().to_string();
    if text.is_empty() {
        return;
    }

    st.shared
        .embedded_subtitles
        .lock()
        .unwrap()
        .insert(SubtitleCue {
            start_us,
            end_us,
            text,
            style,
        });
}

/// Convertit un rectangle de sous-titre image (palettisé, `AV_PIX_FMT_PAL8`)
/// en bitmap RGBA prêt à incruster.
///
/// FFmpeg ne fournit pas d'accès sûr aux pixels/palette : on lit le
/// `AVSubtitleRect` brut. `data[0]` contient un indice de palette par pixel
/// (stride `linesize[0]`), `data[1]` la palette de `nb_colors` entrées au
/// format `AV_PIX_FMT_RGB32` (0xAARRGGBB en ordre natif).
fn bitmap_rect_to_subtitle(
    bitmap: &ffmpeg::subtitle::Bitmap,
    start_us: i64,
    end_us: i64,
) -> Option<crate::subtitles::BitmapSubtitle> {
    let (width, height) = (bitmap.width(), bitmap.height());
    if width == 0 || height == 0 {
        return None;
    }

    // SAFETY : `as_ptr` est un AVSubtitleRect valide tant que le Subtitle
    // parent vit ; on ne lit que ses champs PAL8 documentés, dans les bornes
    // données par width/height/linesize/nb_colors.
    let (x, y, rgba) = unsafe {
        let raw = bitmap.as_ptr();
        let indices = (*raw).data[0];
        let palette = (*raw).data[1] as *const u32;
        let stride = (*raw).linesize[0];
        let nb_colors = (*raw).nb_colors.max(0) as usize;
        if indices.is_null() || palette.is_null() || stride <= 0 || nb_colors == 0 {
            return None;
        }
        let mut rgba = vec![0u8; (width * height * 4) as usize];
        for row in 0..height as usize {
            let line = indices.add(row * stride as usize);
            for col in 0..width as usize {
                let idx = (*line.add(col)) as usize;
                if idx >= nb_colors {
                    continue;
                }
                let argb = *palette.add(idx);
                let a = ((argb >> 24) & 0xff) as u8;
                let r = ((argb >> 16) & 0xff) as u8;
                let g = ((argb >> 8) & 0xff) as u8;
                let b = (argb & 0xff) as u8;
                let o = (row * width as usize + col) * 4;
                rgba[o] = r;
                rgba[o + 1] = g;
                rgba[o + 2] = b;
                rgba[o + 3] = a;
            }
        }
        (bitmap.x() as u32, bitmap.y() as u32, rgba)
    };

    Some(crate::subtitles::BitmapSubtitle {
        start_us,
        end_us,
        x,
        y,
        width,
        height,
        rgba,
    })
}
