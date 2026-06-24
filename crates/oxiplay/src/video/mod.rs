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
    /// textures R16, contenu HDR P010). Le shader échantillonne en `[0, 1]`
    /// dans les deux cas (R16 normalise directement le P010 aligné en haut).
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

/// Réduit une image RGBA à `target_w × target_h` par échantillonnage au plus
/// proche voisin (suffisant pour un GIF, peu coûteux). `src` fait
/// `src_w * src_h * 4` octets ; le résultat `target_w * target_h * 4`.
pub fn downscale_rgba(src: &[u8], src_w: u32, src_h: u32, target_w: u32, target_h: u32) -> Vec<u8> {
    let mut out = vec![0u8; (target_w * target_h * 4) as usize];
    if src_w == 0 || src_h == 0 || target_w == 0 || target_h == 0 {
        return out;
    }
    for ty in 0..target_h {
        let sy = (ty * src_h / target_h).min(src_h - 1);
        for tx in 0..target_w {
            let sx = (tx * src_w / target_w).min(src_w - 1);
            let so = ((sy * src_w + sx) * 4) as usize;
            let to = ((ty * target_w + tx) * 4) as usize;
            if so + 4 <= src.len() {
                out[to..to + 4].copy_from_slice(&src[so..so + 4]);
            }
        }
    }
    out
}

/// Dimension maximale (largeur) d'un GIF capturé : les images sont réduites
/// pour garder un fichier raisonnable.
pub const GIF_MAX_WIDTH: u32 = 480;

/// Calcule les dimensions cibles d'un GIF à partir d'une image source, bornées
/// par [`GIF_MAX_WIDTH`] en conservant le rapport (dimensions paires).
pub fn gif_target_dims(src_w: u32, src_h: u32) -> (u32, u32) {
    if src_w == 0 || src_h == 0 {
        return (0, 0);
    }
    if src_w <= GIF_MAX_WIDTH {
        return (src_w, src_h);
    }
    let w = GIF_MAX_WIDTH;
    let h = (src_h * w / src_w).max(2) & !1; // pair
    (w, h.max(2))
}

/// Encode une suite d'images RGBA (toutes en `w × h`) en GIF animé bouclé, dans
/// le dossier Images. `delay_cs` est le délai par image en centisecondes.
/// Réalisé hors du thread UI (encodage coûteux).
pub fn save_gif(frames: Vec<Vec<u8>>, w: u32, h: u32, delay_cs: u16) -> Result<PathBuf> {
    anyhow::ensure!(!frames.is_empty(), "aucune image à encoder");
    let dir = dirs::picture_dir()
        .or_else(dirs::home_dir)
        .context("aucun dossier de destination pour le GIF")?;
    std::fs::create_dir_all(&dir).ok();
    let path = dir.join(format!("oxiplay-{}.gif", crate::utils::timestamp_slug()));

    let file =
        std::fs::File::create(&path).with_context(|| format!("création de {}", path.display()))?;
    let mut encoder = gif::Encoder::new(std::io::BufWriter::new(file), w as u16, h as u16, &[])
        .context("initialisation de l'encodeur GIF")?;
    encoder
        .set_repeat(gif::Repeat::Infinite)
        .context("configuration de la boucle GIF")?;
    for mut rgba in frames {
        // `from_rgba_speed` quantifie une palette par image (vitesse 1..30,
        // plus haut = plus rapide / moins fin) ; 10 est un bon compromis.
        let mut frame = gif::Frame::from_rgba_speed(w as u16, h as u16, &mut rgba, 10);
        frame.delay = delay_cs;
        encoder
            .write_frame(&frame)
            .context("écriture d'une image GIF")?;
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downscale_halves_dimensions() {
        // 4×2 RGBA → 2×1 : échantillonne sans déborder.
        let src = vec![255u8; 4 * 2 * 4];
        let out = downscale_rgba(&src, 4, 2, 2, 1);
        assert_eq!(out.len(), 2 * 4);
        assert!(out.iter().all(|&b| b == 255));
    }

    #[test]
    fn gif_dims_bounded_and_even() {
        assert_eq!(gif_target_dims(320, 240), (320, 240)); // sous le plafond
        let (w, h) = gif_target_dims(1920, 1080);
        assert_eq!(w, GIF_MAX_WIDTH);
        assert_eq!(h % 2, 0);
        assert_eq!(h, 270);
    }

    #[test]
    fn gif_encodes_frames() {
        let (w, h) = (4u32, 4u32);
        let frames = vec![
            vec![0u8; (w * h * 4) as usize],
            vec![255u8; (w * h * 4) as usize],
        ];
        let path = save_gif(frames, w, h, 10).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[..6], b"GIF89a"); // en-tête GIF
        std::fs::remove_file(&path).ok();
    }

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
