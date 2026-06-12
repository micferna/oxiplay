//! Graphe de filtres audio (libavfilter) :
//!
//! ```text
//! abuffer → equalizer×10 (égaliseur) → atempo×N (vitesse sans
//!           changement de hauteur) → aformat (f32 stéréo, taux
//!           périphérique) → abuffersink
//! ```
//!
//! Le graphe remplace l'ancien rééchantillonnage « à la vitesse » : `atempo`
//! étire le temps **sans** transposer la hauteur, et l'égaliseur 10 bandes
//! s'insère dans la même chaîne. `aformat` réalise la conversion finale
//! (format, disposition, fréquence) auparavant confiée à swresample.

use anyhow::{Context, Result};
use ffmpeg_the_third as ffmpeg;
use ffmpeg_the_third::filter;

/// Fréquences centrales des 10 bandes de l'égaliseur (Hz), espacées par
/// octaves — convention des égaliseurs graphiques.
pub const EQ_FREQUENCIES: [u32; 10] = [31, 62, 125, 250, 500, 1000, 2000, 4000, 8000, 16000];

/// Décompose une vitesse de lecture (0.25–4.0) en une suite de facteurs
/// `atempo`, chacun dans l'intervalle `[0.5, 2.0]` autorisé par le filtre.
///
/// Retourne une liste vide pour une vitesse ≈ 1.0 (aucun étirement).
pub fn atempo_factors(speed: f64) -> Vec<f64> {
    let speed = speed.clamp(0.25, 4.0);
    if (speed - 1.0).abs() < 1e-3 {
        return Vec::new();
    }
    let mut factors = Vec::new();
    let mut remaining = speed;
    while remaining > 2.0 + 1e-9 {
        factors.push(2.0);
        remaining /= 2.0;
    }
    while remaining < 0.5 - 1e-9 {
        factors.push(0.5);
        remaining *= 2.0;
    }
    factors.push(remaining);
    factors
}

/// Construit la spec de filtres (chaîne libavfilter) pour une vitesse et un
/// jeu de gains d'égaliseur donnés. Les bandes à gain nul et les `atempo`
/// inutiles (vitesse 1.0) sont omis ; `aformat` est toujours présent pour la
/// conversion vers la sortie stéréo `f32` du périphérique.
pub fn build_spec(speed: f64, eq_gains: &[f32; 10], device_rate: u32) -> String {
    let mut parts: Vec<String> = Vec::new();
    for (band, &gain) in eq_gains.iter().enumerate() {
        if gain.abs() > 0.05 {
            parts.push(format!(
                "equalizer=f={}:width_type=o:width=1:g={:.2}",
                EQ_FREQUENCIES[band], gain
            ));
        }
    }
    for factor in atempo_factors(speed) {
        parts.push(format!("atempo={factor:.6}"));
    }
    parts.push(format!(
        "aformat=sample_fmts=flt:channel_layouts=stereo:sample_rates={device_rate}"
    ));
    parts.join(",")
}

/// Graphe de filtres audio compilé, ainsi que la configuration d'entrée et de
/// traitement qui l'a produit (pour décider d'une reconstruction).
pub struct AudioFilter {
    graph: filter::Graph,
    pub in_format: ffmpeg::format::Sample,
    pub in_channels: u32,
    pub in_rate: u32,
    pub speed_milli: u32,
    pub eq_generation: u64,
}

impl AudioFilter {
    /// Compile le graphe pour un format d'entrée et une spec donnés.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        in_format: ffmpeg::format::Sample,
        in_layout: &ffmpeg::ChannelLayout,
        in_rate: u32,
        time_base: ffmpeg::Rational,
        spec: &str,
        speed_milli: u32,
        eq_generation: u64,
    ) -> Result<Self> {
        let mut graph = filter::Graph::new();
        let args = format!(
            "time_base={}/{}:sample_rate={}:sample_fmt={}:channel_layout={}",
            time_base.numerator().max(1),
            time_base.denominator().max(1),
            in_rate,
            in_format.name(),
            in_layout.description(),
        );
        let abuffer = filter::find("abuffer").context("filtre abuffer introuvable")?;
        let abuffersink = filter::find("abuffersink").context("filtre abuffersink introuvable")?;
        graph
            .add(&abuffer, "in", &args)
            .context("création de la source abuffer")?;
        graph
            .add(&abuffersink, "out", "")
            .context("création du puits abuffersink")?;
        graph
            .output("in", 0)?
            .input("out", 0)?
            .parse(spec)
            .with_context(|| format!("analyse de la spec de filtres : {spec}"))?;
        graph
            .validate()
            .context("validation du graphe de filtres")?;

        Ok(Self {
            graph,
            in_format,
            in_channels: in_layout.channels(),
            in_rate,
            speed_milli,
            eq_generation,
        })
    }

    /// Injecte une trame décodée et livre chaque trame filtrée à `sink`.
    pub fn process(
        &mut self,
        frame: &ffmpeg::frame::Audio,
        mut sink: impl FnMut(&ffmpeg::frame::Audio),
    ) -> Result<()> {
        self.graph
            .get("in")
            .context("source de filtre absente")?
            .source()
            .add(frame)
            .context("injection dans le graphe de filtres")?;
        let mut filtered = ffmpeg::frame::Audio::empty();
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

    fn product(factors: &[f64]) -> f64 {
        factors.iter().product()
    }

    #[test]
    fn atempo_identity_speed() {
        assert!(atempo_factors(1.0).is_empty());
    }

    #[test]
    fn atempo_factors_in_range_and_correct() {
        for &speed in &[0.25, 0.3, 0.5, 0.75, 1.25, 1.5, 2.0, 3.0, 4.0] {
            let factors = atempo_factors(speed);
            assert!(!factors.is_empty(), "vitesse {speed}");
            for &f in &factors {
                assert!(
                    (0.5..=2.0).contains(&f),
                    "facteur {f} hors plage (vitesse {speed})"
                );
            }
            assert!(
                (product(&factors) - speed).abs() < 1e-6,
                "produit {} ≠ {speed}",
                product(&factors)
            );
        }
    }

    #[test]
    fn spec_includes_only_active_bands() {
        let mut gains = [0.0f32; 10];
        gains[0] = 6.0; // 31 Hz
        gains[9] = -3.0; // 16 kHz
        let spec = build_spec(1.0, &gains, 48_000);
        assert!(spec.contains("equalizer=f=31"));
        assert!(spec.contains("equalizer=f=16000"));
        assert_eq!(spec.matches("equalizer=").count(), 2);
        assert!(!spec.contains("atempo")); // vitesse 1.0
        assert!(spec.contains("sample_rates=48000"));
    }

    #[test]
    fn spec_includes_atempo_for_speed() {
        let spec = build_spec(2.0, &[0.0; 10], 44_100);
        assert!(spec.contains("atempo=2"));
        assert_eq!(spec.matches("equalizer=").count(), 0);
    }
}
