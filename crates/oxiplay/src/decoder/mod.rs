//! Pipeline de décodage multimédia, bâti sur FFmpeg (`ffmpeg-next`).
//!
//! Architecture en threads dédiés, communiquant par canaux bornés
//! (contre-pression naturelle) :
//!
//! ```text
//!            ┌────────────┐  PacketMsg   ┌───────────────┐  VideoFrameMsg
//!  fichier → │   demux    │ ───────────► │ décodage vidéo │ ─────────────► présentation
//!  ou URL    │  (thread)  │              └───────────────┘                  (player)
//!            │            │  PacketMsg   ┌───────────────┐   échantillons
//!            │            │ ───────────► │ décodage audio │ ─────────────► AudioQueue
//!            └────────────┘              └───────────────┘                  (cpal)
//! ```
//!
//! Les sous-titres embarqués sont décodés directement dans le thread de
//! demuxage (coût négligeable) et publiés dans l'état partagé.
//!
//! Chaque seek incrémente une **génération** ; tout paquet ou image d'une
//! génération périmée est jeté sans être décodé ni présenté, ce qui rend
//! les seeks réactifs même pipeline plein.

mod audio;
mod audio_filter;
mod demux;
mod hwaccel;
mod video;

pub use audio::run_audio_decoder;
pub use audio_filter::EQ_FREQUENCIES;
pub use demux::{run_demux, DemuxConfig};
pub use video::run_video_decoder;

use crate::video::VideoFrameData;
use ffmpeg_the_third as ffmpeg;
use std::sync::Arc;

/// Commandes acceptées par le thread de demuxage.
#[derive(Debug)]
pub enum DemuxCommand {
    /// Aller à la position absolue (µs).
    Seek(i64),
    /// Sélectionner une autre piste audio (index de flux).
    SelectAudioTrack(usize),
    /// Sélectionner une piste de sous-titres embarquée (None = désactiver).
    SelectSubtitleTrack(Option<usize>),
}

/// Messages envoyés aux threads de décodage.
pub enum PacketMsg {
    /// Un paquet compressé à décoder.
    Packet {
        packet: ffmpeg::Packet,
        time_base: ffmpeg::Rational,
        generation: u64,
    },
    /// (Re)configuration du décodeur — premier message, ou changement de piste.
    Reconfigure {
        parameters: ffmpeg::codec::Parameters,
        time_base: ffmpeg::Rational,
    },
    /// Vider le décodeur après un seek.
    Flush,
    /// Fin du flux : drainer le décodeur.
    Eof,
}

/// Messages produits par le décodeur vidéo vers le thread de présentation.
pub enum VideoFrameMsg {
    Frame {
        frame: Arc<VideoFrameData>,
        generation: u64,
    },
    Eof,
}

/// Convertit un horodatage exprimé dans `time_base` en microsecondes.
pub(crate) fn ts_to_us(ts: i64, time_base: ffmpeg::Rational) -> i64 {
    let num = time_base.numerator() as i64;
    let den = time_base.denominator() as i64;
    if den == 0 {
        return 0;
    }
    // ts * num/den secondes → µs ; en i128 pour éviter tout débordement.
    ((ts as i128 * num as i128 * 1_000_000) / den as i128) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ts_conversion() {
        // time_base 1/1000 (Matroska) : 1 tick = 1 ms.
        assert_eq!(ts_to_us(1500, ffmpeg::Rational::new(1, 1000)), 1_500_000);
        // time_base 1/90000 (MPEG-TS).
        assert_eq!(ts_to_us(90000, ffmpeg::Rational::new(1, 90000)), 1_000_000);
        // Dénominateur nul : pas de panique.
        assert_eq!(ts_to_us(42, ffmpeg::Rational::new(1, 0)), 0);
    }
}
