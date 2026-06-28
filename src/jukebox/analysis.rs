use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::anyhow;
use http::Method;
use librespot_core::SpotifyId;

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
        let endpoint = format!("/audio-attributes/v1/audio-analysis/{track_id}?format=json");

        let bytes = ASYNC_RUNTIME
            .get()
            .unwrap()
            .block_on(async {
                tokio::time::timeout(
                    Duration::from_secs(10),
                    session
                        .spclient()
                        .request_as_json(&Method::GET, &endpoint, None, None),
                )
                .await
            })
            .map_err(|_| anyhow!("spclient audio-analysis timed out"))?
            .map_err(|e| anyhow!("spclient audio-analysis failed: {e}"))?;

        let analysis: AudioAnalysis = serde_json::from_slice(&bytes)?;
        if analysis.beats.is_empty() {
            return Ok(None);
        }
        Ok(Some(analysis))
    }
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
}
