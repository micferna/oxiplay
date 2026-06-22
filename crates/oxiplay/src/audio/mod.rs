//! Sortie audio multiplateforme via cpal (ALSA, WASAPI, CoreAudio).
//!
//! Le thread de décodage audio pousse des échantillons stéréo `f32`
//! (déjà rééchantillonnés à la fréquence du périphérique) dans une
//! [`AudioQueue`] ; le callback temps réel de cpal les consomme, applique
//! le volume, et resynchronise l'horloge maîtresse sur le PTS réellement
//! joué — l'audio est la référence de synchronisation A/V.

use crate::player::state::SharedState;
use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

/// File d'échantillons stéréo entrelacés, horodatée.
#[derive(Default)]
struct QueueInner {
    /// Échantillons stéréo entrelacés (L, R, L, R…).
    samples: VecDeque<f32>,
    /// PTS média (µs) de l'échantillon en tête de file.
    front_pts_us: i64,
}

/// File partagée producteur (décodeur) / consommateur (callback cpal).
pub struct AudioQueue {
    inner: Mutex<QueueInner>,
    /// Capacité maximale en échantillons (≈ 1 s de stéréo).
    capacity: usize,
}

impl AudioQueue {
    fn new(sample_rate: u32) -> Self {
        Self {
            inner: Mutex::new(QueueInner::default()),
            capacity: sample_rate as usize * 2,
        }
    }

    /// Vrai si la file a encore de la place pour un lot d'échantillons.
    pub fn has_room(&self) -> bool {
        self.inner.lock().unwrap().samples.len() < self.capacity
    }

    pub fn is_empty(&self) -> bool {
        self.inner.lock().unwrap().samples.is_empty()
    }

    /// Pousse un lot d'échantillons stéréo horodatés.
    pub fn push(&self, pts_us: i64, samples: &[f32]) {
        let mut inner = self.inner.lock().unwrap();
        if inner.samples.is_empty() && pts_us >= 0 {
            inner.front_pts_us = pts_us;
        }
        inner.samples.extend(samples.iter().copied());
    }

    /// Vide la file (seek ou changement de média).
    pub fn clear(&self) {
        self.inner.lock().unwrap().samples.clear();
    }
}

/// Sortie audio persistante de l'application. Construite une seule fois ;
/// chaque session de lecture s'y « branche » via [`AudioOutput::attach`].
pub struct AudioOutput {
    _stream: cpal::Stream,
    queue: Arc<AudioQueue>,
    /// Session active dont le callback lit volume/vitesse/pause.
    session: Arc<Mutex<Option<Arc<SharedState>>>>,
    sample_rate: u32,
}

impl AudioOutput {
    /// Liste les noms des périphériques de sortie disponibles (vide si aucun
    /// ou en cas d'erreur d'énumération).
    pub fn list_output_devices() -> Vec<String> {
        cpal::default_host()
            .output_devices()
            .map(|devices| {
                devices
                    .filter_map(|d| d.description().ok().map(|desc| desc.name().to_string()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Ouvre le périphérique de sortie par défaut.
    pub fn new() -> Result<Self> {
        Self::new_with_device(None)
    }

    /// Ouvre un périphérique de sortie par son nom ; repli sur le périphérique
    /// par défaut si le nom est inconnu ou absent.
    pub fn new_with_device(name: Option<&str>) -> Result<Self> {
        let host = cpal::default_host();
        let device = match name {
            Some(wanted) => host
                .output_devices()
                .ok()
                .and_then(|mut devices| {
                    devices.find(|d| {
                        d.description()
                            .map(|desc| desc.name() == wanted)
                            .unwrap_or(false)
                    })
                })
                .or_else(|| host.default_output_device())
                .context("aucun périphérique de sortie audio")?,
            None => host
                .default_output_device()
                .context("aucun périphérique de sortie audio")?,
        };
        let config = device
            .default_output_config()
            .context("configuration audio par défaut indisponible")?;
        let sample_rate = config.sample_rate();
        let channels = config.channels();
        let queue = Arc::new(AudioQueue::new(sample_rate));
        let session: Arc<Mutex<Option<Arc<SharedState>>>> = Arc::new(Mutex::new(None));

        let stream = match config.sample_format() {
            cpal::SampleFormat::F32 => Self::build_stream::<f32>(
                &device,
                config.into(),
                channels,
                sample_rate,
                &queue,
                &session,
            )?,
            cpal::SampleFormat::I16 => Self::build_stream::<i16>(
                &device,
                config.into(),
                channels,
                sample_rate,
                &queue,
                &session,
            )?,
            cpal::SampleFormat::U16 => Self::build_stream::<u16>(
                &device,
                config.into(),
                channels,
                sample_rate,
                &queue,
                &session,
            )?,
            other => anyhow::bail!("format d'échantillon non géré : {other:?}"),
        };
        stream.play().context("démarrage du flux audio")?;
        log::info!("sortie audio : {sample_rate} Hz, {channels} canaux");
        Ok(Self {
            _stream: stream,
            queue,
            session,
            sample_rate,
        })
    }

    fn build_stream<T>(
        device: &cpal::Device,
        config: cpal::StreamConfig,
        channels: u16,
        sample_rate: u32,
        queue: &Arc<AudioQueue>,
        session: &Arc<Mutex<Option<Arc<SharedState>>>>,
    ) -> Result<cpal::Stream>
    where
        T: cpal::SizedSample + cpal::FromSample<f32>,
    {
        let queue = Arc::clone(queue);
        let session = Arc::clone(session);
        let channels = channels as usize;
        let stream = device
            .build_output_stream(
                config,
                move |data: &mut [T], _| {
                    render_audio(data, channels, sample_rate, &queue, &session);
                },
                |err| log::error!("erreur du flux audio : {err}"),
                None,
            )
            .context("création du flux de sortie audio")?;
        Ok(stream)
    }

    /// Fréquence d'échantillonnage du périphérique (cible du resampler).
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// File à remplir par le thread de décodage audio.
    pub fn queue(&self) -> Arc<AudioQueue> {
        Arc::clone(&self.queue)
    }

    /// Branche une session de lecture sur la sortie.
    pub fn attach(&self, shared: Arc<SharedState>) {
        self.queue.clear();
        *self.session.lock().unwrap() = Some(shared);
    }

    /// Débranche la session courante (arrêt de la lecture).
    pub fn detach(&self) {
        *self.session.lock().unwrap() = None;
        self.queue.clear();
    }
}

/// Cœur du callback temps réel : copie les échantillons vers le
/// périphérique, applique le volume et met à jour l'horloge.
fn render_audio<T>(
    data: &mut [T],
    channels: usize,
    sample_rate: u32,
    queue: &AudioQueue,
    session: &Mutex<Option<Arc<SharedState>>>,
) where
    T: cpal::SizedSample + cpal::FromSample<f32>,
{
    let silence = T::from_sample(0.0f32);
    let shared = session.lock().unwrap().clone();
    let Some(shared) = shared else {
        data.fill(silence);
        return;
    };
    if shared.clock.is_paused() || shared.should_stop() {
        data.fill(silence);
        return;
    }

    let volume = shared.effective_volume();
    let speed = shared.speed();
    // Décalage de synchronisation A/V : décale le PTS rapporté à l'horloge
    // maîtresse, donc la vidéo par rapport à l'audio réellement entendu.
    let audio_delay = shared.audio_delay_us.load(Ordering::Relaxed);
    let mut inner = queue.inner.lock().unwrap();
    let mut consumed_frames = 0usize;

    for frame in data.chunks_mut(channels) {
        if inner.samples.len() < 2 {
            frame.fill(silence);
            continue;
        }
        let l = inner.samples.pop_front().unwrap_or(0.0) * volume;
        let r = inner.samples.pop_front().unwrap_or(0.0) * volume;
        consumed_frames += 1;
        // Mappe la stéréo interne vers le nombre de canaux du périphérique.
        match channels {
            1 => frame[0] = T::from_sample((l + r) * 0.5),
            _ => {
                frame[0] = T::from_sample(l);
                frame[1] = T::from_sample(r);
                for c in frame.iter_mut().skip(2) {
                    *c = silence;
                }
            }
        }
    }

    if consumed_frames > 0 {
        // Le flux a été rééchantillonné d'un facteur 1/vitesse : chaque
        // échantillon joué fait avancer le temps média de `vitesse`
        // périodes d'échantillonnage.
        let advance_us = (consumed_frames as f64 * 1_000_000.0 * speed / sample_rate as f64) as i64;
        inner.front_pts_us += advance_us;
        let pts = inner.front_pts_us;
        drop(inner);
        // L'audio pilote l'horloge maîtresse (avec le décalage A/V utilisateur).
        shared.clock.sync_to(pts + audio_delay);
    }
}
