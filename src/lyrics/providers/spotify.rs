use std::sync::Arc;

use librespot_core::SpotifyId;
use librespot_metadata::Lyrics as LibrespotLyrics;
use librespot_metadata::lyrics::SyncType;

use crate::config::Config;
use crate::lyrics::model::{LyricLine, Lyrics, ProviderId, TrackMeta};
use crate::lyrics::provider::LyricsProvider;
use crate::spotify::Spotify;

pub fn from_librespot(l: &LibrespotLyrics) -> Lyrics {
    let inner = &l.lyrics;
    let synced = matches!(inner.sync_type, SyncType::LineSynced);
    let lines = inner
        .lines
        .iter()
        .map(|line| LyricLine {
            start_ms: if synced { line.start_time_ms.parse::<u32>().ok() } else { None },
            text: line.words.clone(),
            translation: None,
            romanization: None,
        })
        .collect();
    Lyrics {
        provider: ProviderId::Spotify,
        synced,
        rtl: inner.is_rtl_language,
        language: Some(inner.language.clone()),
        lines,
    }
}

pub struct SpotifyProvider {
    pub spotify: Arc<Spotify>,
}

impl LyricsProvider for SpotifyProvider {
    fn id(&self) -> ProviderId {
        ProviderId::Spotify
    }

    fn enabled(&self, _cfg: &Config) -> bool {
        self.spotify.session().is_some()
    }

    fn fetch(&self, track: &TrackMeta) -> anyhow::Result<Option<Lyrics>> {
        let Some(id_str) = &track.spotify_id else { return Ok(None) };
        let Some(session) = self.spotify.session() else { return Ok(None) };
        let track_id = SpotifyId::from_base62(id_str)?;
        let fetched = crate::application::ASYNC_RUNTIME.get().unwrap().block_on(async {
            tokio::time::timeout(
                std::time::Duration::from_secs(10),
                LibrespotLyrics::get(&session, &track_id),
            )
            .await
        });
        match fetched {
            Ok(Ok(lib)) => Ok(Some(from_librespot(&lib))),
            Ok(Err(e)) => {
                log::debug!("spotify lyrics fetch failed for {id_str}: {e}");
                Ok(None)
            }
            Err(_elapsed) => {
                log::debug!("spotify lyrics fetch timed out for {id_str}");
                Ok(None)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_line_synced_lyrics() {
        // Build a librespot Lyrics value via its Deserialize from a JSON fixture.
        let json = r#"{
            "colors": {"background": 0, "highlightText": 0, "text": 0},
            "hasVocalRemoval": false,
            "lyrics": {
                "isDenseTypeface": false, "isRtlLanguage": false, "language": "en",
                "provider": "musixmatch", "providerDisplayName": "Musixmatch",
                "providerLyricsId": "1", "syncLyricsUri": "", "syncType": "LINE_SYNCED",
                "lines": [
                    {"startTimeMs": "1000", "endTimeMs": "0", "words": "hello"},
                    {"startTimeMs": "2000", "endTimeMs": "0", "words": "world"}
                ]
            }
        }"#;
        let lib: librespot_metadata::Lyrics = serde_json::from_str(json).unwrap();
        let mapped = from_librespot(&lib);
        assert!(mapped.synced);
        assert_eq!(mapped.provider, crate::lyrics::model::ProviderId::Spotify);
        assert_eq!(mapped.lines.len(), 2);
        assert_eq!(mapped.lines[0].start_ms, Some(1000));
        assert_eq!(mapped.lines[1].text, "world");
    }
}
