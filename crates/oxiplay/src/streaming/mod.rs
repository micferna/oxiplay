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
    /// Disque, dossier BDMV ou image `.iso` Blu-ray, lu via le protocole
    /// `bluray:` de libbluray (titre principal le plus long par défaut).
    BluRay,
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

/// Normalise une source en URL ouvrable par libavformat.
///
/// Les **dossiers BDMV** (un répertoire contenant `BDMV/`) et les **images
/// disque `.iso`** sont préfixés par le protocole `bluray:` de libbluray ;
/// tout le reste (fichiers, URL, sources déjà préfixées) est renvoyé tel quel.
///
/// Note : les disques Blu-ray **commerciaux chiffrés** (AACS, et a fortiori
/// l'AACS 2.0 des UHD 4K) nécessitent des clés que ce lecteur ne fournit pas —
/// seuls les disques/dossiers/images **non chiffrés** sont lisibles.
pub fn normalize_source(input: &str) -> String {
    if input.starts_with("bluray:") || is_url(input) {
        return input.to_string();
    }
    let lower = input.to_ascii_lowercase();
    let path = std::path::Path::new(input);
    let is_bdmv_dir = path.is_dir() && path.join("BDMV").is_dir();
    if lower.ends_with(".iso") || is_bdmv_dir {
        format!("bluray:{input}")
    } else {
        input.to_string()
    }
}

/// Classifie une source (chemin local, URL ou disque Blu-ray).
pub fn classify(input: &str) -> StreamKind {
    let lower = input.to_ascii_lowercase();
    if lower.starts_with("bluray:") {
        return StreamKind::BluRay;
    }
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

/// Distingue un **vrai flux HLS** (à lire tel quel par FFmpeg) d'un **annuaire
/// de chaînes** IPTV (à charger comme playlist). Les manifestes HLS portent des
/// balises `#EXT-X-*` (variantes, segments) qu'un annuaire — simple suite de
/// `#EXTINF` + URLs complètes — ne contient pas.
pub fn looks_like_hls(content: &str) -> bool {
    content.contains("#EXT-X-STREAM-INF")
        || content.contains("#EXT-X-TARGETDURATION")
        || content.contains("#EXT-X-MEDIA-SEQUENCE")
}

/// Récupère le contenu texte d'une URL de playlist (M3U/M3U8). Plafonné à
/// 32 Mio — certains annuaires IPTV pèsent plusieurs Mo, au-delà de la limite
/// par défaut de `into_string`, d'où la lecture via un reader plafonné.
pub fn fetch_text(url: &str) -> anyhow::Result<String> {
    use std::io::Read;
    let response = ureq::get(url)
        .set("User-Agent", concat!("oxiplay/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(15))
        .call()?;
    let mut buf = Vec::new();
    response
        .into_reader()
        .take(32 * 1024 * 1024)
        .read_to_end(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
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
        // Sources locales (fichier ou disque Blu-ray) : aucune option réseau.
        StreamKind::Local | StreamKind::BluRay => {}
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
    fn hls_stream_vs_channel_directory() {
        // Manifeste HLS (variantes) → vrai flux.
        let hls_master = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=800000\nlow.m3u8\n";
        assert!(looks_like_hls(hls_master));
        // Manifeste HLS (segments) → vrai flux.
        let hls_media = "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXTINF:6.0,\nseg0.ts\n";
        assert!(looks_like_hls(hls_media));
        // Annuaire IPTV (chaînes) → pas un flux, à charger en playlist.
        let directory = "#EXTM3U\n#EXTINF:-1 tvg-id=\"a\",Chaîne A\nhttps://ex.com/a.m3u8\n\
                         #EXTINF:-1,Chaîne B\nhttps://ex.com/b.m3u8\n";
        assert!(!looks_like_hls(directory));
    }

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

    #[test]
    fn bluray_sources() {
        assert_eq!(classify("bluray:/dev/sr0"), StreamKind::BluRay);
        assert_eq!(classify("BLURAY:/mnt/disc"), StreamKind::BluRay);
        // Une image .iso est routée vers le protocole bluray (casse préservée).
        assert_eq!(
            normalize_source("/films/disc.iso"),
            "bluray:/films/disc.iso"
        );
        assert_eq!(
            normalize_source("/films/DISQUE.ISO"),
            "bluray:/films/DISQUE.ISO"
        );
        // Sources déjà préfixées, URL ou fichier ordinaire : inchangées.
        assert_eq!(normalize_source("bluray:/dev/sr0"), "bluray:/dev/sr0");
        assert_eq!(
            normalize_source("https://ex.com/v.mp4"),
            "https://ex.com/v.mp4"
        );
        assert_eq!(normalize_source("/films/movie.mkv"), "/films/movie.mkv");
        // Le Blu-ray est local : pas de liste blanche réseau.
        assert!(demux_options(StreamKind::BluRay)
            .get("protocol_whitelist")
            .is_none());
    }
}
