//! Analyse et interrogation des sous-titres.
//!
//! Formats pris en charge : SRT, WebVTT, ASS et SSA (texte uniquement —
//! les balises de style ASS sont retirées). Les pistes peuvent provenir
//! d'un fichier externe chargé manuellement ou d'un flux embarqué décodé
//! par le demuxeur (voir [`crate::decoder`]).

use anyhow::{bail, Context, Result};
use std::path::Path;

/// Une réplique de sous-titre, bornée en temps média (microsecondes).
#[derive(Debug, Clone, PartialEq)]
pub struct SubtitleCue {
    pub start_us: i64,
    pub end_us: i64,
    pub text: String,
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
    Some(((h * 3600 + m * 60 + s) * 1000 + ms) * 1000)
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
    Some(((h * 3600 + m * 60 + s) * 100 + cs) * 10_000)
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
            cues.push(SubtitleCue {
                start_us,
                end_us,
                text: text.trim().to_string(),
            });
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
            cues.push(SubtitleCue {
                start_us,
                end_us,
                text: text.trim().to_string(),
            });
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
            let text = clean_ass_text(fields[text_idx]);
            if !text.is_empty() {
                cues.push(SubtitleCue {
                    start_us,
                    end_us,
                    text,
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
        t.insert(SubtitleCue {
            start_us: 0,
            end_us: 5_000_000,
            text: "A".into(),
        });
        t.insert(SubtitleCue {
            start_us: 1_000_000,
            end_us: 3_000_000,
            text: "B".into(),
        });
        assert_eq!(t.query(2_000_000).as_deref(), Some("A\nB"));
    }

    #[test]
    fn duplicate_insert_ignored() {
        let mut t = SubtitleTrack::default();
        let cue = SubtitleCue {
            start_us: 0,
            end_us: 1,
            text: "X".into(),
        };
        t.insert(cue.clone());
        t.insert(cue);
        assert_eq!(t.len(), 1);
    }
}
