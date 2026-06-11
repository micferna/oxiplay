//! Thread de décodage audio : paquets compressés → échantillons stéréo
//! `f32` à la fréquence du périphérique, poussés dans l'[`AudioQueue`].
//!
//! Le contrôle de vitesse est réalisé par rééchantillonnage : pour une
//! vitesse `v`, le flux est rééchantillonné vers `taux_périphérique / v`
//! échantillons par seconde de média — joués à `taux_périphérique`, ils
//! durent `1/v` seconde réelle. (La hauteur est modifiée ; un filtre
//! `atempo` préservant la hauteur est prévu — voir ARCHITECTURE.md.)

use super::{ts_to_us, PacketMsg};
use crate::audio::AudioQueue;
use crate::player::state::SharedState;
use crossbeam_channel::{Receiver, RecvTimeoutError};
use ffmpeg_next as ffmpeg;
use ffmpeg_next::software::resampling;
use ffmpeg_next::util::format::sample::{Sample, Type as SampleType};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

/// État du resampler, recréé quand le format d'entrée ou la vitesse change.
struct ResamplerCache {
    resampler: resampling::Context,
    in_format: ffmpeg::format::Sample,
    in_layout: ffmpeg::ChannelLayout,
    in_rate: u32,
    speed_milli: u32,
}

/// Point d'entrée du thread de décodage audio.
pub fn run_audio_decoder(
    shared: Arc<SharedState>,
    rx: Receiver<PacketMsg>,
    queue: Arc<AudioQueue>,
    device_rate: u32,
) {
    let mut decoder: Option<ffmpeg::decoder::Audio> = None;
    let mut time_base = ffmpeg::Rational::new(1, 1_000_000);
    let mut cache: Option<ResamplerCache> = None;
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
                        cache = None;
                        eof = false;
                    }
                    Err(e) => shared.set_error(format!("décodeur audio indisponible : {e}")),
                }
            }
            PacketMsg::Flush => {
                if let Some(d) = decoder.as_mut() {
                    d.flush();
                }
                cache = None;
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
                    &mut cache,
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
                        &mut cache,
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

/// Récupère et rééchantillonne toutes les trames disponibles du décodeur.
fn drain_samples(
    shared: &Arc<SharedState>,
    decoder: &mut ffmpeg::decoder::Audio,
    queue: &Arc<AudioQueue>,
    cache: &mut Option<ResamplerCache>,
    time_base: ffmpeg::Rational,
    device_rate: u32,
    generation: u64,
) {
    let mut decoded = ffmpeg::frame::Audio::empty();
    while decoder.receive_frame(&mut decoded).is_ok() {
        if let Err(e) = resample_and_push(
            shared,
            &decoded,
            queue,
            cache,
            time_base,
            device_rate,
            generation,
        ) {
            log::warn!("rééchantillonnage échoué : {e}");
        }
    }
}

fn resample_and_push(
    shared: &Arc<SharedState>,
    decoded: &ffmpeg::frame::Audio,
    queue: &Arc<AudioQueue>,
    cache: &mut Option<ResamplerCache>,
    time_base: ffmpeg::Rational,
    device_rate: u32,
    generation: u64,
) -> anyhow::Result<()> {
    if decoded.samples() == 0 {
        return Ok(());
    }

    let speed_milli = shared.speed_milli.load(Ordering::Relaxed).max(250);
    let in_format = decoded.format();
    let mut in_layout = decoded.channel_layout();
    if in_layout.is_empty() {
        in_layout = ffmpeg::ChannelLayout::default(decoded.channels() as i32);
    }
    let in_rate = decoded.rate();

    // Taux de sortie ajusté pour la vitesse de lecture.
    let needs_rebuild = !matches!(
        cache,
        Some(c) if c.in_format == in_format
            && c.in_layout == in_layout
            && c.in_rate == in_rate
            && c.speed_milli == speed_milli
    );
    if needs_rebuild {
        let out_rate = (device_rate as u64 * 1000 / speed_milli as u64).max(8000) as u32;
        let resampler = resampling::Context::get(
            in_format,
            in_layout,
            in_rate,
            ffmpeg::format::Sample::F32(SampleType::Packed),
            ffmpeg::ChannelLayout::STEREO,
            out_rate,
        )?;
        *cache = Some(ResamplerCache {
            resampler,
            in_format,
            in_layout,
            in_rate,
            speed_milli,
        });
    }
    let cache = cache.as_mut().expect("resampler initialisé ci-dessus");

    let mut resampled = ffmpeg::frame::Audio::empty();
    cache.resampler.run(decoded, &mut resampled)?;
    if resampled.samples() == 0 {
        return Ok(());
    }

    // f32 stéréo entrelacé : 2 valeurs par trame d'échantillonnage.
    let value_count = resampled.samples() * 2;
    let bytes = resampled.data(0);
    anyhow::ensure!(bytes.len() >= value_count * 4, "tampon audio trop court");
    // Sûr : le tampon AVFrame est aligné et contient `value_count` f32.
    let samples: &[f32] =
        unsafe { std::slice::from_raw_parts(bytes.as_ptr().cast::<f32>(), value_count) };

    let pts_us = decoded
        .timestamp()
        .or(decoded.pts())
        .map(|ts| ts_to_us(ts, time_base))
        .unwrap_or(-1);

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
    queue.push(pts_us, samples);
    Ok(())
}

#[allow(dead_code)]
/// Vérifie le type d'échantillon attendu par la file (documentation vivante).
const _: Sample = Sample::F32(SampleType::Packed);
