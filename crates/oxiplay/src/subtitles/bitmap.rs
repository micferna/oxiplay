//! Sous-titres **image** (PGS Blu-ray, DVD VobSub…).
//!
//! Contrairement aux sous-titres texte (rendus par l'interface), ceux-ci
//! sont des bitmaps positionnés, livrés par le décodeur FFmpeg sous forme
//! d'images palettisées. On les convertit en RGBA puis on les **incruste**
//! directement sur l'image vidéo décodée (donc ils suivent la mise à
//! l'échelle de la vidéo).

/// Un sous-titre image prêt à incruster, borné en temps média (µs).
#[derive(Debug, Clone, PartialEq)]
pub struct BitmapSubtitle {
    pub start_us: i64,
    pub end_us: i64,
    /// Position du coin haut-gauche dans le repère de l'image vidéo.
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    /// Pixels RGBA8 (`width * height * 4`).
    pub rgba: Vec<u8>,
}

/// Piste de sous-titres image, alimentée au fil du demuxage. On conserve une
/// fenêtre glissante des dernières entrées (les bitmaps sont volumineux).
#[derive(Debug, Default)]
pub struct BitmapSubtitleTrack {
    subs: Vec<BitmapSubtitle>,
}

/// Nombre maximal de bitmaps conservés (mémoire bornée).
const MAX_BITMAPS: usize = 24;

impl BitmapSubtitleTrack {
    pub fn clear(&mut self) {
        self.subs.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.subs.is_empty()
    }

    /// Insère un sous-titre image, en évacuant les plus anciens.
    pub fn insert(&mut self, sub: BitmapSubtitle) {
        if self.subs.len() >= MAX_BITMAPS {
            self.subs.remove(0);
        }
        self.subs.push(sub);
    }

    /// Incruste sur l'image RGBA tous les sous-titres actifs à `t_us`.
    pub fn composite_active(&self, frame: &mut [u8], fw: u32, fh: u32, t_us: i64) {
        for sub in &self.subs {
            if sub.start_us <= t_us && t_us < sub.end_us {
                composite(frame, fw, fh, sub);
            }
        }
    }
}

/// Alpha-composite un bitmap sur l'image RGBA (clippé aux bords).
pub fn composite(frame: &mut [u8], fw: u32, fh: u32, sub: &BitmapSubtitle) {
    if fw == 0 || fh == 0 {
        return;
    }
    // Clippe une seule fois la zone visible, au lieu d'un test de bornes par
    // pixel : on ne parcourt que les colonnes/lignes réellement dans le cadre.
    let row_end = sub.height.min(fh.saturating_sub(sub.y)) as usize;
    let col_end = sub.width.min(fw.saturating_sub(sub.x)) as usize;
    let sub_w = sub.width as usize;
    let fw = fw as usize;
    for row in 0..row_end {
        let fy = sub.y as usize + row;
        let src_row = (row * sub_w) * 4;
        let dst_row = (fy * fw + sub.x as usize) * 4;
        for col in 0..col_end {
            let si = src_row + col * 4;
            let alpha = sub.rgba[si + 3] as u32;
            if alpha == 0 {
                continue;
            }
            let di = dst_row + col * 4;
            // out = src*a + dst*(1-a), en entiers (a sur 0..=255).
            let inv = 255 - alpha;
            for c in 0..3 {
                let src = sub.rgba[si + c] as u32;
                let dst = frame[di + c] as u32;
                frame[di + c] = ((src * alpha + dst * inv) / 255) as u8;
            }
            frame[di + 3] = 255;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn red_sub(x: u32, y: u32) -> BitmapSubtitle {
        BitmapSubtitle {
            start_us: 0,
            end_us: 1_000_000,
            x,
            y,
            width: 1,
            height: 1,
            rgba: vec![255, 0, 0, 255], // rouge opaque
        }
    }

    #[test]
    fn composite_opaque_overwrites() {
        let mut frame = vec![0u8; 2 * 2 * 4]; // 2x2 noir
        composite(&mut frame, 2, 2, &red_sub(1, 1));
        // Pixel (1,1) devient rouge : offset (y*w + x)*4 = (1*2+1)*4 = 12.
        let di = 12;
        assert_eq!(&frame[di..di + 4], &[255, 0, 0, 255]);
        // Pixel (0,0) reste noir.
        assert_eq!(&frame[0..4], &[0, 0, 0, 0]);
    }

    #[test]
    fn composite_half_alpha_blends() {
        let mut frame = vec![0, 0, 0, 255 /* un pixel */];
        let sub = BitmapSubtitle {
            start_us: 0,
            end_us: 1,
            x: 0,
            y: 0,
            width: 1,
            height: 1,
            rgba: vec![200, 200, 200, 128],
        };
        composite(&mut frame, 1, 1, &sub);
        // 200*128/255 ≈ 100.
        assert!((frame[0] as i32 - 100).abs() <= 1);
    }

    #[test]
    fn composite_clips_out_of_bounds() {
        let mut frame = vec![0u8; 4]; // 1×1 RGBA
                                      // Sous-titre hors cadre : aucune écriture, pas de panique.
        composite(&mut frame, 1, 1, &red_sub(5, 5));
        assert_eq!(frame, vec![0, 0, 0, 0]);
    }

    #[test]
    fn track_prunes_old_entries() {
        let mut track = BitmapSubtitleTrack::default();
        for i in 0..(MAX_BITMAPS + 5) {
            track.insert(BitmapSubtitle {
                start_us: i as i64,
                end_us: i as i64 + 1,
                x: 0,
                y: 0,
                width: 1,
                height: 1,
                rgba: vec![0, 0, 0, 0],
            });
        }
        assert_eq!(track.subs.len(), MAX_BITMAPS);
    }
}
