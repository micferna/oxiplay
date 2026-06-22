//! Graphe de filtres **vidéo** (libavfilter) : désentrelacement `yadif` et/ou
//! rotation `transpose`, composés selon les besoins de la trame courante.
//!
//! ```text
//! buffer → <spec> → buffersink
//! ```
//!
//! La `spec` est construite par `video.rs` (par ex. `yadif=…,transpose=1`).
//! N'est instancié que lorsqu'au moins un filtre est requis (entrelacement
//! détecté ou rotation active) : le contenu progressif non tourné ne traverse
//! jamais le graphe. `yadif` est en `mode=0` (une image en sortie par image
//! en entrée) et `deint=1` (ne traite que les trames entrelacées).

use anyhow::{Context, Result};
use ffmpeg_the_third as ffmpeg;
use ffmpeg_the_third::filter;

/// Graphe de filtres compilé, avec la spec/géométrie/format qui l'a produit
/// (pour décider d'une reconstruction quand l'un d'eux change).
pub struct VideoFilter {
    graph: filter::Graph,
    pub spec: String,
    pub format: ffmpeg::format::Pixel,
    pub width: u32,
    pub height: u32,
}

impl VideoFilter {
    /// Compile un graphe `buffer → <spec> → buffersink` pour une géométrie,
    /// un format et un rapport d'aspect d'échantillon donnés.
    pub fn new(
        spec: &str,
        format: ffmpeg::format::Pixel,
        width: u32,
        height: u32,
        time_base: ffmpeg::Rational,
        sar: ffmpeg::Rational,
    ) -> Result<Self> {
        let pix_name = format
            .descriptor()
            .map(|d| d.name())
            .context("format de pixel sans descripteur")?;
        // Un SAR nul/à zéro casse l'analyse du filtre buffer : on borne à 1/1.
        let sar_n = if sar.numerator() > 0 {
            sar.numerator()
        } else {
            1
        };
        let sar_d = if sar.denominator() > 0 {
            sar.denominator()
        } else {
            1
        };
        let args = format!(
            "video_size={width}x{height}:pix_fmt={pix_name}:time_base={}/{}:pixel_aspect={sar_n}/{sar_d}",
            time_base.numerator().max(1),
            time_base.denominator().max(1),
        );

        let mut graph = filter::Graph::new();
        let buffer = filter::find("buffer").context("filtre buffer introuvable")?;
        let buffersink = filter::find("buffersink").context("filtre buffersink introuvable")?;
        graph
            .add(&buffer, "in", &args)
            .context("création de la source buffer")?;
        graph
            .add(&buffersink, "out", "")
            .context("création du puits buffersink")?;
        graph
            .output("in", 0)?
            .input("out", 0)?
            .parse(spec)
            .with_context(|| format!("analyse de la spec de filtres vidéo : {spec}"))?;
        graph
            .validate()
            .context("validation du graphe de filtres vidéo")?;

        Ok(Self {
            graph,
            spec: spec.to_string(),
            format,
            width,
            height,
        })
    }

    /// Injecte une trame et livre chaque trame filtrée à `sink` (généralement
    /// une seule, parfois zéro le temps que `yadif` dispose d'une trame
    /// voisine).
    pub fn process(
        &mut self,
        frame: &ffmpeg::frame::Video,
        mut sink: impl FnMut(&ffmpeg::frame::Video),
    ) -> Result<()> {
        self.graph
            .get("in")
            .context("source de filtre absente")?
            .source()
            .add(frame)
            .context("injection dans le graphe vidéo")?;
        let mut filtered = ffmpeg::frame::Video::empty();
        while self
            .graph
            .get("out")
            .context("puits de filtre absent")?
            .sink()
            .frame(&mut filtered)
            .is_ok()
        {
            sink(&filtered);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deinterlace_and_rotate_graphs_build() {
        // Valide la chaîne d'arguments et la présence des filtres pour le
        // format le plus courant : désentrelacement seul, puis combiné rotation.
        for spec in [
            "yadif=mode=0:parity=-1:deint=1",
            "yadif=mode=0:deint=1,transpose=1",
        ] {
            let f = VideoFilter::new(
                spec,
                ffmpeg::format::Pixel::YUV420P,
                720,
                576,
                ffmpeg::Rational::new(1, 25),
                ffmpeg::Rational::new(1, 1),
            );
            assert!(
                f.is_ok(),
                "construction du graphe « {spec} » : {:?}",
                f.err()
            );
        }
    }
}
