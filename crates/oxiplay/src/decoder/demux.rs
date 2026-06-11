//! Thread de demuxage : lit les paquets du conteneur (fichier ou flux
//! réseau), les route vers les décodeurs vidéo/audio, traite les commandes
//! (seek, changement de piste) et décode les sous-titres embarqués.

use super::{ts_to_us, DemuxCommand, PacketMsg};
use crate::audio::AudioQueue;
use crate::player::state::{SharedState, TrackInfo};
use crate::subtitles::{embedded_ass_to_text, SubtitleCue, SubtitleTrack};
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
    let kind = crate::streaming::classify(&config.source);
    let options = crate::streaming::demux_options(kind);
    let mut ictx =
        ffmpeg::format::input_with_dictionary(std::path::Path::new(&config.source), options)?;

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

    let mut text_parts: Vec<String> = Vec::new();
    for rect in subtitle.rects() {
        match rect {
            ffmpeg::subtitle::Rect::Text(t) => text_parts.push(t.get().to_string()),
            ffmpeg::subtitle::Rect::Ass(a) => text_parts.push(embedded_ass_to_text(a.get())),
            // Sous-titres bitmap (DVD/PGS) : nécessitent un rendu image,
            // prévu avec la voie wgpu (voir ARCHITECTURE.md).
            _ => {}
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
            end_us: start_us + duration_us,
            text,
        });
}
