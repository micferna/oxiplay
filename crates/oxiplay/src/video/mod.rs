//! Types et services côté vidéo : images décodées prêtes à afficher,
//! capture d'écran PNG.
//!
//! Le pipeline actuel convertit chaque image en RGBA8 (via libswscale) et
//! la livre au thread UI qui la téléverse vers le GPU via Slint. Une voie
//! « zéro copie » wgpu (textures YUV + shader de conversion) est prévue —
//! voir docs/ARCHITECTURE.md, section « Rendu vidéo ».

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Vrai quand le rendu vidéo GPU (wgpu) est actif. Renseigné par le module
/// `render` (feature `gpu`) une fois le device Slint capturé ; lu par le
/// décodeur pour décider d'extraire les plans YUV. Toujours `false` sans la
/// feature `gpu`.
pub static GPU_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Le rendu GPU est-il actif ? (voir [`GPU_ACTIVE`].)
pub fn gpu_active() -> bool {
    GPU_ACTIVE.load(Ordering::Relaxed)
}

/// Plans YUV planaires 8 bits (4:2:0) d'une image décodée, avec ses
/// métadonnées colorimétriques — alimente le shader de conversion GPU.
#[derive(Debug, Clone)]
pub struct YuvFrame {
    pub width: u32,
    pub height: u32,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
    pub y_stride: u32,
    pub uv_stride: u32,
    pub chroma_width: u32,
    pub chroma_height: u32,
    /// Octets par échantillon : 1 (8 bits, textures R8) ou 2 (10/16 bits,
    /// textures R16, contenu HDR P010). Le shader échantillonne en [0,1] dans
    /// les deux cas (R16 normalise directement le P010 aligné en haut).
    pub bytes_per_sample: u32,
    /// 0 = BT.601, 1 = BT.709, 2 = BT.2020.
    pub matrix: u32,
    /// 0 = plage limitée, 1 = plage complète.
    pub full_range: u32,
    /// 0 = SDR, 1 = HDR PQ, 2 = HDR HLG.
    pub transfer: u32,
}

/// Une image vidéo décodée, en RGBA8 compact (stride == width * 4).
///
/// `yuv` est renseigné uniquement quand le chemin GPU est actif (plans bruts
/// pour le shader) ; le RGBA reste toujours présent (captures d'écran,
/// incrustation des sous-titres image, repli logiciel).
#[derive(Debug, Clone)]
pub struct VideoFrameData {
    pub width: u32,
    pub height: u32,
    /// Pixels RGBA8, `width * height * 4` octets.
    pub pixels: Vec<u8>,
    /// Horodatage de présentation en temps média (µs).
    pub pts_us: i64,
    /// Plans YUV pour le rendu GPU (`None` = chemin logiciel RGBA).
    pub yuv: Option<YuvFrame>,
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
            yuv: None,
        });
        let path = save_screenshot(&frame).unwrap();
        let data = std::fs::read(&path).unwrap();
        assert_eq!(&data[1..4], b"PNG");
        std::fs::remove_file(path).ok();
    }
}
