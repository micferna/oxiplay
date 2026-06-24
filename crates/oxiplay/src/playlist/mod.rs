//! Modèle de playlist : ajout, suppression, réordonnancement, navigation,
//! sauvegarde et chargement au format M3U (compatible VLC).

use anyhow::{Context, Result};
use std::path::Path;

/// Une entrée de playlist : fichier local ou URL.
#[derive(Debug, Clone, PartialEq)]
pub struct PlaylistItem {
    /// Chemin local ou URL.
    pub source: String,
    /// Titre affiché (nom de fichier par défaut, ou #EXTINF).
    pub title: String,
    /// Catégorie / pays (attribut `group-title` M3U), vide si absent. Sert au
    /// filtrage des gros annuaires IPTV.
    pub group: String,
    /// URL du logo (attribut `tvg-logo` M3U), vide si absent.
    pub logo: String,
}

impl PlaylistItem {
    pub fn new(source: impl Into<String>) -> Self {
        let source = source.into();
        let title = crate::utils::display_name(&source);
        Self {
            source,
            title,
            group: String::new(),
            logo: String::new(),
        }
    }
}

/// Extrait la valeur d'un attribut `clé="valeur"` d'une ligne `#EXTINF`.
fn extract_attr(line: &str, key: &str) -> Option<String> {
    let needle = format!("{key}=\"");
    let start = line.find(&needle)? + needle.len();
    let rest = &line[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// Mode de répétition de la lecture, cyclé par l'utilisateur.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RepeatMode {
    /// Aucune répétition : s'arrête en fin de liste.
    #[default]
    Off,
    /// Répète toute la liste (boucle au début après la dernière entrée).
    All,
    /// Répète indéfiniment le média courant.
    One,
}

impl RepeatMode {
    /// Mode suivant dans le cycle Off → Tous → Un → Off.
    pub fn cycled(self) -> Self {
        match self {
            RepeatMode::Off => RepeatMode::All,
            RepeatMode::All => RepeatMode::One,
            RepeatMode::One => RepeatMode::Off,
        }
    }

    /// Index pour l'interface (0 = Off, 1 = Tous, 2 = Un).
    pub fn as_index(self) -> i32 {
        match self {
            RepeatMode::Off => 0,
            RepeatMode::All => 1,
            RepeatMode::One => 2,
        }
    }
}

/// Générateur pseudo-aléatoire xorshift64 minimal (lecture aléatoire). Évite
/// d'ajouter la dépendance `rand` pour un simple tirage d'index. Graine semée
/// paresseusement depuis l'horloge système au premier usage (état 0 = non semé),
/// ce qui permet de conserver `#[derive(Default)]` sur `Playlist`.
fn xorshift_seed() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9e37_79b9_7f4a_7c15)
        | 1
}

/// Playlist ordonnée avec un curseur de lecture.
#[derive(Debug, Default)]
pub struct Playlist {
    items: Vec<PlaylistItem>,
    current: Option<usize>,
    /// Lecture aléatoire activée.
    shuffle: bool,
    /// Historique ordonné des index visités depuis le début du cycle aléatoire
    /// courant (le dernier élément est la lecture en cours). Sert à éviter de
    /// rejouer une entrée tant que le cycle n'est pas épuisé, et à reculer
    /// (`previous`) dans l'ordre réellement joué.
    shuffle_history: Vec<usize>,
    /// État du PRNG (0 = pas encore semé).
    rng: u64,
}

impl Playlist {
    pub fn items(&self) -> &[PlaylistItem] {
        &self.items
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Compagnon conventionnel de `len` (lint `len_without_is_empty`).
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn current_index(&self) -> Option<usize> {
        self.current
    }

    pub fn current(&self) -> Option<&PlaylistItem> {
        self.current.and_then(|i| self.items.get(i))
    }

    /// Ajoute une entrée en fin de liste et retourne son index.
    pub fn add(&mut self, item: PlaylistItem) -> usize {
        self.items.push(item);
        self.items.len() - 1
    }

    /// Supprime l'entrée `index` en gardant le curseur cohérent.
    pub fn remove(&mut self, index: usize) {
        if index >= self.items.len() {
            return;
        }
        self.items.remove(index);
        self.current = match self.current {
            Some(c) if c == index => None,
            Some(c) if c > index => Some(c - 1),
            other => other,
        };
        // Les index de l'historique aléatoire deviennent caducs après décalage.
        self.reset_shuffle_history();
    }

    pub fn clear(&mut self) {
        self.items.clear();
        self.current = None;
        self.shuffle_history.clear();
    }

    /// Déplace l'entrée `index` de `delta` positions (±1 pour monter/descendre).
    pub fn shift(&mut self, index: usize, delta: i32) {
        let len = self.items.len() as i32;
        let target = index as i32 + delta;
        if index as i32 >= len || target < 0 || target >= len {
            return;
        }
        let target = target as usize;
        self.items.swap(index, target);
        self.current = self.current.map(|c| {
            if c == index {
                target
            } else if c == target {
                index
            } else {
                c
            }
        });
        // Les index de l'historique aléatoire deviennent caducs après échange.
        self.reset_shuffle_history();
    }

    /// Lecture aléatoire activée ?
    pub fn shuffle(&self) -> bool {
        self.shuffle
    }

    /// (Dés)active la lecture aléatoire et réamorce l'historique sur l'entrée
    /// courante (qui ne sera donc pas rejouée immédiatement).
    pub fn set_shuffle(&mut self, on: bool) {
        self.shuffle = on;
        self.reset_shuffle_history();
    }

    /// Réamorce l'historique aléatoire à la seule entrée courante. Appelé quand
    /// la liste est restructurée (les anciens index deviendraient caducs).
    fn reset_shuffle_history(&mut self) {
        self.shuffle_history.clear();
        if self.shuffle {
            if let Some(c) = self.current {
                self.shuffle_history.push(c);
            }
        }
    }

    /// Prochain entier pseudo-aléatoire (sème le PRNG au premier appel).
    fn next_rand(&mut self) -> u64 {
        if self.rng == 0 {
            self.rng = xorshift_seed();
        }
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        self.rng
    }

    /// Positionne le curseur et retourne l'entrée correspondante. En mode
    /// aléatoire, consigne l'entrée dans l'historique (sauf doublon consécutif),
    /// pour que `previous` reparcoure l'ordre réellement joué.
    pub fn select(&mut self, index: usize) -> Option<&PlaylistItem> {
        if index < self.items.len() {
            self.current = Some(index);
            if self.shuffle && self.shuffle_history.last() != Some(&index) {
                self.shuffle_history.push(index);
            }
            self.items.get(index)
        } else {
            None
        }
    }

    /// Avance vers l'entrée suivante (None en fin de liste). En mode aléatoire,
    /// tire une entrée non encore jouée du cycle courant.
    pub fn advance(&mut self) -> Option<&PlaylistItem> {
        if self.shuffle {
            return self.advance_shuffled();
        }
        let next = match self.current {
            Some(c) => c + 1,
            None if !self.items.is_empty() => 0,
            None => return None,
        };
        self.select(next)
    }

    /// Tire la prochaine entrée aléatoire : uniforme parmi les entrées non
    /// encore jouées ce cycle ; une fois toutes jouées, recommence un cycle en
    /// évitant de rejouer la courante d'affilée.
    fn advance_shuffled(&mut self) -> Option<&PlaylistItem> {
        let n = self.items.len();
        if n == 0 {
            return None;
        }
        if n == 1 {
            return self.select(0);
        }
        let mut candidates: Vec<usize> = (0..n)
            .filter(|i| !self.shuffle_history.contains(i))
            .collect();
        if candidates.is_empty() {
            // Cycle épuisé : on repart à zéro sans rejouer la courante.
            self.shuffle_history.clear();
            if let Some(c) = self.current {
                self.shuffle_history.push(c);
            }
            candidates = (0..n).filter(|i| Some(*i) != self.current).collect();
        }
        let pick = candidates[(self.next_rand() % candidates.len() as u64) as usize];
        self.select(pick)
    }

    /// Recule vers l'entrée précédente. En mode aléatoire, remonte l'historique
    /// des entrées réellement jouées.
    pub fn previous(&mut self) -> Option<&PlaylistItem> {
        if self.shuffle {
            if self.shuffle_history.len() < 2 {
                return None;
            }
            self.shuffle_history.pop(); // retire la lecture courante
            let prev = *self.shuffle_history.last().unwrap();
            self.current = Some(prev); // sans réenregistrer dans l'historique
            return self.items.get(prev);
        }
        match self.current {
            Some(c) if c > 0 => self.select(c - 1),
            _ => None,
        }
    }

    /// Sauvegarde au format M3U étendu (UTF-8).
    pub fn save_m3u(&self, path: &Path) -> Result<()> {
        let mut out = String::from("#EXTM3U\n");
        for item in &self.items {
            if item.group.is_empty() {
                out.push_str(&format!("#EXTINF:-1,{}\n{}\n", item.title, item.source));
            } else {
                out.push_str(&format!(
                    "#EXTINF:-1 group-title=\"{}\",{}\n{}\n",
                    item.group, item.title, item.source
                ));
            }
        }
        std::fs::write(path, out)
            .with_context(|| format!("écriture de la playlist {}", path.display()))
    }

    /// Charge un fichier M3U/M3U8 (remplace le contenu actuel).
    pub fn load_m3u(&mut self, path: &Path) -> Result<usize> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("lecture de la playlist {}", path.display()))?;
        self.clear();
        self.items = parse_m3u_content(&text, path.parent());
        Ok(self.items.len())
    }
}

/// Parse le contenu d'une playlist M3U/M3U8 en entrées (titre `#EXTINF` +
/// source). `base` résout les chemins relatifs d'un fichier local ; passer
/// `None` pour un contenu distant (annuaire IPTV récupéré par le réseau).
pub fn parse_m3u_content(text: &str, base: Option<&Path>) -> Vec<PlaylistItem> {
    let mut items = Vec::new();
    let mut pending_title: Option<String> = None;
    let mut pending_group: Option<String> = None;
    let mut pending_logo: Option<String> = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(info) = line.strip_prefix("#EXTINF:") {
            pending_title = info.split_once(',').map(|(_, t)| t.trim().to_string());
            pending_group = extract_attr(info, "group-title");
            pending_logo = extract_attr(info, "tvg-logo");
        } else if line.starts_with('#') {
            continue;
        } else {
            // Résout les chemins relatifs par rapport au fichier M3U.
            let source = if crate::streaming::is_url(line) || Path::new(line).is_absolute() {
                line.to_string()
            } else if let Some(base) = base {
                base.join(line).to_string_lossy().into_owned()
            } else {
                line.to_string()
            };
            let mut item = PlaylistItem::new(source);
            if let Some(t) = pending_title.take().filter(|t| !t.is_empty()) {
                item.title = t;
            }
            if let Some(g) = pending_group.take().filter(|g| !g.is_empty()) {
                item.group = g;
            }
            if let Some(l) = pending_logo.take().filter(|l| !l.is_empty()) {
                item.logo = l;
            }
            items.push(item);
        }
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    fn playlist_abc() -> Playlist {
        let mut p = Playlist::default();
        p.add(PlaylistItem::new("/tmp/a.mp4"));
        p.add(PlaylistItem::new("/tmp/b.mp4"));
        p.add(PlaylistItem::new("/tmp/c.mp4"));
        p
    }

    #[test]
    fn navigation() {
        let mut p = playlist_abc();
        assert_eq!(p.advance().unwrap().title, "a.mp4");
        assert_eq!(p.advance().unwrap().title, "b.mp4");
        assert_eq!(p.previous().unwrap().title, "a.mp4");
        p.select(2);
        assert!(p.advance().is_none());
    }

    #[test]
    fn remove_keeps_cursor() {
        let mut p = playlist_abc();
        p.select(2);
        p.remove(0);
        assert_eq!(p.current_index(), Some(1));
        assert_eq!(p.current().unwrap().title, "c.mp4");
        p.remove(1);
        assert_eq!(p.current_index(), None);
    }

    #[test]
    fn shift_moves_and_tracks_cursor() {
        let mut p = playlist_abc();
        p.select(1);
        p.shift(1, -1);
        assert_eq!(p.items()[0].title, "b.mp4");
        assert_eq!(p.current_index(), Some(0));
        p.shift(0, -1); // hors limites : sans effet
        assert_eq!(p.current_index(), Some(0));
    }

    #[test]
    fn shuffle_visits_all_without_repeat_then_recycles() {
        let mut p = playlist_abc();
        p.add(PlaylistItem::new("/tmp/d.mp4"));
        p.select(0);
        p.set_shuffle(true);
        // Un cycle visite les 3 entrées restantes (jamais la courante) sans
        // doublon, puis recommence.
        let mut seen = std::collections::HashSet::new();
        seen.insert(0usize);
        for _ in 0..3 {
            let idx = p.current_index().unwrap();
            assert!(p.advance().is_some());
            let next_idx = p.current_index().unwrap();
            assert_ne!(next_idx, idx, "ne rejoue pas la courante d'affilée");
            assert!(seen.insert(next_idx), "pas de doublon dans le cycle");
        }
        assert_eq!(seen.len(), 4, "cycle complet : toutes les entrées vues");
        // Cycle suivant : on doit pouvoir rejouer (l'historique s'est réamorcé).
        assert!(p.advance().is_some());
    }

    #[test]
    fn shuffle_previous_rewinds_play_history() {
        let mut p = playlist_abc();
        p.select(0);
        p.set_shuffle(true);
        let first = p.current_index().unwrap();
        assert!(p.advance().is_some());
        let second = p.current_index().unwrap();
        assert_ne!(first, second);
        // `previous` revient sur l'entrée précédemment jouée, pas index-1.
        assert!(p.previous().is_some());
        assert_eq!(p.current_index(), Some(first));
    }

    #[test]
    fn repeat_mode_cycles() {
        assert_eq!(RepeatMode::default(), RepeatMode::Off);
        assert_eq!(RepeatMode::Off.cycled(), RepeatMode::All);
        assert_eq!(RepeatMode::All.cycled(), RepeatMode::One);
        assert_eq!(RepeatMode::One.cycled(), RepeatMode::Off);
        assert_eq!(
            [RepeatMode::Off, RepeatMode::All, RepeatMode::One].map(RepeatMode::as_index),
            [0, 1, 2]
        );
    }

    #[test]
    fn m3u_roundtrip() {
        let mut p = playlist_abc();
        p.add(PlaylistItem::new("https://ex.com/live.m3u8"));
        let dir = std::env::temp_dir().join("oxiplay-test-playlist");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("list.m3u");
        p.save_m3u(&file).unwrap();

        let mut loaded = Playlist::default();
        let n = loaded.load_m3u(&file).unwrap();
        assert_eq!(n, 4);
        assert_eq!(loaded.items()[3].source, "https://ex.com/live.m3u8");
        assert_eq!(loaded.items()[0].title, "a.mp4");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn parse_remote_channel_directory() {
        // Annuaire IPTV distant : titres `#EXTINF` + URLs absolues, base `None`.
        let content = "#EXTM3U\n\
                       #EXTINF:-1 tvg-id=\"a\" tvg-logo=\"https://ex.com/a.png\" group-title=\"News\",Chaîne A\nhttps://ex.com/a.m3u8\n\
                       #EXTINF:-1,Chaîne B\nhttps://ex.com/b.m3u8\n";
        let items = parse_m3u_content(content, None);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].title, "Chaîne A");
        assert_eq!(items[0].source, "https://ex.com/a.m3u8");
        assert_eq!(items[0].logo, "https://ex.com/a.png");
        assert_eq!(items[0].group, "News");
        assert_eq!(items[1].title, "Chaîne B");
        // La chaîne B n'a pas d'attributs : logo/groupe vides (pas de fuite
        // depuis l'entrée précédente).
        assert!(items[1].logo.is_empty());
        assert!(items[1].group.is_empty());
    }
}
