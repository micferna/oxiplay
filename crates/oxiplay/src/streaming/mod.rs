//! Prise en charge des sources réseau.
//!
//! Le gros du travail (HTTP/HTTPS, HLS, RTSP, MMS…) est effectué nativement
//! par libavformat ; ce module se charge de classifier les URL, de valider
//! les entrées utilisateur et de fournir les options de demuxage adaptées
//! à chaque protocole (transport RTSP, timeouts, reconnexion…).

use ffmpeg_next::Dictionary;

/// Type de source réseau reconnu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamKind {
    /// Fichier local.
    Local,
    /// Flux progressif HTTP/HTTPS (fichier distant).
    Http,
    /// Playlist HLS (`.m3u8`).
    Hls,
    /// Flux RTSP (caméras, serveurs de streaming).
    Rtsp,
    /// Flux IPTV (UDP/RTP multicast ou listes M3U distantes).
    Iptv,
}

/// Retourne `true` si la chaîne ressemble à une URL plutôt qu'à un chemin local.
pub fn is_url(input: &str) -> bool {
    let lower = input.to_ascii_lowercase();
    [
        "http://", "https://", "rtsp://", "rtmp://", "udp://", "rtp://", "mms://",
    ]
    .iter()
    .any(|p| lower.starts_with(p))
}

/// Classifie une source (chemin local ou URL).
pub fn classify(input: &str) -> StreamKind {
    let lower = input.to_ascii_lowercase();
    if !is_url(&lower) {
        return StreamKind::Local;
    }
    if lower.starts_with("rtsp://") {
        StreamKind::Rtsp
    } else if lower.starts_with("udp://") || lower.starts_with("rtp://") {
        StreamKind::Iptv
    } else if lower.contains(".m3u8") {
        StreamKind::Hls
    } else if lower.ends_with(".m3u") {
        StreamKind::Iptv
    } else {
        StreamKind::Http
    }
}

/// Options libavformat recommandées pour une source donnée.
///
/// Ces options sont passées à `avformat_open_input` ; elles améliorent
/// nettement la robustesse sur les flux réseau instables.
pub fn demux_options(kind: StreamKind) -> Dictionary<'static> {
    let mut opts = Dictionary::new();
    match kind {
        StreamKind::Local => {}
        StreamKind::Http | StreamKind::Hls => {
            // Reconnexion automatique en cas de coupure réseau.
            opts.set("reconnect", "1");
            opts.set("reconnect_streamed", "1");
            opts.set("reconnect_delay_max", "5");
            // Timeout d'I/O (µs) pour ne pas bloquer indéfiniment.
            opts.set("rw_timeout", "15000000");
            opts.set("user_agent", concat!("oxiplay/", env!("CARGO_PKG_VERSION")));
        }
        StreamKind::Rtsp => {
            // TCP est beaucoup plus fiable que l'UDP par défaut derrière un NAT.
            opts.set("rtsp_transport", "tcp");
            opts.set("stimeout", "15000000");
        }
        StreamKind::Iptv => {
            // Tolérance aux pertes de paquets multicast.
            opts.set("fifo_size", "1000000");
            opts.set("overrun_nonfatal", "1");
        }
    }
    opts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_sources() {
        assert_eq!(classify("/home/u/film.mkv"), StreamKind::Local);
        assert_eq!(classify("C:\\Videos\\film.mp4"), StreamKind::Local);
        assert_eq!(classify("https://ex.com/video.mp4"), StreamKind::Http);
        assert_eq!(
            classify("https://ex.com/live/master.m3u8?tok=1"),
            StreamKind::Hls
        );
        assert_eq!(classify("rtsp://10.0.0.2:554/cam"), StreamKind::Rtsp);
        assert_eq!(classify("udp://239.0.0.1:1234"), StreamKind::Iptv);
    }

    #[test]
    fn url_detection() {
        assert!(is_url("HTTP://EX.COM/a"));
        assert!(!is_url("relative/path.mp4"));
    }
}
