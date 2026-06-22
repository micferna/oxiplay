//! Recherche et téléchargement de sous-titres via l'API **OpenSubtitles**
//! (REST v1).
//!
//! Nécessite une **clé d'API** (gratuite sur opensubtitles.com) renseignée
//! dans les paramètres ; sans clé, la fonctionnalité est inactive. Best
//! effort : toute erreur réseau/API renvoie `None`. À appeler hors du thread
//! d'interface (réseau bloquant).

use std::time::Duration;

const API: &str = "https://api.opensubtitles.com/api/v1";
const UA: &str = concat!("oxiplay v", env!("CARGO_PKG_VERSION"));

/// Recherche le meilleur sous-titre pour un titre + langue, et renvoie son
/// contenu (texte SRT/ASS). `None` si pas de clé, pas de résultat, ou échec.
pub fn find(query: &str, language: &str, api_key: &str) -> Option<String> {
    if api_key.trim().is_empty() {
        return None;
    }
    let file_id = search_top_file(query, language, api_key)?;
    let link = download_link(file_id, api_key)?;
    fetch(&link)
}

fn search_top_file(query: &str, language: &str, api_key: &str) -> Option<i64> {
    let url = format!(
        "{API}/subtitles?query={}&languages={language}",
        urlencode(query)
    );
    let body = ureq::get(&url)
        .set("Api-Key", api_key)
        .set("User-Agent", UA)
        .timeout(Duration::from_secs(10))
        .call()
        .ok()?
        .into_string()
        .ok()?;
    parse_top_file_id(&body)
}

fn parse_top_file_id(json: &str) -> Option<i64> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    v.get("data")?
        .get(0)?
        .get("attributes")?
        .get("files")?
        .get(0)?
        .get("file_id")?
        .as_i64()
}

fn download_link(file_id: i64, api_key: &str) -> Option<String> {
    let body = ureq::post(&format!("{API}/download"))
        .set("Api-Key", api_key)
        .set("User-Agent", UA)
        .set("Content-Type", "application/json")
        .timeout(Duration::from_secs(10))
        .send_string(&format!("{{\"file_id\":{file_id}}}"))
        .ok()?
        .into_string()
        .ok()?;
    parse_download_link(&body)
}

fn parse_download_link(json: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    v.get("link")?.as_str().map(str::to_string)
}

fn fetch(link: &str) -> Option<String> {
    ureq::get(link)
        .timeout(Duration::from_secs(15))
        .call()
        .ok()?
        .into_string()
        .ok()
}

/// Encodage de query string (RFC 3986, espaces en `+`).
fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b' ' => "+".to_string(),
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_search_response() {
        let json = r#"{"data":[{"attributes":{"files":[{"file_id":12345}]}}]}"#;
        assert_eq!(parse_top_file_id(json), Some(12345));
        assert_eq!(parse_top_file_id(r#"{"data":[]}"#), None);
        assert_eq!(parse_top_file_id("pas du json"), None);
    }

    #[test]
    fn parses_download_response() {
        assert_eq!(
            parse_download_link(r#"{"link":"https://dl.os.com/s.srt"}"#),
            Some("https://dl.os.com/s.srt".to_string())
        );
        assert_eq!(parse_download_link(r#"{"message":"quota"}"#), None);
    }

    #[test]
    fn urlencodes_query() {
        assert_eq!(urlencode("big buck bunny"), "big+buck+bunny");
        assert_eq!(urlencode("le fabuleux"), "le+fabuleux");
        assert_eq!(urlencode("a/b&c"), "a%2Fb%26c");
    }

    #[test]
    fn empty_key_is_inactive() {
        assert_eq!(find("x", "fr", ""), None);
        assert_eq!(find("x", "fr", "   "), None);
    }
}
