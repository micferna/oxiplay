//! Vérification de mise à jour au lancement.
//!
//! Interroge l'API GitHub Releases sur un thread de fond et signale, sans
//! bloquer, qu'une version plus récente est disponible. Échoue silencieusement
//! hors ligne. Désactivable via les paramètres.

use crossbeam_channel::{unbounded, Receiver};
use std::time::Duration;

/// Dépôt GitHub interrogé pour les releases.
const REPO: &str = "micferna/oxiplay";

/// Mise à jour disponible.
#[derive(Debug, Clone)]
pub struct UpdateInfo {
    /// Version disponible (ex. « 0.2.0 »).
    pub version: String,
    /// URL de la page de release.
    pub url: String,
}

/// Vérificateur asynchrone : la requête tourne en fond, le résultat éventuel
/// se récupère via [`UpdateChecker::poll`].
pub struct UpdateChecker {
    rx: Receiver<UpdateInfo>,
}

impl UpdateChecker {
    /// Démarre la vérification en arrière-plan.
    pub fn spawn() -> Self {
        let (tx, rx) = unbounded();
        let _ = std::thread::Builder::new()
            .name("oxiplay-update".into())
            .spawn(move || {
                if let Some(info) = check_latest() {
                    let _ = tx.send(info);
                }
            });
        Self { rx }
    }

    /// Vérificateur inactif (vérification désactivée).
    pub fn disabled() -> Self {
        let (_tx, rx) = unbounded();
        Self { rx }
    }

    /// Récupère le résultat s'il est arrivé (non bloquant).
    pub fn poll(&self) -> Option<UpdateInfo> {
        self.rx.try_recv().ok()
    }
}

/// Interroge l'API GitHub et renvoie une maj si la dernière release publiée
/// (préreleases « dev » incluses) est plus récente que la version courante.
fn check_latest() -> Option<UpdateInfo> {
    let url = format!("https://api.github.com/repos/{REPO}/releases?per_page=1");
    let body = ureq::get(&url)
        .set("User-Agent", concat!("oxiplay/", env!("CARGO_PKG_VERSION")))
        .set("Accept", "application/vnd.github+json")
        .timeout(Duration::from_secs(8))
        .call()
        .ok()?
        .into_string()
        .ok()?;
    let releases: serde_json::Value = serde_json::from_str(&body).ok()?;
    let latest = releases.get(0)?;
    let tag = latest.get("tag_name")?.as_str()?;
    let html_url = latest
        .get("html_url")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    if parse_version(tag)? > parse_version(env!("CARGO_PKG_VERSION"))? {
        Some(UpdateInfo {
            version: tag.trim_start_matches('v').to_string(),
            url: html_url,
        })
    } else {
        None
    }
}

/// Extrait `(major, minor, patch)` d'une version (préfixe `v` et suffixe de
/// prérelease/`+build` ignorés).
fn parse_version(s: &str) -> Option<(u32, u32, u32)> {
    let mut parts = s.trim().trim_start_matches('v').split('.');
    let major = leading_number(parts.next()?)?;
    let minor = parts.next().and_then(leading_number).unwrap_or(0);
    let patch = parts.next().and_then(leading_number).unwrap_or(0);
    Some((major, minor, patch))
}

/// Nombre en tête d'un segment (« 0-dev.1 » → 0).
fn leading_number(seg: &str) -> Option<u32> {
    seg.chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .ok()
}

/// Ouvre une URL dans le navigateur/explorateur par défaut (best effort).
pub fn open_in_browser(url: &str) {
    use std::process::Command;
    #[cfg(target_os = "windows")]
    let _ = Command::new("cmd").args(["/C", "start", "", url]).spawn();
    #[cfg(target_os = "macos")]
    let _ = Command::new("open").arg(url).spawn();
    #[cfg(target_os = "linux")]
    let _ = Command::new("xdg-open").arg(url).spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_parsing() {
        assert_eq!(parse_version("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("0.1.0"), Some((0, 1, 0)));
        assert_eq!(parse_version("v2.0.0-dev.1"), Some((2, 0, 0)));
        assert_eq!(parse_version("1.4"), Some((1, 4, 0)));
    }

    #[test]
    fn version_ordering() {
        assert!(parse_version("v0.2.0") > parse_version("v0.1.0"));
        assert!(parse_version("v1.0.0") > parse_version("v0.9.9"));
        assert!(parse_version("v0.1.1") > parse_version("v0.1.0"));
        assert!(parse_version("v0.1.0") <= parse_version("v0.1.0"));
    }
}
