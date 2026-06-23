//! Liaison avec l'interface Slint.
//!
//! Le code de `ui/main.slint` est compilé par `build.rs` (slint-build) ;
//! `include_modules!` injecte ici les types générés (`MainWindow`,
//! `PlaylistEntry`, …), réexportés pour le reste du crate.

slint::include_modules!();

use crate::video::VideoFrameData;
use slint::{Image, Rgba8Pixel, SharedPixelBuffer};
use std::sync::Arc;

/// Convertit une image décodée en image Slint.
///
/// Si les plans YUV sont présents (chemin GPU actif), on les convertit via le
/// pipeline wgpu (YUV→RGB + HDR) et on importe la texture résultante. Sinon —
/// ou si le rendu GPU échoue — on téléverse le RGBA logiciel (repli sûr).
pub fn frame_to_image(frame: &Arc<VideoFrameData>) -> Image {
    #[cfg(feature = "gpu")]
    if let Some(yuv) = &frame.yuv {
        if let Some(image) = crate::render::render_yuv_to_image(yuv) {
            return image;
        }
    }
    let buffer =
        SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(&frame.pixels, frame.width, frame.height);
    Image::from_rgba8(buffer)
}
