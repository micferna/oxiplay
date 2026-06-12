//! Analyse et interrogation des sous-titres.
//!
//! Formats pris en charge : SRT, WebVTT, ASS et SSA (texte uniquement —
//! les balises de style ASS sont retirées). Les pistes peuvent provenir
//! d'un fichier externe chargé manuellement ou d'un flux embarqué décodé
//! par le demuxeur (voir [`crate::decoder`]).

pub mod bitmap;
pub use bitmap::{BitmapSubtitle, BitmapSubtitleTrack};

use anyhow::{bail, Context, Result};
use std::path::Path;

/// Style d'affichage d'une réplique, dérivé des balises ASS/SSA. Les
/// sous-titres texte simples (SRT/VTT) utilisent les valeurs par défaut.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CueStyle {
    /// Alignement façon pavé numérique (1=bas-gauche … 9=haut-droite),
    /// 2 = bas-centre par défaut.
    pub align: u8,
    pub bold: bool,
    pub italic: bool,
    /// Couleur primaire `0xRRGGBB`, si spécifiée.
    pub color: Option<u32>,
}

impl Default for CueStyle {
    fn default() -> Self {
        Self {
            align: 2,
            bold: false,
            italic: false,
            color: None,
        }
    }
}

/// Une réplique de sous-titre, bornée en temps média (microsecondes).
#[derive(Debug, Clone, PartialEq)]
pub struct SubtitleCue {
    pub start_us: i64,
    pub end_us: i64,
    pub text: String,
    pub style: CueStyle,
}

impl SubtitleCue {
    /// Réplique texte simple (style par défaut), pour SRT/VTT.
    pub fn plain(start_us: i64, end_us: i64, text: String) -> Self {
        Self {
            start_us,
            end_us,
            text,
            style: CueStyle::default(),
        }
    }
}

/// Une piste de sous-titres triée par temps de début, interrogeable
/// efficacement par recherche binaire.
#[derive(Debug, Default, Clone)]
pub struct SubtitleTrack {
    cues: Vec<SubtitleCue>,
}

impl SubtitleTrack {
    pub fn new(mut cues: Vec<SubtitleCue>) -> Self {
        cues.sort_by_key(|c| c.start_us);
        Self { cues }
    }

    /// Compagnon conventionnel de `len` (lint `len_without_is_empty`).
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.cues.is_empty()
    }

    pub fn len(&self) -> usize {
        self.cues.len()
    }

    /// Insère une réplique en maintenant l'ordre (pistes embarquées,
    /// alimentées au fil du demuxage).
    pub fn insert(&mut self, cue: SubtitleCue) {
        // Évite les doublons exacts (certains flux répètent les répliques
        // après un seek).
        let pos = self.cues.partition_point(|c| c.start_us <= cue.start_us);
        if pos > 0 && self.cues[pos - 1] == cue {
            return;
        }
        self.cues.insert(pos, cue);
    }

    /// Texte à afficher au temps `t_us` (plusieurs répliques simultanées
    /// sont concaténées par des sauts de ligne).
    pub fn query(&self, t_us: i64) -> Option<String> {
        // Les répliques se chevauchent rarement de plus de quelques entrées :
        // on remonte un peu en arrière depuis le point de partition.
        let end = self.cues.partition_point(|c| c.start_us <= t_us);
        let start = end.saturating_sub(8);
        let active: Vec<&str> = self.cues[start..end]
            .iter()
            .filter(|c| c.start_us <= t_us && t_us < c.end_us)
            .map(|c| c.text.as_str())
            .collect();
        if active.is_empty() {
            None
        } else {
            Some(active.join("\n"))
        }
    }

    /// Style de la réplique active à `t_us` (la dernière en cas de
    /// superposition). Style par défaut si aucune.
    pub fn query_style(&self, t_us: i64) -> CueStyle {
        let end = self.cues.partition_point(|c| c.start_us <= t_us);
        let start = end.saturating_sub(8);
        self.cues[start..end]
            .iter()
            .rfind(|c| c.start_us <= t_us && t_us < c.end_us)
            .map(|c| c.style)
            .unwrap_or_default()
    }
}

/// Charge un fichier de sous-titres en détectant le format via l'extension
/// puis, à défaut, via le contenu.
pub fn load_file(path: &Path) -> Result<SubtitleTrack> {
    let raw = std::fs::read(path).with_context(|| format!("lecture de {}", path.display()))?;
    let text = decode_text(&raw);
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let cues = match ext.as_str() {
        "srt" => parse_srt(&text)?,
        "vtt" => parse_vtt(&text)?,
        "ass" | "ssa" => parse_ass(&text)?,
        _ => {
            // Détection par contenu.
            if text.trim_start().starts_with("WEBVTT") {
                parse_vtt(&text)?
            } else if text.contains("[Script Info]") || text.contains("Dialogue:") {
                parse_ass(&text)?
            } else {
                parse_srt(&text)?
            }
        }
    };
    if cues.is_empty() {
        bail!("aucune réplique trouvée dans {}", path.display());
    }
    Ok(SubtitleTrack::new(cues))
}

/// Décode le fichier en UTF-8, en tolérant un BOM et le Latin-1 hérité.
fn decode_text(raw: &[u8]) -> String {
    let raw = raw.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(raw);
    match std::str::from_utf8(raw) {
        Ok(s) => s.to_string(),
        // Repli Latin-1 : chaque octet correspond au point de code Unicode.
        Err(_) => raw.iter().map(|&b| b as char).collect(),
    }
}

/// `HH:MM:SS,mmm` (SRT) ou `HH:MM:SS.mmm` / `MM:SS.mmm` (VTT) → µs.
fn parse_timestamp(ts: &str) -> Option<i64> {
    let ts = ts.trim();
    let (hms, ms) = ts.rsplit_once(',').or_else(|| ts.rsplit_once('.'))?;
    let ms: i64 = ms.trim().get(..3.min(ms.trim().len()))?.parse().ok()?;
    let parts: Vec<&str> = hms.split(':').collect();
    let (h, m, s): (i64, i64, i64) = match parts.as_slice() {
        [h, m, s] => (h.trim().parse().ok()?, m.parse().ok()?, s.parse().ok()?),
        [m, s] => (0, m.trim().parse().ok()?, s.parse().ok()?),
        _ => return None,
    };
    // Arithmétique protégée : un fichier hostile avec des heures démesurées
    // ne doit ni paniquer (debug) ni produire une valeur erronée (release).
    h.checked_mul(3600)?
        .checked_add(m.checked_mul(60)?)?
        .checked_add(s)?
        .checked_mul(1000)?
        .checked_add(ms)?
        .checked_mul(1000)
}

/// `H:MM:SS.cc` (centisecondes, format ASS/SSA) → µs.
fn parse_ass_timestamp(ts: &str) -> Option<i64> {
    let parts: Vec<&str> = ts.trim().split(':').collect();
    let [h, m, rest] = parts.as_slice() else {
        return None;
    };
    let (s, cs) = rest.split_once('.')?;
    let (h, m, s, cs): (i64, i64, i64, i64) = (
        h.parse().ok()?,
        m.parse().ok()?,
        s.parse().ok()?,
        cs.parse().ok()?,
    );
    h.checked_mul(3600)?
        .checked_add(m.checked_mul(60)?)?
        .checked_add(s)?
        .checked_mul(100)?
        .checked_add(cs)?
        .checked_mul(10_000)
}

/// Retire les balises HTML simples (`<i>`, `<b>`, …) présentes dans SRT/VTT.
fn strip_html_tags(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_tag = false;
    for c in text.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

/// Analyse un document SRT complet.
pub fn parse_srt(input: &str) -> Result<Vec<SubtitleCue>> {
    let mut cues = Vec::new();
    // Blocs séparés par des lignes vides.
    for block in input.replace("\r\n", "\n").split("\n\n") {
        let mut lines = block.lines().filter(|l| !l.trim().is_empty()).peekable();
        // Saute le numéro de séquence s'il est présent.
        if let Some(first) = lines.peek() {
            if first.trim().chars().all(|c| c.is_ascii_digit()) {
                lines.next();
            }
        }
        let Some(timing) = lines.next() else { continue };
        let Some((start, end)) = timing.split_once("-->") else {
            continue;
        };
        let (Some(start_us), Some(end_us)) = (parse_timestamp(start), parse_timestamp(end)) else {
            continue;
        };
        let text = strip_html_tags(&lines.collect::<Vec<_>>().join("\n"));
        if !text.trim().is_empty() {
            cues.push(SubtitleCue::plain(
                start_us,
                end_us,
                text.trim().to_string(),
            ));
        }
    }
    Ok(cues)
}

/// Analyse un document WebVTT.
pub fn parse_vtt(input: &str) -> Result<Vec<SubtitleCue>> {
    let input = input.replace("\r\n", "\n");
    let mut cues = Vec::new();
    for block in input.split("\n\n") {
        // Ignore l'en-tête, les blocs NOTE/STYLE/REGION.
        let block = block.trim();
        if block.is_empty()
            || block.starts_with("WEBVTT")
            || block.starts_with("NOTE")
            || block.starts_with("STYLE")
            || block.starts_with("REGION")
        {
            continue;
        }
        let mut lines = block.lines();
        let mut timing = lines.next().unwrap_or_default();
        // Un identifiant de réplique optionnel peut précéder le timing.
        let mut rest: Vec<&str>;
        if !timing.contains("-->") {
            timing = match lines.next() {
                Some(l) if l.contains("-->") => l,
                _ => continue,
            };
        }
        rest = lines.collect();
        // Retire les réglages de position après le timestamp de fin.
        let Some((start, end_part)) = timing.split_once("-->") else {
            continue;
        };
        let end = end_part.split_whitespace().next().unwrap_or_default();
        let (Some(start_us), Some(end_us)) = (parse_timestamp(start), parse_timestamp(end)) else {
            continue;
        };
        rest.retain(|l| !l.trim().is_empty());
        let text = strip_html_tags(&rest.join("\n"));
        if !text.trim().is_empty() {
            cues.push(SubtitleCue::plain(
                start_us,
                end_us,
                text.trim().to_string(),
            ));
        }
    }
    Ok(cues)
}

/// Retire les balises de style ASS (`{\an8}`, `{\i1}`…) et convertit les
/// retours à la ligne `\N`/`\n`.
pub fn clean_ass_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' => {
                // Saute jusqu'à l'accolade fermante.
                for c2 in chars.by_ref() {
                    if c2 == '}' {
                        break;
                    }
                }
            }
            '\\' => match chars.peek() {
                Some('N') | Some('n') => {
                    chars.next();
                    out.push('\n');
                }
                Some('h') => {
                    chars.next();
                    out.push(' ');
                }
                _ => out.push(c),
            },
            c => out.push(c),
        }
    }
    out.trim().to_string()
}

/// Analyse un document ASS/SSA (lignes `Dialogue:` de la section Events).
pub fn parse_ass(input: &str) -> Result<Vec<SubtitleCue>> {
    let mut cues = Vec::new();
    // L'ordre des champs est donné par la ligne `Format:` ; le format
    // standard place Start en 2e position, End en 3e et Text en dernier.
    let mut start_idx = 1usize;
    let mut end_idx = 2usize;
    let mut text_idx = 9usize;
    for line in input.lines() {
        let line = line.trim();
        if let Some(fmt) = line.strip_prefix("Format:") {
            let fields: Vec<&str> = fmt.split(',').map(str::trim).collect();
            if fields.iter().any(|f| f.eq_ignore_ascii_case("Start")) {
                for (i, f) in fields.iter().enumerate() {
                    match f.to_ascii_lowercase().as_str() {
                        "start" => start_idx = i,
                        "end" => end_idx = i,
                        "text" => text_idx = i,
                        _ => {}
                    }
                }
            }
        } else if let Some(dialogue) = line.strip_prefix("Dialogue:") {
            let fields: Vec<&str> = dialogue.splitn(text_idx + 1, ',').collect();
            if fields.len() <= text_idx {
                continue;
            }
            let (Some(start_us), Some(end_us)) = (
                parse_ass_timestamp(fields[start_idx]),
                parse_ass_timestamp(fields[end_idx]),
            ) else {
                continue;
            };
            let raw = fields[text_idx];
            let text = clean_ass_text(raw);
            if !text.is_empty() {
                cues.push(SubtitleCue {
                    start_us,
                    end_us,
                    text,
                    style: parse_ass_style(raw),
                });
            }
        }
    }
    Ok(cues)
}

/// Extrait le texte d'une ligne ASS *embarquée* (paquet `ass` de Matroska :
/// `ReadOrder,Layer,Style,Name,MarginL,MarginR,MarginV,Effect,Text`).
pub fn embedded_ass_to_text(payload: &str) -> String {
    let text = payload.splitn(9, ',').nth(8).unwrap_or(payload);
    clean_ass_text(text)
}

/// Comme [`embedded_ass_to_text`], mais renvoie aussi le style dérivé des
/// balises d'override (alignement, gras, italique, couleur).
pub fn embedded_ass_to_styled(payload: &str) -> (String, CueStyle) {
    let raw = payload.splitn(9, ',').nth(8).unwrap_or(payload);
    (clean_ass_text(raw), parse_ass_style(raw))
}

/// Extrait le style des balises d'override ASS (`{\an8\b1\i1\c&Hxxxxxx&}`).
/// Les balises inconnues sont ignorées. Seul le premier bloc d'override
/// significatif est pris en compte (suffisant pour le positionnement et le
/// style d'une réplique courante).
pub fn parse_ass_style(raw: &str) -> CueStyle {
    let mut style = CueStyle::default();
    // Concatène le contenu de tous les blocs `{...}`.
    let mut in_tag = false;
    let mut tags = String::new();
    for c in raw.chars() {
        match c {
            '{' => in_tag = true,
            '}' => in_tag = false,
            c if in_tag => tags.push(c),
            _ => {}
        }
    }

    // Alignement : `\an<1-9>` (pavé numérique) ou `\a<1-11>` (hérité).
    if let Some(n) = capture_after(&tags, "\\an") {
        if (1..=9).contains(&n) {
            style.align = n as u8;
        }
    } else if let Some(n) = capture_after(&tags, "\\a") {
        // Conversion héritée → pavé numérique.
        style.align = match n {
            1 => 1,
            2 => 2,
            3 => 3,
            5 => 7,
            6 => 8,
            7 => 9,
            9 => 4,
            10 => 5,
            11 => 6,
            _ => 2,
        };
    }

    if let Some(v) = capture_after(&tags, "\\b") {
        style.bold = v != 0;
    }
    if let Some(v) = capture_after(&tags, "\\i") {
        style.italic = v != 0;
    }
    // Couleur primaire `\c&Hbbggrr&` ou `\1c&Hbbggrr&` (ASS = BGR).
    if let Some(bgr) = capture_ass_color(&tags) {
        let r = bgr & 0xff;
        let g = (bgr >> 8) & 0xff;
        let b = (bgr >> 16) & 0xff;
        style.color = Some((r << 16) | (g << 8) | b);
    }
    style
}

/// Lit l'entier décimal qui suit immédiatement `tag` dans `tags`.
fn capture_after(tags: &str, tag: &str) -> Option<i64> {
    let pos = tags.find(tag)? + tag.len();
    let rest = &tags[pos..];
    // S'arrête au premier caractère non chiffre (hors signe initial).
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// Lit une couleur ASS `&Hbbggrr&` après `\c` ou `\1c`.
fn capture_ass_color(tags: &str) -> Option<u32> {
    for marker in ["\\1c&H", "\\c&H"] {
        if let Some(pos) = tags.find(marker) {
            let rest = &tags[pos + marker.len()..];
            let hex: String = rest.chars().take_while(|c| c.is_ascii_hexdigit()).collect();
            if !hex.is_empty() {
                return u32::from_str_radix(&hex, 16).ok();
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRT: &str = "1\n00:00:01,000 --> 00:00:02,500\nBonjour <i>le monde</i>\n\n2\n00:00:03,000 --> 00:00:04,000\nDeuxième ligne\nsur deux lignes\n";

    #[test]
    fn srt_parsing_and_query() {
        let track = SubtitleTrack::new(parse_srt(SRT).unwrap());
        assert_eq!(track.len(), 2);
        assert_eq!(track.query(1_500_000).as_deref(), Some("Bonjour le monde"));
        assert_eq!(track.query(2_700_000), None);
        assert_eq!(
            track.query(3_500_000).as_deref(),
            Some("Deuxième ligne\nsur deux lignes")
        );
    }

    #[test]
    fn vtt_parsing() {
        let vtt = "WEBVTT\n\nNOTE test\n\n1\n00:01.000 --> 00:02.000 align:middle\nSalut\n";
        let cues = parse_vtt(vtt).unwrap();
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_us, 1_000_000);
        assert_eq!(cues[0].end_us, 2_000_000);
        assert_eq!(cues[0].text, "Salut");
    }

    #[test]
    fn ass_parsing() {
        let ass = "[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\nDialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,{\\an8}Texte\\Nstylé\n";
        let cues = parse_ass(ass).unwrap();
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "Texte\nstylé");
        assert_eq!(cues[0].start_us, 1_000_000);
    }

    #[test]
    fn embedded_ass_payload() {
        let p = "12,0,Default,,0,0,0,,Bonjour {\\i1}toi{\\i0}";
        assert_eq!(embedded_ass_to_text(p), "Bonjour toi");
    }

    #[test]
    fn overlapping_cues_are_joined() {
        let mut t = SubtitleTrack::default();
        t.insert(SubtitleCue::plain(0, 5_000_000, "A".into()));
        t.insert(SubtitleCue::plain(1_000_000, 3_000_000, "B".into()));
        assert_eq!(t.query(2_000_000).as_deref(), Some("A\nB"));
    }

    #[test]
    fn ass_style_parsing() {
        // Haut-centre, gras, italique, couleur primaire bleue (&HFF0000& = BGR).
        let style = parse_ass_style("{\\an8\\b1\\i1\\c&HFF0000&}Salut");
        assert_eq!(style.align, 8);
        assert!(style.bold && style.italic);
        assert_eq!(style.color, Some(0x0000FF)); // bleu en RGB
                                                 // Style par défaut sans balises.
        assert_eq!(parse_ass_style("Texte"), CueStyle::default());
    }

    #[test]
    fn embedded_ass_styled() {
        let (text, style) = embedded_ass_to_styled("12,0,Default,,0,0,0,,{\\an5}Centre");
        assert_eq!(text, "Centre");
        assert_eq!(style.align, 5);
    }

    #[test]
    fn hostile_timestamps_do_not_panic() {
        // Heures démesurées : dépassement i64 → None, jamais de panique.
        let srt = "1\n99999999999:00:00,000 --> 99999999999:00:01,000\nX\n";
        assert!(parse_srt(srt).unwrap().is_empty());
        // ASS de même.
        let ass = "[Events]\nFormat: Layer, Start, End, Text\nDialogue: 0,99999999999:00:00.00,99999999999:00:01.00,Y\n";
        assert!(parse_ass(ass).unwrap().is_empty());
    }

    #[test]
    fn duplicate_insert_ignored() {
        let mut t = SubtitleTrack::default();
        let cue = SubtitleCue::plain(0, 1, "X".into());
        t.insert(cue.clone());
        t.insert(cue);
        assert_eq!(t.len(), 1);
    }
}
