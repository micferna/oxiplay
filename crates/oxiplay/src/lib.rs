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
pub mod player;
pub mod playlist;
pub mod settings;
pub mod streaming;
pub mod subtitles;
pub mod ui;
pub mod utils;
pub mod video;
