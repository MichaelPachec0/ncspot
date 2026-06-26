use serde_json::Value;

use crate::config::Config;
use crate::lyrics::lrc::parse_lrc;
use crate::lyrics::model::{Lyrics, ProviderId, TrackMeta};
use crate::lyrics::provider::LyricsProvider;

const USER_AGENT: &str = concat!(
    "ncspot/",
    env!("CARGO_PKG_VERSION"),
    " (https://github.com/hrkfdn/ncspot)"
);

pub fn parse_macro(json: &str) -> Option<Lyrics> {
    let v: Value = serde_json::from_str(json).ok()?;
    let body = v["message"]["body"]["macro_calls"]["track.subtitles.get"]["message"]["body"]
        ["subtitle_list"][0]["subtitle"]["subtitle_body"]
        .as_str()?;
    let lines = parse_lrc(body);
    if lines.is_empty() {
        return None;
    }
    Some(Lyrics {
        provider: ProviderId::Musixmatch,
        synced: true,
        rtl: false,
        language: None,
        lines,
    })
}

pub struct Musixmatch {
    pub token: String,
}

impl LyricsProvider for Musixmatch {
    fn id(&self) -> ProviderId {
        ProviderId::Musixmatch
    }

    fn enabled(&self, cfg: &Config) -> bool {
        if self.token.is_empty() {
            return false;
        }
        let l = cfg.values().lyrics.clone().unwrap_or_default();
        l.enabled.unwrap_or(true) && l.provider_order().contains(&ProviderId::Musixmatch)
    }

    fn fetch(&self, track: &TrackMeta) -> anyhow::Result<Option<Lyrics>> {
        let duration_sec = (track.duration_ms / 1000).to_string();
        let client = reqwest::blocking::Client::builder()
            .user_agent(USER_AGENT)
            .build()?;
        let mut req = client
            .get("https://apic-desktop.musixmatch.com/ws/1.1/macro.subtitles.get")
            .query(&[
                ("format", "json"),
                ("subtitle_format", "lrc"),
                ("app_id", "web-desktop-app-v1.0"),
                ("q_track", track.title.as_str()),
                ("q_artist", track.artist.as_str()),
                ("q_duration", duration_sec.as_str()),
                ("usertoken", self.token.as_str()),
            ]);
        if let Some(album) = &track.album {
            req = req.query(&[("q_album", album.as_str())]);
        }
        if let Some(id) = &track.spotify_id {
            let spotify_id = format!("spotify:track:{id}");
            req = req.query(&[("track_spotify_id", spotify_id.as_str())]);
        }
        let resp = req.send()?;
        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Ok(None);
        }
        if !status.is_success() {
            return Ok(None);
        }
        let text = resp.text()?;
        // Detect captcha / throttle responses (Musixmatch returns 200 with a
        // captcha page or a JSON error when the token is rate-limited).
        if text.contains("\"captcha\"") || text.contains("\"status_code\":401") {
            return Ok(None);
        }
        Ok(parse_macro(&text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Trimmed shape of macro.subtitles.get with subtitle_format=lrc.
    const MACRO: &str = r#"{"message":{"body":{"macro_calls":{
        "track.subtitles.get":{"message":{"body":{"subtitle_list":[
            {"subtitle":{"subtitle_body":"[00:01.00]hello\n[00:04.00]world"}}
        ]}}}
    }}}}"#;

    #[test]
    fn extracts_synced_subtitle() {
        let l = parse_macro(MACRO).unwrap();
        assert!(l.synced);
        assert_eq!(l.lines.len(), 2);
        assert_eq!(l.lines[1].start_ms, Some(4000));
        assert_eq!(l.provider, crate::lyrics::model::ProviderId::Musixmatch);
    }
}
