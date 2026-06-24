//! Enregistrement d'un flux/média vers un fichier par **copie de flux** (remux
//! sans réencodage).
//!
//! L'approche est volontairement **isolée** du pipeline de lecture : on rouvre
//! la source dans un thread dédié et on recopie ses paquets vers un conteneur
//! de sortie. Aucune incidence sur le démux critique (zapping IPTV réactif).
//! Un `interrupt_callback` posé sur l'entrée permet d'arrêter immédiatement une
//! lecture réseau bloquante.

use anyhow::{Context, Result};
use ffmpeg_the_third as ffmpeg;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

/// Enregistreur actif : possède le thread de copie et expose l'avancement.
pub struct Recorder {
    stop: Arc<AtomicBool>,
    bytes: Arc<AtomicU64>,
    handle: Option<JoinHandle<()>>,
    /// Fichier de destination.
    pub path: PathBuf,
}

impl Recorder {
    /// Démarre l'enregistrement de `source` vers `path` (le conteneur est
    /// déduit de l'extension de `path`).
    pub fn start(source: String, path: PathBuf) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let bytes = Arc::new(AtomicU64::new(0));
        let handle = {
            let (stop, bytes, path) = (Arc::clone(&stop), Arc::clone(&bytes), path.clone());
            std::thread::Builder::new()
                .name("oxiplay-record".into())
                .spawn(move || {
                    if let Err(e) = record_loop(&source, &path, &stop, &bytes) {
                        log::warn!("enregistrement interrompu : {e}");
                    }
                })
                .ok()
        };
        Recorder {
            stop,
            bytes,
            handle,
            path,
        }
    }

    /// Octets de paquets recopiés jusqu'ici (≈ taille du fichier).
    pub fn bytes_written(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }

    /// Le thread de copie s'est-il arrêté de lui-même (fin de flux / erreur) ?
    pub fn finished(&self) -> bool {
        self.handle.as_ref().map_or(true, |h| h.is_finished())
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        // Demande l'arrêt (l'interrupt_callback débloque toute lecture réseau)
        // et attend la fin propre (write_trailer) avant de rendre la main.
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Extension de conteneur la mieux adaptée pour enregistrer `source` en copie :
/// `ts` pour le MPEG-TS/HLS (robuste aux discontinuités du direct), sinon le
/// conteneur d'origine s'il est connu, à défaut `mkv` (très permissif).
pub fn recording_extension(source: &str) -> &'static str {
    let s = source.to_ascii_lowercase();
    let s = s.split(['?', '#']).next().unwrap_or(&s);
    if s.contains(".m3u8") || s.ends_with(".ts") || s.contains(".m3u") {
        "ts"
    } else if s.ends_with(".mp4") || s.ends_with(".m4v") || s.ends_with(".mov") {
        "mp4"
    } else if s.ends_with(".webm") {
        "webm"
    } else {
        "mkv"
    }
}

/// Callback d'interruption d'I/O : renvoie 1 (abandon) dès que l'arrêt est
/// demandé, pour débloquer une lecture réseau en cours.
unsafe extern "C" fn interrupt_cb(opaque: *mut std::ffi::c_void) -> std::os::raw::c_int {
    if opaque.is_null() {
        return 0;
    }
    // SAFETY : `opaque` pointe vers l'`AtomicBool` d'arrêt, maintenu en vie par
    // le thread d'enregistrement tant que le contexte d'entrée existe.
    let stop = unsafe { &*(opaque as *const AtomicBool) };
    i32::from(stop.load(Ordering::Relaxed))
}

/// Ouvre la source avec un `interrupt_callback` posé avant l'ouverture (arrêt
/// immédiat possible), puis analyse ses flux.
fn open_input_interruptible(
    source: &str,
    stop: &Arc<AtomicBool>,
) -> Result<ffmpeg::format::context::Input> {
    use ffmpeg::ffi;
    unsafe {
        let mut ps = ffi::avformat_alloc_context();
        anyhow::ensure!(!ps.is_null(), "allocation du contexte de format échouée");
        (*ps).interrupt_callback.callback = Some(interrupt_cb);
        (*ps).interrupt_callback.opaque = Arc::as_ptr(stop) as *mut std::ffi::c_void;

        let path = std::ffi::CString::new(source)?;
        let res = ffi::avformat_open_input(
            &mut ps,
            path.as_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
        anyhow::ensure!(res >= 0, "ouverture de la source échouée ({res})");
        if ffi::avformat_find_stream_info(ps, std::ptr::null_mut()) < 0 {
            ffi::avformat_close_input(&mut ps);
            anyhow::bail!("analyse du flux échouée");
        }
        Ok(ffmpeg::format::context::Input::wrap(ps))
    }
}

/// Boucle de copie : ouvre l'entrée et la sortie, recopie les paquets des flux
/// audio/vidéo/sous-titres jusqu'à l'arrêt demandé ou la fin du flux.
fn record_loop(
    source: &str,
    path: &Path,
    stop: &Arc<AtomicBool>,
    bytes: &Arc<AtomicU64>,
) -> Result<()> {
    use ffmpeg::media::Type;

    let normalized = crate::streaming::normalize_source(source);
    let mut ictx = open_input_interruptible(&normalized, stop)?;
    let mut octx = ffmpeg::format::output(path)
        .with_context(|| format!("création du conteneur de sortie {}", path.display()))?;

    // Mappe chaque flux d'entrée audio/vidéo/sous-titre vers un flux de sortie
    // dont les paramètres de codec sont copiés tels quels (pas de réencodage).
    let mut mapping = vec![-1i32; ictx.nb_streams() as usize];
    let mut ost_index = 0i32;
    for ist in ictx.streams() {
        let medium = ist.parameters().medium();
        if !matches!(medium, Type::Video | Type::Audio | Type::Subtitle) {
            continue;
        }
        mapping[ist.index()] = ost_index;
        ost_index += 1;
        let mut ost = octx
            .add_stream(ffmpeg::codec::Id::None)
            .context("ajout d'un flux de sortie")?;
        ost.set_parameters(ist.parameters());
        // Le tag de codec d'entrée n'est pas forcément valide dans le conteneur
        // de sortie : on le remet à 0 pour laisser le muxeur le déterminer.
        // SAFETY : `as_mut_ptr` renvoie l'AVCodecParameters du flux, valide.
        unsafe {
            (*ost.parameters_mut().as_mut_ptr()).codec_tag = 0;
        }
    }
    anyhow::ensure!(ost_index > 0, "aucun flux exploitable à enregistrer");

    octx.write_header()
        .context("écriture de l'en-tête du conteneur")?;

    // Time bases de sortie (figées après write_header), pour le rééchelonnage.
    let out_tb: Vec<ffmpeg::Rational> = (0..ost_index as usize)
        .map(|i| {
            octx.stream(i)
                .map(|s| s.time_base())
                .unwrap_or(ffmpeg::Rational::new(1, 1000))
        })
        .collect();

    while !stop.load(Ordering::Relaxed) {
        let Some(item) = ictx.packets().next() else {
            break; // fin du flux
        };
        let (stream, mut packet) = match item {
            Ok(sp) => sp,
            Err(e) => {
                log::debug!("paquet illisible pendant l'enregistrement : {e}");
                continue;
            }
        };
        let isi = stream.index();
        let osi = mapping.get(isi).copied().unwrap_or(-1);
        if osi < 0 {
            continue;
        }
        let src_tb = stream.time_base();
        packet.rescale_ts(src_tb, out_tb[osi as usize]);
        packet.set_position(-1);
        packet.set_stream(osi as usize);
        bytes.fetch_add(packet.size() as u64, Ordering::Relaxed);
        if let Err(e) = packet.write_interleaved(&mut octx) {
            log::warn!("écriture d'un paquet échouée : {e}");
            break;
        }
    }

    octx.write_trailer().context("finalisation du conteneur")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::recording_extension;

    #[test]
    fn extension_picks_ts_for_hls_and_mkv_default() {
        assert_eq!(recording_extension("http://x/live.m3u8"), "ts");
        assert_eq!(recording_extension("http://x/stream.ts"), "ts");
        assert_eq!(recording_extension("http://x/movie.mp4?token=1"), "mp4");
        assert_eq!(recording_extension("http://x/clip.webm"), "webm");
        assert_eq!(recording_extension("rtsp://x/stream"), "mkv");
    }
}
