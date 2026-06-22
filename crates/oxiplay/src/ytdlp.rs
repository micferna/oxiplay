//! Résolution d'URL via **yt-dlp** : transforme une page web (YouTube,
//! Vimeo, Twitch…) en URL de flux directe jouable par libavformat.
//!
//! Best effort : si yt-dlp est **absent** du système ou **échoue**, l'URL
//! d'origine est conservée (aucune régression). La résolution se fait dans le
//! thread de demuxage (jamais dans l'UI).

use std::process::Command;

/// Extensions de média/playlist directs : ces URL sont jouées telles quelles,
/// sans passer par yt-dlp.
const DIRECT_EXTENSIONS: &[&str] = &[
    ".mp4", ".mkv", ".webm", ".avi", ".mov", ".flv", ".ts", ".m2ts", ".wmv", ".mp3", ".flac",
    ".aac", ".m4a", ".ogg", ".oga", ".opus", ".wav", ".wma", ".m3u8", ".mpd",
];

/// Vrai si l'URL devrait passer par yt-dlp : une page HTTP(S) sans extension
/// média reconnue (un fichier ou une playlist directe est joué directement).
pub fn should_resolve(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        return false;
    }
    // On regarde le chemin, avant l'éventuelle query string / fragment.
    let path = lower.split(['?', '#']).next().unwrap_or(&lower);
    !DIRECT_EXTENSIONS.iter().any(|ext| path.ends_with(ext))
}

/// Résout une URL de page en URL de flux directe via `yt-dlp -g`. Renvoie
/// `None` si yt-dlp est absent, échoue, ou ne produit rien — l'appelant
/// conserve alors l'URL d'origine.
pub fn resolve(url: &str) -> Option<String> {
    let output = Command::new("yt-dlp")
        // `-f best` force un flux combiné unique (une seule URL), jouable
        // directement ; `-g` imprime l'URL résolue sur stdout.
        .args(["-g", "--no-playlist", "-f", "best", url])
        .output()
        .ok()?;
    if !output.status.success() {
        log::debug!("yt-dlp n'a pas résolu {url}");
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_urls() {
        // Pages web → yt-dlp.
        assert!(should_resolve("https://www.youtube.com/watch?v=abc"));
        assert!(should_resolve("https://vimeo.com/123456"));
        assert!(should_resolve("http://twitch.tv/somestream"));
        // Fichiers / playlists directs → lecture directe.
        assert!(!should_resolve("https://ex.com/video.mp4"));
        assert!(!should_resolve("https://ex.com/live/master.m3u8?token=1"));
        assert!(!should_resolve("https://ex.com/audio.opus"));
        // Sources non HTTP → jamais yt-dlp.
        assert!(!should_resolve("/home/u/film.mkv"));
        assert!(!should_resolve("rtsp://10.0.0.2/cam"));
        assert!(!should_resolve("bluray:/dev/sr0"));
    }
}
