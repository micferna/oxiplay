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
}

impl PlaylistItem {
    pub fn new(source: impl Into<String>) -> Self {
        let source = source.into();
        let title = crate::utils::display_name(&source);
        Self { source, title }
    }
}

/// Playlist ordonnée avec un curseur de lecture.
#[derive(Debug, Default)]
pub struct Playlist {
    items: Vec<PlaylistItem>,
    current: Option<usize>,
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
    }

    pub fn clear(&mut self) {
        self.items.clear();
        self.current = None;
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
    }

    /// Positionne le curseur et retourne l'entrée correspondante.
    pub fn select(&mut self, index: usize) -> Option<&PlaylistItem> {
        if index < self.items.len() {
            self.current = Some(index);
            self.items.get(index)
        } else {
            None
        }
    }

    /// Avance vers l'entrée suivante (None en fin de liste).
    pub fn advance(&mut self) -> Option<&PlaylistItem> {
        let next = match self.current {
            Some(c) => c + 1,
            None if !self.items.is_empty() => 0,
            None => return None,
        };
        self.select(next)
    }

    /// Recule vers l'entrée précédente.
    pub fn previous(&mut self) -> Option<&PlaylistItem> {
        match self.current {
            Some(c) if c > 0 => self.select(c - 1),
            _ => None,
        }
    }

    /// Sauvegarde au format M3U étendu (UTF-8).
    pub fn save_m3u(&self, path: &Path) -> Result<()> {
        let mut out = String::from("#EXTM3U\n");
        for item in &self.items {
            out.push_str(&format!("#EXTINF:-1,{}\n{}\n", item.title, item.source));
        }
        std::fs::write(path, out)
            .with_context(|| format!("écriture de la playlist {}", path.display()))
    }

    /// Charge un fichier M3U/M3U8 (remplace le contenu actuel).
    pub fn load_m3u(&mut self, path: &Path) -> Result<usize> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("lecture de la playlist {}", path.display()))?;
        let base = path.parent();
        self.clear();
        let mut pending_title: Option<String> = None;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Some(info) = line.strip_prefix("#EXTINF:") {
                pending_title = info.split_once(',').map(|(_, t)| t.trim().to_string());
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
                self.items.push(item);
            }
        }
        Ok(self.items.len())
    }
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
}
