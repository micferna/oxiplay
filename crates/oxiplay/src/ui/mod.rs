//! Liaison avec l'interface Slint.
//!
//! Le code de `ui/main.slint` est compilé par `build.rs` (slint-build) ;
//! `include_modules!` injecte ici les types générés (`MainWindow`,
//! `PlaylistEntry`, …), réexportés pour le reste du crate.

slint::include_modules!();

use crate::video::VideoFrameData;
use slint::{Image, Rgba8Pixel, SharedPixelBuffer};
use std::sync::Arc;

/// Convertit une image décodée en image Slint (téléversée au GPU par le
/// moteur de rendu au prochain cycle).
pub fn frame_to_image(frame: &Arc<VideoFrameData>) -> Image {
    let buffer =
        SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(&frame.pixels, frame.width, frame.height);
    Image::from_rgba8(buffer)
}
