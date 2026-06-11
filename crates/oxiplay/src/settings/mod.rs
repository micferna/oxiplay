//! Paramètres persistants, historique de lecture et reprise de position.
//!
//! Tout est sérialisé en JSON dans le répertoire de configuration de la
//! plateforme (`~/.config/oxiplay` sous Linux, `%APPDATA%` sous Windows,
//! `~/Library/Application Support` sous macOS).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// Nombre maximal d'entrées d'historique conservées.
const HISTORY_LIMIT: usize = 50;
/// Une position n'est mémorisée que si elle dépasse ce seuil…
const RESUME_MIN_US: i64 = 10_000_000;
/// …et qu'elle est avant ce pourcentage de la durée (sinon : « terminé »).
const RESUME_MAX_RATIO: f64 = 0.95;

/// Paramètres de l'application, persistés entre les sessions.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Volume entre 0.0 et 1.25.
    pub volume: f32,
    /// Thème sombre activé.
    pub dark_theme: bool,
    /// Historique des derniers médias lus (du plus récent au plus ancien).
    pub history: Vec<String>,
    /// Position de reprise par média (µs), pour « reprendre où on s'était
    /// arrêté ».
    pub resume_positions: HashMap<String, i64>,
    /// Décalage des sous-titres appliqué par défaut (secondes).
    pub subtitle_delay_secs: f32,
    /// Gains de l'égaliseur 10 bandes (dB) — réservé à l'égaliseur audio.
    pub equalizer_gains: [f32; 10],
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            volume: 0.8,
            dark_theme: true,
            history: Vec::new(),
            resume_positions: HashMap::new(),
            subtitle_delay_secs: 0.0,
            equalizer_gains: [0.0; 10],
        }
    }
}

impl Settings {
    fn config_file() -> Option<PathBuf> {
        Some(dirs::config_dir()?.join("oxiplay").join("settings.json"))
    }

    /// Charge les paramètres (valeurs par défaut si absents ou corrompus).
    pub fn load() -> Self {
        Self::config_file()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// Sauvegarde silencieuse (les erreurs sont journalisées, pas fatales).
    pub fn save(&self) {
        let Some(path) = Self::config_file() else {
            return;
        };
        if let Some(dir) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(dir) {
                log::warn!("création du dossier de config impossible : {e}");
                return;
            }
        }
        match serde_json::to_string_pretty(self) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&path, json) {
                    log::warn!("sauvegarde des paramètres impossible : {e}");
                }
            }
            Err(e) => log::warn!("sérialisation des paramètres impossible : {e}"),
        }
    }

    /// Enregistre un média dans l'historique (déduplication + limite).
    pub fn push_history(&mut self, source: &str) {
        self.history.retain(|s| s != source);
        self.history.insert(0, source.to_string());
        self.history.truncate(HISTORY_LIMIT);
    }

    /// Mémorise la position d'arrêt d'un média, ou l'oublie si la lecture
    /// était quasiment terminée ou venait de commencer.
    pub fn remember_position(&mut self, source: &str, position_us: i64, duration_us: i64) {
        let near_end =
            duration_us > 0 && position_us as f64 / duration_us as f64 > RESUME_MAX_RATIO;
        if position_us < RESUME_MIN_US || near_end {
            self.resume_positions.remove(source);
        } else {
            self.resume_positions
                .insert(source.to_string(), position_us);
        }
    }

    /// Position de reprise éventuelle pour un média.
    pub fn resume_position(&self, source: &str) -> Option<i64> {
        self.resume_positions.get(source).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_dedup_and_limit() {
        let mut s = Settings::default();
        for i in 0..60 {
            s.push_history(&format!("/m/{i}.mp4"));
        }
        s.push_history("/m/10.mp4");
        assert_eq!(s.history.len(), HISTORY_LIMIT);
        assert_eq!(s.history[0], "/m/10.mp4");
        assert_eq!(s.history.iter().filter(|h| *h == "/m/10.mp4").count(), 1);
    }

    #[test]
    fn resume_rules() {
        let mut s = Settings::default();
        let dur = 100 * 60 * 1_000_000i64;
        // Trop tôt : pas mémorisé.
        s.remember_position("a", 5_000_000, dur);
        assert_eq!(s.resume_position("a"), None);
        // Milieu : mémorisé.
        s.remember_position("a", dur / 2, dur);
        assert_eq!(s.resume_position("a"), Some(dur / 2));
        // Quasi fini : oublié.
        s.remember_position("a", dur - 1_000_000, dur);
        assert_eq!(s.resume_position("a"), None);
    }

    #[test]
    fn settings_json_roundtrip() {
        let mut s = Settings {
            volume: 0.5,
            ..Settings::default()
        };
        s.push_history("/x.mkv");
        let json = serde_json::to_string(&s).unwrap();
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(back.volume, 0.5);
        assert_eq!(back.history, vec!["/x.mkv"]);
    }
}
