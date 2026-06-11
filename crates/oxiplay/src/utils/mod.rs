//! Petites fonctions utilitaires partagées par tous les modules.

/// Formate une durée en microsecondes vers `H:MM:SS` (ou `M:SS` si < 1 h).
pub fn format_time(us: i64) -> String {
    let total_secs = (us.max(0)) / 1_000_000;
    let h = total_secs / 3600;
    let m = (total_secs % 3600) / 60;
    let s = total_secs % 60;
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// Extrait un nom lisible depuis un chemin de fichier ou une URL.
pub fn display_name(path: &str) -> String {
    if crate::streaming::is_url(path) {
        // Pour une URL, on garde le dernier segment significatif.
        let trimmed = path.trim_end_matches('/');
        trimmed
            .rsplit('/')
            .next()
            .filter(|s| !s.is_empty() && !s.contains("://"))
            .unwrap_or(trimmed)
            .to_string()
    } else {
        std::path::Path::new(path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string())
    }
}

/// Horodatage `AAAAMMJJ-HHMMSS`-approché (sans dépendance chrono) pour nommer
/// les captures d'écran de manière unique.
pub fn timestamp_slug() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{secs}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_time_basic() {
        assert_eq!(format_time(0), "0:00");
        assert_eq!(format_time(59 * 1_000_000), "0:59");
        assert_eq!(format_time(61 * 1_000_000), "1:01");
        assert_eq!(format_time(3_661 * 1_000_000), "1:01:01");
        assert_eq!(format_time(-5), "0:00");
    }

    #[test]
    fn display_name_file_and_url() {
        assert_eq!(display_name("/tmp/films/film.mkv"), "film.mkv");
        assert_eq!(
            display_name("https://ex.com/live/stream.m3u8"),
            "stream.m3u8"
        );
    }
}
