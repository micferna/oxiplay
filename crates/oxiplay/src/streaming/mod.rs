//! Prise en charge des sources réseau.
//!
//! Le gros du travail (HTTP/HTTPS, HLS, RTSP, MMS…) est effectué nativement
//! par libavformat ; ce module se charge de classifier les URL, de valider
//! les entrées utilisateur et de fournir les options de demuxage adaptées
//! à chaque protocole (transport RTSP, timeouts, reconnexion…).

use ffmpeg_the_third::Dictionary;

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
///
/// # Sécurité
///
/// Pour les sources **réseau**, un `protocol_whitelist` strict est imposé :
/// libavformat (et le demuxeur HLS en particulier) suit les URL imbriquées
/// d'un manifeste/playlist ; sans liste blanche, un `.m3u8` malveillant
/// pourrait référencer `file:`, `concat:`, `subfile:`, `data:`… et exfiltrer
/// des fichiers locaux ou abuser d'un protocole dangereux. On exclut donc
/// `file` de toute source distante (les fichiers locaux passent par
/// [`StreamKind::Local`], sans restriction, puisque l'utilisateur ouvre ses
/// propres fichiers). Voir SECURITY.md.
pub fn demux_options(kind: StreamKind) -> Dictionary {
    let mut opts = Dictionary::new();
    match kind {
        StreamKind::Local => {}
        StreamKind::Http | StreamKind::Hls => {
            // Liste blanche : pas de `file`, `concat`, `subfile`… depuis un
            // flux distant. `crypto`/`tls` couvrent HTTPS et HLS-AES.
            opts.set(
                "protocol_whitelist",
                "http,https,tcp,tls,crypto,data,httpproxy",
            );
            // Reconnexion automatique en cas de coupure réseau.
            opts.set("reconnect", "1");
            opts.set("reconnect_streamed", "1");
            opts.set("reconnect_delay_max", "5");
            // Timeout d'I/O (µs) pour ne pas bloquer indéfiniment.
            opts.set("rw_timeout", "15000000");
            opts.set("user_agent", concat!("oxiplay/", env!("CARGO_PKG_VERSION")));
        }
        StreamKind::Rtsp => {
            opts.set(
                "protocol_whitelist",
                "rtsp,rtps,rtp,udp,tcp,tls,crypto,http,https",
            );
            // TCP est beaucoup plus fiable que l'UDP par défaut derrière un NAT.
            opts.set("rtsp_transport", "tcp");
            opts.set("stimeout", "15000000");
        }
        StreamKind::Iptv => {
            // `http`/`https` autorisés pour les listes M3U distantes.
            opts.set("protocol_whitelist", "udp,rtp,tcp,http,https,tls,crypto");
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

    #[test]
    fn network_sources_forbid_file_protocol() {
        // Sécurité : aucune source distante ne doit autoriser `file:`
        // (sinon un manifeste piégé lirait des fichiers locaux).
        for kind in [
            StreamKind::Http,
            StreamKind::Hls,
            StreamKind::Rtsp,
            StreamKind::Iptv,
        ] {
            let opts = demux_options(kind);
            let whitelist = opts
                .get("protocol_whitelist")
                .unwrap_or_else(|| panic!("{kind:?} sans protocol_whitelist"));
            assert!(
                !whitelist.split(',').any(|p| p == "file"),
                "{kind:?} autorise file: ({whitelist})"
            );
            // Pas de protocoles dangereux d'enchaînement.
            for bad in ["concat", "subfile", "fd", "pipe"] {
                assert!(
                    !whitelist.split(',').any(|p| p == bad),
                    "{kind:?} autorise {bad}: ({whitelist})"
                );
            }
        }
    }

    #[test]
    fn local_source_is_unrestricted() {
        // L'utilisateur ouvre ses propres fichiers : pas de liste blanche.
        assert!(demux_options(StreamKind::Local)
            .get("protocol_whitelist")
            .is_none());
    }
}
