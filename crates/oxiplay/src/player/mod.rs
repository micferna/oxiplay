//! Moteur de lecture : orchestre les threads de demuxage, de décodage et
//! de présentation, et expose une API de contrôle simple à l'application.
//!
//! Le moteur est **indépendant de l'interface** : les images prêtes à
//! afficher sont livrées via un callback (`FrameSink`), ce qui permet de
//! le tester sans UI et de changer de toolkit sans toucher au cœur.

pub mod clock;
pub mod state;

use crate::audio::AudioQueue;
use crate::decoder::{run_demux, DemuxCommand, DemuxConfig, PacketMsg, VideoFrameMsg};
use crate::video::VideoFrameData;
use crossbeam_channel::{bounded, Receiver, RecvTimeoutError, Sender};
use state::SharedState;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

/// Callback de livraison des images au thread d'interface.
pub type FrameSink = Box<dyn Fn(Arc<VideoFrameData>) + Send + 'static>;

/// Tailles des canaux du pipeline (contre-pression).
const VIDEO_PACKET_QUEUE: usize = 60;
const AUDIO_PACKET_QUEUE: usize = 120;
const VIDEO_FRAME_QUEUE: usize = 6;

/// Sortie audio mise à disposition du moteur par l'application.
pub struct AudioSink {
    pub queue: Arc<AudioQueue>,
    pub sample_rate: u32,
}

/// Une session de lecture : un média ouvert et ses threads.
///
/// La session s'arrête proprement quand la valeur est *droppée*.
pub struct PlayerEngine {
    pub shared: Arc<SharedState>,
    cmd_tx: Sender<DemuxCommand>,
    threads: Vec<JoinHandle<()>>,
}

impl PlayerEngine {
    /// Ouvre un média (fichier ou URL) et démarre la lecture.
    ///
    /// * `audio` — sortie audio de l'application (None : lecture muette).
    /// * `frame_sink` — callback recevant les images décodées.
    /// * `start_at_us` — position de reprise éventuelle.
    pub fn open(
        source: &str,
        audio: Option<AudioSink>,
        frame_sink: FrameSink,
        start_at_us: Option<i64>,
    ) -> Self {
        let shared = Arc::new(SharedState::default());
        let (cmd_tx, cmd_rx) = bounded::<DemuxCommand>(32);
        let (video_pkt_tx, video_pkt_rx) = bounded::<PacketMsg>(VIDEO_PACKET_QUEUE);
        let (audio_pkt_tx, audio_pkt_rx) = bounded::<PacketMsg>(AUDIO_PACKET_QUEUE);
        let (frame_tx, frame_rx) = bounded::<VideoFrameMsg>(VIDEO_FRAME_QUEUE);

        let mut threads = Vec::new();

        // Thread de demuxage.
        let demux_shared = Arc::clone(&shared);
        let demux_queue = audio.as_ref().map(|a| Arc::clone(&a.queue));
        let config = DemuxConfig {
            source: source.to_string(),
            start_at_us,
            audio_enabled: audio.is_some(),
        };
        threads.push(
            std::thread::Builder::new()
                .name("oxiplay-demux".into())
                .spawn(move || {
                    run_demux(
                        config,
                        demux_shared,
                        cmd_rx,
                        video_pkt_tx,
                        audio_pkt_tx,
                        demux_queue,
                    )
                })
                .expect("spawn demux"),
        );

        // Thread de décodage vidéo.
        let video_shared = Arc::clone(&shared);
        threads.push(
            std::thread::Builder::new()
                .name("oxiplay-vdec".into())
                .spawn(move || {
                    crate::decoder::run_video_decoder(video_shared, video_pkt_rx, frame_tx)
                })
                .expect("spawn vdec"),
        );

        // Thread de décodage audio (si un périphérique existe).
        if let Some(sink) = audio {
            let audio_shared = Arc::clone(&shared);
            threads.push(
                std::thread::Builder::new()
                    .name("oxiplay-adec".into())
                    .spawn(move || {
                        crate::decoder::run_audio_decoder(
                            audio_shared,
                            audio_pkt_rx,
                            sink.queue,
                            sink.sample_rate,
                        )
                    })
                    .expect("spawn adec"),
            );
        } else {
            // Personne ne consommera ce canal : on le draine pour ne pas
            // bloquer le demuxeur.
            let drain_shared = Arc::clone(&shared);
            threads.push(
                std::thread::Builder::new()
                    .name("oxiplay-adrain".into())
                    .spawn(move || {
                        while !drain_shared.should_stop() {
                            match audio_pkt_rx.recv_timeout(Duration::from_millis(100)) {
                                Ok(_) => {}
                                Err(RecvTimeoutError::Timeout) => {}
                                Err(RecvTimeoutError::Disconnected) => break,
                            }
                        }
                    })
                    .expect("spawn adrain"),
            );
        }

        // Thread de présentation vidéo.
        let present_shared = Arc::clone(&shared);
        threads.push(
            std::thread::Builder::new()
                .name("oxiplay-present".into())
                .spawn(move || run_presenter(present_shared, frame_rx, frame_sink))
                .expect("spawn present"),
        );

        // La lecture démarre immédiatement.
        shared.clock.set_paused(false);

        Self {
            shared,
            cmd_tx,
            threads,
        }
    }

    // ---- Contrôles de lecture -------------------------------------------

    pub fn is_paused(&self) -> bool {
        self.shared.clock.is_paused()
    }

    pub fn set_paused(&self, paused: bool) {
        self.shared.clock.set_paused(paused);
    }

    pub fn toggle_pause(&self) {
        self.set_paused(!self.is_paused());
    }

    /// Position courante (µs).
    pub fn position_us(&self) -> i64 {
        self.shared.clock.now_us().max(0)
    }

    /// Durée totale (µs), 0 si inconnue.
    pub fn duration_us(&self) -> i64 {
        self.shared.duration_us.load(Ordering::Relaxed)
    }

    /// Seek absolu (µs).
    pub fn seek(&self, target_us: i64) {
        let _ = self.cmd_tx.try_send(DemuxCommand::Seek(target_us.max(0)));
    }

    /// Seek relatif (avance/retour rapide), en secondes.
    pub fn seek_relative(&self, delta_secs: f64) {
        let target = self.position_us() + (delta_secs * 1_000_000.0) as i64;
        self.seek(target);
    }

    /// Vitesse de lecture (0.25 à 4.0).
    pub fn set_speed(&self, speed: f64) {
        self.shared.set_speed(speed);
    }

    /// Volume 0.0 à 1.25.
    pub fn set_volume(&self, volume: f32) {
        self.shared
            .volume_milli
            .store((volume.clamp(0.0, 1.25) * 1000.0) as u32, Ordering::Relaxed);
    }

    pub fn set_muted(&self, muted: bool) {
        self.shared.muted.store(muted, Ordering::Relaxed);
    }

    /// Sélectionne une piste audio par index de flux.
    pub fn select_audio_track(&self, stream_index: usize) {
        let _ = self
            .cmd_tx
            .try_send(DemuxCommand::SelectAudioTrack(stream_index));
    }

    /// Sélectionne une piste de sous-titres embarquée (None = aucune).
    pub fn select_subtitle_track(&self, stream_index: Option<usize>) {
        let _ = self
            .cmd_tx
            .try_send(DemuxCommand::SelectSubtitleTrack(stream_index));
    }

    /// Décalage des sous-titres en secondes (positif = sous-titres retardés).
    pub fn set_subtitle_delay(&self, delay_secs: f64) {
        self.shared
            .subtitle_delay_us
            .store((delay_secs * 1_000_000.0) as i64, Ordering::Relaxed);
    }
}

impl Drop for PlayerEngine {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        for handle in self.threads.drain(..) {
            let _ = handle.join();
        }
    }
}

/// Tolérance avant d'estimer une image « en retard » et de la sauter.
const LATE_DROP_US: i64 = 120_000;
/// Avance maximale tolérée avant présentation immédiate.
const EARLY_SHOW_US: i64 = 2_000;

/// Thread de présentation : cadence l'affichage des images décodées sur
/// l'horloge maîtresse, saute les images en retard, et gère l'affichage
/// immédiat de la première image après un seek en pause.
fn run_presenter(
    shared: Arc<SharedState>,
    frame_rx: Receiver<VideoFrameMsg>,
    frame_sink: FrameSink,
) {
    let mut pending: Option<(Arc<VideoFrameData>, u64)> = None;
    let mut last_generation = shared.current_generation();
    let mut first_of_generation = true;

    loop {
        if shared.should_stop() {
            return;
        }

        // Récupère une image si nécessaire.
        if pending.is_none() {
            match frame_rx.recv_timeout(Duration::from_millis(50)) {
                Ok(VideoFrameMsg::Frame { frame, generation }) => {
                    pending = Some((frame, generation));
                }
                Ok(VideoFrameMsg::Eof) => {
                    shared.video_done.store(true, Ordering::Relaxed);
                    continue;
                }
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }

        let (frame, generation) = pending.take().expect("image en attente");

        // Image périmée par un seek : on la jette immédiatement (même en
        // pause), ce qui draine le pipeline et débloque la contre-pression.
        if generation != shared.current_generation() {
            continue;
        }
        if generation != last_generation {
            last_generation = generation;
            first_of_generation = true;
        }

        let paused = shared.clock.is_paused();
        if paused {
            if first_of_generation {
                // Aperçu immédiat après un seek en pause.
                present(&shared, &frame, &frame_sink);
                first_of_generation = false;
            } else {
                // On garde l'image et on attend la reprise (ou un seek).
                pending = Some((frame, generation));
                std::thread::sleep(Duration::from_millis(20));
            }
            continue;
        }

        let now = shared.clock.now_us();
        let delta = frame.pts_us - now;
        if delta > EARLY_SHOW_US {
            // Trop tôt : dort (en temps mural) puis réévalue.
            let speed = shared.speed().max(0.25);
            let sleep_us = ((delta as f64 / speed) as i64).clamp(500, 20_000);
            pending = Some((frame, generation));
            std::thread::sleep(Duration::from_micros(sleep_us as u64));
            continue;
        }
        if delta < -LATE_DROP_US && !first_of_generation {
            // Trop tard : image sautée pour rattraper l'horloge.
            log::trace!("image en retard de {} ms, sautée", -delta / 1000);
            continue;
        }

        present(&shared, &frame, &frame_sink);
        first_of_generation = false;
    }
}

/// Présente une image : mémorise pour la capture d'écran puis livre à l'UI.
fn present(shared: &Arc<SharedState>, frame: &Arc<VideoFrameData>, sink: &FrameSink) {
    *shared.last_frame.lock().unwrap() = Some(Arc::clone(frame));
    // Sans audio, la vidéo ancre elle-même l'horloge au premier affichage.
    if !shared.clock.started() {
        shared.clock.set_position(frame.pts_us);
    }
    sink(Arc::clone(frame));
}
