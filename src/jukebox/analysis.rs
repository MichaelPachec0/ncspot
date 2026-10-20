use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::anyhow;
use http::Method;
use librespot_core::error::ErrorKind;
use librespot_core::{Session, SpotifyId, SpotifyUri};
use librespot_metadata::Metadata;

use crate::application::ASYNC_RUNTIME;
use crate::jukebox::model::AudioAnalysis;
use crate::spotify::Spotify;

/// On-disk + in-memory cache of immutable analyses, keyed by track id. Mirrors LyricsCache.
pub struct AnalysisCache {
    dir: PathBuf,
    mem: RwLock<HashMap<String, AudioAnalysis>>,
}

impl AnalysisCache {
    pub fn new(dir: PathBuf) -> Self {
        let _ = std::fs::create_dir_all(&dir);
        Self {
            dir,
            mem: RwLock::new(HashMap::new()),
        }
    }

    fn path(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.json"))
    }

    pub fn get(&self, key: &str) -> Option<AudioAnalysis> {
        if let Some(hit) = self.mem.read().unwrap().get(key).cloned() {
            return Some(hit);
        }
        let bytes = std::fs::read(self.path(key)).ok()?;
        let a: AudioAnalysis = serde_json::from_slice(&bytes).ok()?;
        self.mem.write().unwrap().insert(key.to_string(), a.clone());
        Some(a)
    }

    pub fn put(&self, key: &str, analysis: &AudioAnalysis) {
        self.mem
            .write()
            .unwrap()
            .insert(key.to_string(), analysis.clone());
        if let Ok(json) = serde_json::to_vec(analysis) {
            let _ = std::fs::write(self.path(key), json);
        }
    }
}

pub trait AnalysisSource: Send + Sync {
    fn id(&self) -> &str;
    fn enabled(&self) -> bool;
    fn fetch(&self, track_id: &str) -> anyhow::Result<Option<AudioAnalysis>>;
}

/// Primary source: Spotify's internal spclient endpoint, reached with the official-client
/// token via librespot. Same credential path as SpClient::get_lyrics.
pub struct SpclientSource {
    pub spotify: Arc<Spotify>,
}

impl AnalysisSource for SpclientSource {
    fn id(&self) -> &str {
        "spclient"
    }

    fn enabled(&self) -> bool {
        self.spotify.session().is_some()
    }

    fn fetch(&self, track_id: &str) -> anyhow::Result<Option<AudioAnalysis>> {
        let Some(session) = self.spotify.session() else {
            return Ok(None);
        };
        // Validate the id; the endpoint takes the base62 track id directly.
        SpotifyId::from_base62(track_id).map_err(|e| anyhow!("invalid track id: {e:?}"))?;

        let Some(bytes) = fetch_relinked(
            track_id,
            |id| request_analysis(&session, id),
            || alternatives(&session, track_id),
        )?
        else {
            return Ok(None);
        };
        let analysis: AudioAnalysis = serde_json::from_slice(&bytes)?;
        if analysis.beats.is_empty() {
            return Ok(None);
        }
        Ok(Some(analysis))
    }
}

/// Run `fut` on the async runtime, failing after 10 seconds.
fn block_on_timeout<T>(
    what: &str,
    fut: impl std::future::Future<Output = Result<T, librespot_core::Error>>,
) -> anyhow::Result<Result<T, librespot_core::Error>> {
    ASYNC_RUNTIME
        .get()
        .unwrap()
        .block_on(async { tokio::time::timeout(Duration::from_secs(10), fut).await })
        .map_err(|_| anyhow!("spclient {what} timed out"))
}

/// The analysis of the track with base62 id `id`, or `None` if Spotify has none under it.
fn request_analysis(session: &Session, id: &str) -> anyhow::Result<Option<Vec<u8>>> {
    let endpoint = format!("/audio-attributes/v1/audio-analysis/{id}?format=json");
    match block_on_timeout(
        "audio-analysis",
        session
            .spclient()
            .request_as_json(&Method::GET, &endpoint, None, None),
    )? {
        Ok(bytes) => Ok(Some(bytes.to_vec())),
        Err(e) if e.kind == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow!("spclient audio-analysis failed: {e}")),
    }
}

/// Base62 ids of the alternatives of track `track_id`, the tracks playback is relinked to when
/// the track itself is not available in the user's market.
fn alternatives(session: &Session, track_id: &str) -> anyhow::Result<Vec<String>> {
    let uri = SpotifyUri::from_uri(&format!("spotify:track:{track_id}"))
        .map_err(|e| anyhow!("invalid track id: {e:?}"))?;
    let track = block_on_timeout(
        "track metadata",
        librespot_metadata::Track::get(session, &uri),
    )?
    .map_err(|e| anyhow!("track metadata failed: {e}"))?;
    Ok(track.alternatives.iter().map(SpotifyUri::to_id).collect())
}

/// Fetch the analysis of `track_id` with `request`, and when there is none under that id, of
/// each of its `alternatives` in order. A track relinked in the user's market (no audio files
/// of its own, played through an alternative) has its analysis only under the alternative's
/// id. An error stops the lookup.
fn fetch_relinked(
    track_id: &str,
    request: impl Fn(&str) -> anyhow::Result<Option<Vec<u8>>>,
    alternatives: impl FnOnce() -> anyhow::Result<Vec<String>>,
) -> anyhow::Result<Option<Vec<u8>>> {
    if let Some(body) = request(track_id)? {
        return Ok(Some(body));
    }
    for alt in alternatives()? {
        if let Some(body) = request(&alt)? {
            log::debug!("audio analysis of {track_id} found under its alternative {alt}");
            return Ok(Some(body));
        }
    }
    Ok(None)
}

/// Eternalbox wraps the Spotify analysis under an `analysis` key. The inner object's field
/// names match `AudioAnalysis`, so it deserializes directly.
#[derive(serde::Deserialize)]
struct EternalboxResponse {
    analysis: AudioAnalysis,
}

/// Fallback source: a public eternalbox instance (configurable). Serves only already-cached
/// analyses (non-200 for un-analyzed tracks -> treated as "no analysis").
pub struct EternalboxSource {
    pub base_url: String,
}

impl AnalysisSource for EternalboxSource {
    fn id(&self) -> &str {
        "eternalbox"
    }

    fn enabled(&self) -> bool {
        true
    }

    fn fetch(&self, track_id: &str) -> anyhow::Result<Option<AudioAnalysis>> {
        let url = format!(
            "{}/api/analysis/analyse/{track_id}",
            self.base_url.trim_end_matches('/')
        );
        let resp = reqwest::blocking::Client::new()
            .get(&url)
            .timeout(Duration::from_secs(10))
            .send()?;
        if !resp.status().is_success() {
            return Ok(None);
        }
        let parsed: EternalboxResponse = resp.json()?;
        if parsed.analysis.beats.is_empty() {
            return Ok(None);
        }
        Ok(Some(parsed.analysis))
    }
}

pub struct AnalysisManager {
    sources: Vec<Box<dyn AnalysisSource>>,
    cache: AnalysisCache,
}

impl AnalysisManager {
    pub fn new(sources: Vec<Box<dyn AnalysisSource>>, cache: AnalysisCache) -> Self {
        Self { sources, cache }
    }

    fn cache_key(track_id: &str) -> String {
        track_id.replace(['/', '\\'], "_")
    }

    /// Returns the first analysis produced by an enabled source in `order`, caching it.
    pub fn fetch(&self, order: &[String], track_id: &str) -> Option<AudioAnalysis> {
        let key = Self::cache_key(track_id);
        if let Some(hit) = self.cache.get(&key) {
            return Some(hit);
        }
        for id in order {
            let Some(src) = self.sources.iter().find(|s| s.id() == id) else {
                continue;
            };
            if !src.enabled() {
                continue;
            }
            match src.fetch(track_id) {
                Ok(Some(a)) => {
                    self.cache.put(&key, &a);
                    return Some(a);
                }
                Ok(None) => {}
                Err(e) => log::warn!("analysis source {id} failed: {e}"),
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jukebox::model::{AudioAnalysis, TimeInterval};

    fn sample() -> AudioAnalysis {
        AudioAnalysis {
            bars: vec![],
            beats: vec![TimeInterval {
                start: 0.0,
                duration: 1.0,
                confidence: 1.0,
            }],
            tatums: vec![],
            sections: vec![],
            segments: vec![],
        }
    }

    struct Mock {
        id: &'static str,
        result: Option<AudioAnalysis>,
    }
    impl AnalysisSource for Mock {
        fn id(&self) -> &str {
            self.id
        }
        fn enabled(&self) -> bool {
            true
        }
        fn fetch(&self, _t: &str) -> anyhow::Result<Option<AudioAnalysis>> {
            Ok(self.result.clone())
        }
    }

    #[test]
    fn cache_round_trips_from_disk() {
        let dir = std::env::temp_dir().join(format!("ncspot-jukebox-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        {
            let c = AnalysisCache::new(dir.clone());
            c.put("abc", &sample());
        }
        let c2 = AnalysisCache::new(dir.clone());
        assert_eq!(c2.get("abc").unwrap().beats.len(), 1);
        assert!(c2.get("missing").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn eternalbox_wrapped_payload_parses() {
        let json = r#"{
            "analysis": {
                "beats": [{"start": 0.0, "duration": 0.5, "confidence": 0.9}],
                "segments": [{
                    "start": 0.0, "duration": 0.3, "confidence": 0.6,
                    "loudness_start": -20.0, "loudness_max": -5.0,
                    "loudness_max_time": 0.1, "loudness_end": 0.0,
                    "pitches": [0.1], "timbre": [1.0]
                }]
            }
        }"#;
        let parsed: EternalboxResponse = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.analysis.beats.len(), 1);
        assert_eq!(parsed.analysis.segments[0].timbre, vec![1.0]);
    }

    #[test]
    fn manager_returns_first_source_in_order() {
        let dir = std::env::temp_dir().join(format!("ncspot-jb-mgr-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mgr = AnalysisManager::new(
            vec![
                Box::new(Mock {
                    id: "spclient",
                    result: None,
                }),
                Box::new(Mock {
                    id: "eternalbox",
                    result: Some(sample()),
                }),
            ],
            AnalysisCache::new(dir.clone()),
        );
        let order = vec!["spclient".to_string(), "eternalbox".to_string()];
        assert!(mgr.fetch(&order, "trackid1").is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    use std::cell::RefCell;

    #[test]
    fn relinked_lookup_uses_the_track_itself_when_it_has_an_analysis() {
        let body = fetch_relinked(
            "main",
            |id| Ok((id == "main").then(|| b"main".to_vec())),
            || panic!("alternatives must not be looked up"),
        )
        .unwrap();
        assert_eq!(body, Some(b"main".to_vec()));
    }

    #[test]
    fn relinked_lookup_tries_alternatives_in_order() {
        let asked = RefCell::new(Vec::new());
        let body = fetch_relinked(
            "main",
            |id| {
                asked.borrow_mut().push(id.to_string());
                Ok((id == "alt2").then(|| b"alt2".to_vec()))
            },
            || {
                Ok(vec![
                    "alt1".to_string(),
                    "alt2".to_string(),
                    "alt3".to_string(),
                ])
            },
        )
        .unwrap();
        assert_eq!(body, Some(b"alt2".to_vec()));
        assert_eq!(*asked.borrow(), vec!["main", "alt1", "alt2"]);
    }

    #[test]
    fn relinked_lookup_without_alternatives_is_missing() {
        let body = fetch_relinked("main", |_| Ok(None), || Ok(vec![])).unwrap();
        assert_eq!(body, None);
    }

    #[test]
    fn relinked_lookup_does_not_try_alternatives_after_an_error() {
        let result = fetch_relinked(
            "main",
            |_| Err(anyhow!("timed out")),
            || panic!("alternatives must not be looked up"),
        );
        assert!(result.is_err());
    }
}
