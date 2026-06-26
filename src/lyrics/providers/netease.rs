use serde::Deserialize;

use crate::config::Config;
use crate::lyrics::lrc::parse_lrc;
use crate::lyrics::model::{Lyrics, ProviderId, TrackMeta};
use crate::lyrics::provider::LyricsProvider;
use crate::lyrics::romanize::romanize_line;

#[derive(Deserialize)]
struct SearchResp {
    result: Option<SearchResult>,
}
#[derive(Deserialize)]
struct SearchResult {
    songs: Option<Vec<Song>>,
}
#[derive(Deserialize)]
struct Song {
    id: u64,
    duration: u64,
}

pub fn best_match(json: &str, track: &TrackMeta) -> Option<u64> {
    let r: SearchResp = serde_json::from_str(json).ok()?;
    let songs = r.result?.songs?;
    let target = track.duration_ms as i64;
    songs
        .iter()
        .find(|s| (s.duration as i64 - target).abs() <= 3000)
        .or_else(|| songs.first())
        .map(|s| s.id)
}

#[derive(Deserialize)]
struct LyricResp {
    lrc: Option<LyricBody>,
    tlyric: Option<LyricBody>,
    romalrc: Option<LyricBody>,
}
#[derive(Deserialize)]
struct LyricBody {
    lyric: Option<String>,
}

fn lrc_text(b: &Option<LyricBody>) -> Option<String> {
    b.as_ref()
        .and_then(|x| x.lyric.clone())
        .filter(|s| !s.trim().is_empty())
}

pub fn parse_lyric_response(json: &str, want_romaji: bool, client_romanize: bool) -> Option<Lyrics> {
    let r: LyricResp = serde_json::from_str(json).ok()?;
    let base = lrc_text(&r.lrc)?;
    let mut lines = parse_lrc(&base);
    if lines.is_empty() {
        return None;
    }
    // Merge translation by matching start_ms.
    if let Some(tl) = lrc_text(&r.tlyric) {
        let tmap = parse_lrc(&tl);
        for line in lines.iter_mut() {
            if let Some(m) = tmap.iter().find(|t| t.start_ms == line.start_ms) {
                line.translation = Some(m.text.clone());
            }
        }
    }
    if want_romaji {
        if let Some(rl) = lrc_text(&r.romalrc) {
            let rmap = parse_lrc(&rl);
            for line in lines.iter_mut() {
                if let Some(m) = rmap.iter().find(|t| t.start_ms == line.start_ms) {
                    line.romanization = Some(m.text.clone());
                }
            }
        }
        if client_romanize {
            for line in lines.iter_mut() {
                if line.romanization.is_none() {
                    line.romanization = romanize_line(&line.text);
                }
            }
        }
    }
    Some(Lyrics { provider: ProviderId::Netease, synced: true, rtl: false, language: None, lines })
}

pub struct Netease;

impl LyricsProvider for Netease {
    fn id(&self) -> ProviderId {
        ProviderId::Netease
    }

    fn enabled(&self, cfg: &Config) -> bool {
        let l = cfg.values().lyrics.clone().unwrap_or_default();
        l.enabled.unwrap_or(true) && l.provider_order().contains(&ProviderId::Netease)
    }

    fn fetch(&self, track: &TrackMeta) -> anyhow::Result<Option<Lyrics>> {
        let client = reqwest::blocking::Client::new();
        let search = client
            .get("https://music.163.com/api/search/get")
            .query(&[
                ("s", format!("{} {}", track.title, track.artist)),
                ("type", "1".into()),
                ("limit", "10".into()),
            ])
            .send()?
            .text()?;
        let Some(id) = best_match(&search, track) else {
            return Ok(None);
        };
        let lyric = client
            .get("https://music.163.com/api/song/lyric")
            .query(&[
                ("id", id.to_string()),
                ("lv", "-1".into()),
                ("kv", "-1".into()),
                ("tv", "-1".into()),
            ])
            .send()?
            .text()?;
        // want_romaji / client_romanize from config are passed by the caller in a fuller
        // build; here default to true/true (the manager could thread config later).
        Ok(parse_lyric_response(&lyric, true, true))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lyrics::model::TrackMeta;

    fn track(dur_ms: u32) -> TrackMeta {
        TrackMeta {
            spotify_id: None,
            title: "Hai Kuo Tian Kong".into(),
            artist: "Beyond".into(),
            album: None,
            duration_ms: dur_ms,
        }
    }

    const SEARCH: &str = r#"{"result":{"songs":[
        {"id":111,"name":"X","artists":[{"name":"Z"}],"duration":120000},
        {"id":222,"name":"Y","artists":[{"name":"Beyond"}],"duration":239000}
    ]}}"#;

    #[test]
    fn best_match_uses_duration_tolerance() {
        // 239_560 ms target; candidate 222 is within 3s.
        assert_eq!(best_match(SEARCH, &track(239_560)), Some(222));
    }

    const LYRIC: &str = r#"{
        "lrc": {"lyric": "[00:01.00]今天我\n[00:05.00]怀着冷却"},
        "tlyric": {"lyric": "[00:01.00]Today I\n[00:05.00]With a cooled heart"},
        "romalrc": {"lyric": "[00:01.00]gam tin o\n[00:05.00]waai zoek laang koek"}
    }"#;

    #[test]
    fn merges_translation_and_romaji_by_timestamp() {
        let l = parse_lyric_response(LYRIC, true, false).unwrap();
        assert!(l.synced);
        assert_eq!(l.lines.len(), 2);
        assert_eq!(l.lines[0].translation.as_deref(), Some("Today I"));
        assert_eq!(l.lines[0].romanization.as_deref(), Some("gam tin o"));
    }
}
