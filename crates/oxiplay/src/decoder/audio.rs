//! Thread de décodage audio : paquets compressés → échantillons stéréo
//! `f32` à la fréquence du périphérique, poussés dans l'[`AudioQueue`].
//!
//! Le traitement (vitesse sans changement de hauteur via `atempo`, égaliseur
//! 10 bandes, conversion finale) est réalisé par un graphe libavfilter, voir
//! [`super::audio_filter`]. Le graphe est reconstruit lorsque le format
//! d'entrée, la vitesse ou les gains de l'égaliseur changent.

use super::audio_filter::{build_spec, AudioFilter};
use super::{ts_to_us, PacketMsg};
use crate::audio::AudioQueue;
use crate::player::state::SharedState;
use crossbeam_channel::{Receiver, RecvTimeoutError};
use ffmpeg_the_third as ffmpeg;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

/// Point d'entrée du thread de décodage audio.
pub fn run_audio_decoder(
    shared: Arc<SharedState>,
    rx: Receiver<PacketMsg>,
    queue: Arc<AudioQueue>,
    device_rate: u32,
) {
    let mut decoder: Option<ffmpeg::decoder::Audio> = None;
    let mut time_base = ffmpeg::Rational::new(1, 1_000_000);
    let mut filter: Option<AudioFilter> = None;
    let mut eof = false;

    loop {
        if shared.should_stop() {
            return;
        }

        let msg = match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(msg) => msg,
            Err(RecvTimeoutError::Timeout) => {
                // Après l'EOF, signale la fin réelle quand la file se vide.
                if eof && queue.is_empty() {
                    shared.audio_done.store(true, Ordering::Relaxed);
                }
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => return,
        };

        match msg {
            PacketMsg::Reconfigure {
                parameters,
                time_base: tb,
            } => {
                match ffmpeg::codec::context::Context::from_parameters(parameters)
                    .and_then(|ctx| ctx.decoder().audio())
                {
                    Ok(d) => {
                        log::info!("décodeur audio prêt : {:?} {} Hz", d.id(), d.rate());
                        decoder = Some(d);
                        time_base = tb;
                        filter = None;
                        eof = false;
                    }
                    Err(e) => shared.set_error(format!("décodeur audio indisponible : {e}")),
                }
            }
            PacketMsg::Flush => {
                if let Some(d) = decoder.as_mut() {
                    d.flush();
                }
                filter = None;
                eof = false;
            }
            PacketMsg::Packet {
                packet,
                time_base: tb,
                generation,
            } => {
                if generation != shared.current_generation() {
                    continue;
                }
                time_base = tb;
                eof = false;
                let Some(d) = decoder.as_mut() else { continue };
                if let Err(e) = d.send_packet(&packet) {
                    log::debug!("paquet audio rejeté : {e}");
                    continue;
                }
                drain_samples(
                    &shared,
                    d,
                    &queue,
                    &mut filter,
                    time_base,
                    device_rate,
                    generation,
                );
            }
            PacketMsg::Eof => {
                if let Some(d) = decoder.as_mut() {
                    let _ = d.send_eof();
                    let generation = shared.current_generation();
                    drain_samples(
                        &shared,
                        d,
                        &queue,
                        &mut filter,
                        time_base,
                        device_rate,
                        generation,
                    );
                }
                eof = true;
            }
        }
    }
}

/// Récupère et filtre toutes les trames disponibles du décodeur.
fn drain_samples(
    shared: &Arc<SharedState>,
    decoder: &mut ffmpeg::decoder::Audio,
    queue: &Arc<AudioQueue>,
    filter: &mut Option<AudioFilter>,
    time_base: ffmpeg::Rational,
    device_rate: u32,
    generation: u64,
) {
    let mut decoded = ffmpeg::frame::Audio::empty();
    while decoder.receive_frame(&mut decoded).is_ok() {
        if let Err(e) = filter_and_push(
            shared,
            &decoded,
            queue,
            filter,
            time_base,
            device_rate,
            generation,
        ) {
            log::warn!("filtrage audio échoué : {e}");
        }
    }
}

fn filter_and_push(
    shared: &Arc<SharedState>,
    decoded: &ffmpeg::frame::Audio,
    queue: &Arc<AudioQueue>,
    filter: &mut Option<AudioFilter>,
    time_base: ffmpeg::Rational,
    device_rate: u32,
    generation: u64,
) -> anyhow::Result<()> {
    if decoded.samples() == 0 {
        return Ok(());
    }

    let speed_milli = shared.speed_milli.load(Ordering::Relaxed).max(250);
    let eq_generation = shared.eq_generation.load(Ordering::Acquire);
    let in_format = decoded.format();
    let in_channels = decoded.ch_layout().channels();
    anyhow::ensure!(in_channels > 0, "disposition de canaux audio inconnue");
    let in_rate = decoded.rate();

    // (Re)construit le graphe si l'entrée, la vitesse ou l'égaliseur changent.
    let needs_rebuild = !matches!(
        filter,
        Some(f) if f.in_format == in_format
            && f.in_channels == in_channels
            && f.in_rate == in_rate
            && f.speed_milli == speed_milli
            && f.eq_generation == eq_generation
    );
    if needs_rebuild {
        let speed = speed_milli as f64 / 1000.0;
        let gains = shared.equalizer_gains();
        let spec = build_spec(speed, &gains, device_rate);
        *filter = Some(AudioFilter::new(
            in_format,
            &decoded.ch_layout(),
            in_rate,
            time_base,
            &spec,
            speed_milli,
            eq_generation,
        )?);
        log::debug!("graphe audio reconstruit : {spec}");
    }
    let filter = filter.as_mut().expect("filtre initialisé ci-dessus");

    let pts_us = decoded
        .timestamp()
        .or(decoded.pts())
        .map(|ts| ts_to_us(ts, time_base))
        .unwrap_or(-1);

    // Collecte les trames filtrées (souvent une seule) en un tampon stéréo.
    let mut samples: Vec<f32> = Vec::new();
    filter.process(decoded, |filtered| {
        let count = filtered.samples() * 2;
        let bytes = filtered.data(0);
        if bytes.len() < count * 4 {
            return;
        }
        // Sûr : le tampon AVFrame est aligné et contient `count` f32 packés.
        let slice = unsafe { std::slice::from_raw_parts(bytes.as_ptr().cast::<f32>(), count) };
        samples.extend_from_slice(slice);
    })?;

    if samples.is_empty() {
        return Ok(());
    }

    // Contre-pression : attend qu'il y ait de la place, sans bloquer les
    // arrêts ni les seeks.
    loop {
        if shared.should_stop() || generation != shared.current_generation() {
            return Ok(());
        }
        if queue.has_room() {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    queue.push(pts_us, &samples);
    Ok(())
}
