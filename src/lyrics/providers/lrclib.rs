use serde::Deserialize;

use crate::config::Config;
use crate::lyrics::lrc::parse_lrc;
use crate::lyrics::model::{LyricLine, Lyrics, ProviderId, TrackMeta};
use crate::lyrics::provider::LyricsProvider;

const USER_AGENT: &str = concat!(
    "ncspot/", env!("CARGO_PKG_VERSION"), " (https://github.com/hrkfdn/ncspot)"
);

#[derive(Deserialize)]
struct GetResponse {
    #[serde(rename = "plainLyrics")]
    plain_lyrics: Option<String>,
    #[serde(rename = "syncedLyrics")]
    synced_lyrics: Option<String>,
}

pub fn parse_get_response(json: &str) -> Option<Lyrics> {
    let r: GetResponse = serde_json::from_str(json).ok()?;
    if let Some(synced) = r.synced_lyrics.filter(|s| !s.trim().is_empty()) {
        let lines = parse_lrc(&synced);
        if !lines.is_empty() {
            return Some(Lyrics {
                provider: ProviderId::Lrclib,
                synced: true,
                rtl: false,
                language: None,
                lines,
            });
        }
    }
    let plain = r.plain_lyrics.filter(|s| !s.trim().is_empty())?;
    let lines = plain
        .lines()
        .map(|t| LyricLine { start_ms: None, text: t.to_string(), translation: None, romanization: None })
        .collect();
    Some(Lyrics { provider: ProviderId::Lrclib, synced: false, rtl: false, language: None, lines })
}

pub struct Lrclib;

impl LyricsProvider for Lrclib {
    fn id(&self) -> ProviderId {
        ProviderId::Lrclib
    }

    fn enabled(&self, cfg: &Config) -> bool {
        let lyrics = cfg.values().lyrics.clone().unwrap_or_default();
        lyrics.enabled.unwrap_or(true)
            && lyrics.provider_order().contains(&ProviderId::Lrclib)
    }

    fn fetch(&self, track: &TrackMeta) -> anyhow::Result<Option<Lyrics>> {
        let client = reqwest::blocking::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(std::time::Duration::from_secs(10))
            .build()?;
        let mut req = client
            .get("https://lrclib.net/api/get")
            .query(&[
                ("track_name", track.title.as_str()),
                ("artist_name", track.artist.as_str()),
                ("duration", &(track.duration_ms / 1000).to_string()),
            ]);
        if let Some(album) = &track.album {
            req = req.query(&[("album_name", album.as_str())]);
        }
        let resp = req.send()?;
        if resp.status().is_success() {
            return Ok(parse_get_response(&resp.text()?));
        }
        Ok(None) // 404 / not found; (search fallback added in a later iteration)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SYNCED: &str = r#"{
        "id": 1, "trackName": "X", "artistName": "Y", "duration": 200,
        "instrumental": false,
        "plainLyrics": "line one\nline two",
        "syncedLyrics": "[00:01.00]line one\n[00:05.00]line two"
    }"#;

    const PLAIN_ONLY: &str = r#"{
        "id": 2, "instrumental": false,
        "plainLyrics": "only plain",
        "syncedLyrics": null
    }"#;

    #[test]
    fn prefers_synced_lyrics() {
        let l = parse_get_response(SYNCED).unwrap();
        assert!(l.synced);
        assert_eq!(l.lines.len(), 2);
        assert_eq!(l.lines[1].start_ms, Some(5000));
        assert_eq!(l.provider, crate::lyrics::model::ProviderId::Lrclib);
    }

    #[test]
    fn falls_back_to_plain() {
        let l = parse_get_response(PLAIN_ONLY).unwrap();
        assert!(!l.synced);
        assert_eq!(l.lines.len(), 1);
        assert_eq!(l.lines[0].text, "only plain");
        assert_eq!(l.lines[0].start_ms, None);
    }
}
