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

/// État mémorisé par fichier : pistes et vitesse choisies, pour les retrouver
/// à la réouverture du même média.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MediaState {
    /// Index de vitesse (dans la table des vitesses de l'application).
    pub speed_index: i32,
    /// Index de combo de piste audio (0 = première piste).
    pub audio_track: i32,
    /// Index de combo de sous-titres (0 = désactivés).
    pub subtitle_track: i32,
}

impl Default for MediaState {
    fn default() -> Self {
        // 3 = vitesse 1.00× ; 0 = première piste audio / sous-titres désactivés.
        Self {
            speed_index: 3,
            audio_track: 0,
            subtitle_track: 0,
        }
    }
}

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
    /// Décalage de synchronisation audio/vidéo par défaut (secondes).
    pub audio_delay_secs: f32,
    /// Gains de l'égaliseur 10 bandes (dB) — réservé à l'égaliseur audio.
    pub equalizer_gains: [f32; 10],
    /// Échelle de taille des sous-titres (1.0 = 100 %).
    pub subtitle_scale: f32,
    /// Couleur forcée des sous-titres (0xRRGGBB), ou `None` pour suivre le
    /// style d'origine (ASS).
    pub subtitle_color: Option<u32>,
    /// État mémorisé (pistes, vitesse) par média.
    pub media_states: HashMap<String, MediaState>,
    /// Vérifier les mises à jour au lancement (API GitHub).
    pub check_updates: bool,
    /// Clé d'API OpenSubtitles (vide = recherche en ligne désactivée).
    pub opensubtitles_api_key: String,
    /// Langue préférée des sous-titres en ligne (code ISO, ex. « fr »).
    pub subtitle_language: String,
    /// Langue de l'interface : « auto » (selon le système), « fr » ou « en ».
    pub language: String,
    /// Chaînes/médias marqués en favori (identifiés par leur source/URL).
    pub favorites: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            volume: 0.8,
            dark_theme: true,
            history: Vec::new(),
            resume_positions: HashMap::new(),
            subtitle_delay_secs: 0.0,
            audio_delay_secs: 0.0,
            equalizer_gains: [0.0; 10],
            subtitle_scale: 1.0,
            subtitle_color: None,
            media_states: HashMap::new(),
            check_updates: true,
            opensubtitles_api_key: String::new(),
            subtitle_language: "fr".to_string(),
            language: "auto".to_string(),
            favorites: Vec::new(),
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

    /// Mémorise l'état (pistes, vitesse) d'un média, ou l'efface s'il est
    /// entièrement par défaut (pour ne pas gonfler la configuration).
    pub fn remember_media_state(&mut self, source: &str, state: MediaState) {
        if state == MediaState::default() {
            self.media_states.remove(source);
        } else {
            self.media_states.insert(source.to_string(), state);
        }
    }

    /// État mémorisé pour un média, le cas échéant.
    pub fn media_state(&self, source: &str) -> Option<MediaState> {
        self.media_states.get(source).cloned()
    }

    /// La source est-elle marquée en favori ?
    pub fn is_favorite(&self, source: &str) -> bool {
        self.favorites.iter().any(|f| f == source)
    }

    /// Bascule l'état favori d'une source ; renvoie le nouvel état.
    pub fn toggle_favorite(&mut self, source: &str) -> bool {
        if let Some(pos) = self.favorites.iter().position(|f| f == source) {
            self.favorites.remove(pos);
            false
        } else {
            self.favorites.push(source.to_string());
            true
        }
    }

    /// Résout la langue d'interface effective (`"fr"` ou `"en"`).
    ///
    /// Le réglage `language` vaut `"auto"` (suit la variable d'environnement
    /// `LANG`/`LC_ALL`), `"fr"` ou `"en"`. Toute valeur ne commençant pas par
    /// `en` retombe sur le français (langue source de l'interface).
    pub fn resolve_language(&self) -> &'static str {
        let want = if self.language == "auto" {
            std::env::var("LC_ALL")
                .or_else(|_| std::env::var("LANG"))
                .unwrap_or_default()
                .to_lowercase()
        } else {
            self.language.to_lowercase()
        };
        if want.starts_with("en") {
            "en"
        } else {
            "fr"
        }
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
    fn media_state_remember_and_clear() {
        let mut s = Settings::default();
        // L'état par défaut n'est pas stocké.
        s.remember_media_state("a", MediaState::default());
        assert!(s.media_state("a").is_none());
        // Un état non-défaut est stocké et relu.
        let st = MediaState {
            speed_index: 5,
            audio_track: 1,
            subtitle_track: 2,
        };
        s.remember_media_state("a", st.clone());
        assert_eq!(s.media_state("a"), Some(st));
        // Repasser au défaut efface l'entrée.
        s.remember_media_state("a", MediaState::default());
        assert!(s.media_state("a").is_none());
    }

    #[test]
    fn resolve_language_explicit_and_auto() {
        // Choix explicite.
        let mut s = Settings {
            language: "en".to_string(),
            ..Settings::default()
        };
        assert_eq!(s.resolve_language(), "en");
        s.language = "fr".to_string();
        assert_eq!(s.resolve_language(), "fr");
        // Valeur inconnue → repli français.
        s.language = "de".to_string();
        assert_eq!(s.resolve_language(), "fr");
    }

    #[test]
    fn favorites_toggle() {
        let mut s = Settings::default();
        assert!(!s.is_favorite("http://ex/a"));
        assert!(s.toggle_favorite("http://ex/a")); // ajouté
        assert!(s.is_favorite("http://ex/a"));
        assert!(!s.toggle_favorite("http://ex/a")); // retiré
        assert!(!s.is_favorite("http://ex/a"));
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
