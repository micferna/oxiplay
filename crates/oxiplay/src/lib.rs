//! # OxiPlay — lecteur multimédia en Rust
//!
//! Bibliothèque interne du lecteur : moteur de lecture ([`player`]),
//! pipeline de décodage FFmpeg ([`decoder`]), sortie audio ([`audio`]),
//! sous-titres ([`subtitles`]), playlist ([`playlist`]), streaming réseau
//! ([`streaming`]), paramètres persistants ([`settings`]) et liaison avec
//! l'interface Slint ([`ui`], [`app`]).
//!
//! Le binaire (`main.rs`) ne fait qu'assembler ces briques ; tout le cœur
//! est testable sans interface graphique (voir `tests/`).

pub mod app;
pub mod audio;
pub mod decoder;
pub mod inhibit;
pub mod media_controls;
pub mod opensubtitles;
pub mod player;
pub mod playlist;
/// Rendu vidéo GPU zéro-copie + HDR (expérimental, feature `gpu`).
#[cfg(feature = "gpu")]
pub mod render;
pub mod settings;
pub mod streaming;
pub mod subtitles;
pub mod ui;
pub mod update;
pub mod utils;
pub mod video;
pub mod ytdlp;
