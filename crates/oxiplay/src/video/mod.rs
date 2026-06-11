//! Types et services côté vidéo : images décodées prêtes à afficher,
//! capture d'écran PNG.
//!
//! Le pipeline actuel convertit chaque image en RGBA8 (via libswscale) et
//! la livre au thread UI qui la téléverse vers le GPU via Slint. Une voie
//! « zéro copie » wgpu (textures YUV + shader de conversion) est prévue —
//! voir docs/ARCHITECTURE.md, section « Rendu vidéo ».

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::Arc;

/// Une image vidéo décodée, en RGBA8 compact (stride == width * 4).
#[derive(Debug, Clone)]
pub struct VideoFrameData {
    pub width: u32,
    pub height: u32,
    /// Pixels RGBA8, `width * height * 4` octets.
    pub pixels: Vec<u8>,
    /// Horodatage de présentation en temps média (µs).
    pub pts_us: i64,
}

/// Enregistre une image en PNG dans le dossier Images de l'utilisateur
/// (ou le dossier personnel à défaut) et retourne le chemin écrit.
pub fn save_screenshot(frame: &Arc<VideoFrameData>) -> Result<PathBuf> {
    let dir = dirs::picture_dir()
        .or_else(dirs::home_dir)
        .context("aucun dossier de destination pour la capture")?;
    std::fs::create_dir_all(&dir).ok();
    let path = dir.join(format!("oxiplay-{}.png", crate::utils::timestamp_slug()));

    let file =
        std::fs::File::create(&path).with_context(|| format!("création de {}", path.display()))?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), frame.width, frame.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder
        .write_header()
        .context("écriture de l'en-tête PNG")?;
    writer
        .write_image_data(&frame.pixels)
        .context("écriture des pixels PNG")?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screenshot_writes_valid_png() {
        let frame = Arc::new(VideoFrameData {
            width: 2,
            height: 2,
            pixels: vec![255; 2 * 2 * 4],
            pts_us: 0,
        });
        let path = save_screenshot(&frame).unwrap();
        let data = std::fs::read(&path).unwrap();
        assert_eq!(&data[1..4], b"PNG");
        std::fs::remove_file(path).ok();
    }
}
