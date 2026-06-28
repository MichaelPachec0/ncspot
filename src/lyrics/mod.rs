pub mod cache;
pub mod lrc;
pub mod model;
pub mod provider;
pub mod providers;
pub mod romanize;

use crate::config::Config;
use crate::lyrics::cache::LyricsCache;
use crate::lyrics::model::{Lyrics, ProviderId, TrackMeta};
use crate::lyrics::provider::LyricsProvider;

pub enum FetchOutcome {
    Found(Lyrics),
    NotFound { tried: Vec<ProviderId> },
    /// At least one provider errored and no lyrics were found, so we can't say
    /// for sure whether lyrics exist.
    Error { message: String },
}

pub struct LyricsManager {
    providers: Vec<Box<dyn LyricsProvider>>,
    cache: LyricsCache,
}

impl LyricsManager {
    pub fn new(providers: Vec<Box<dyn LyricsProvider>>, cache: LyricsCache) -> Self {
        Self { providers, cache }
    }

    fn cache_key(track: &TrackMeta) -> String {
        let raw = track
            .spotify_id
            .clone()
            .unwrap_or_else(|| format!("{}|{}", track.title, track.artist));
        // Sanitize: replace path separators so keys cannot escape the cache dir.
        raw.replace(['/', '\\'], "_")
    }

    pub fn invalidate(&self, track: &TrackMeta) {
        self.cache.remove(&Self::cache_key(track));
    }

    pub fn fetch(&self, cfg: &Config, order: &[ProviderId], track: &TrackMeta) -> FetchOutcome {
        let key = Self::cache_key(track);
        if let Some(hit) = self.cache.get(&key) {
            return FetchOutcome::Found(hit);
        }
        let mut tried = Vec::new();
        let mut errors = Vec::new();
        for id in order {
            let Some(p) = self.providers.iter().find(|p| p.id() == *id) else {
                continue;
            };
            if !p.enabled(cfg) {
                continue;
            }
            tried.push(*id);
            match p.fetch(track) {
                Ok(Some(lyrics)) => {
                    self.cache.put(&key, &lyrics);
                    return FetchOutcome::Found(lyrics);
                }
                Ok(None) => {}
                Err(e) => {
                    log::warn!("lyrics provider {} failed: {e}", id.as_str());
                    errors.push(format!("{}: {e}", id.as_str()));
                }
            }
        }
        // Distinguish "searched cleanly, nothing exists" from "providers errored":
        // only the former is a true NotFound.
        if errors.is_empty() {
            FetchOutcome::NotFound { tried }
        } else {
            FetchOutcome::Error {
                message: errors.join("; "),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lyrics::model::{LyricLine, Lyrics, ProviderId, TrackMeta};
    use crate::lyrics::provider::LyricsProvider;

    struct Mock {
        id: ProviderId,
        result: Option<Lyrics>,
    }
    impl LyricsProvider for Mock {
        fn id(&self) -> ProviderId {
            self.id
        }
        fn enabled(&self, _c: &crate::config::Config) -> bool {
            true
        }
        fn fetch(&self, _t: &TrackMeta) -> anyhow::Result<Option<Lyrics>> {
            Ok(self.result.clone())
        }
    }

    /// Provider that always errors, for exercising the error path.
    struct ErrMock {
        id: ProviderId,
    }
    impl LyricsProvider for ErrMock {
        fn id(&self) -> ProviderId {
            self.id
        }
        fn enabled(&self, _c: &crate::config::Config) -> bool {
            true
        }
        fn fetch(&self, _t: &TrackMeta) -> anyhow::Result<Option<Lyrics>> {
            Err(anyhow::anyhow!("boom"))
        }
    }

    fn ly(p: ProviderId) -> Lyrics {
        Lyrics {
            provider: p,
            synced: true,
            rtl: false,
            language: None,
            lines: vec![LyricLine {
                start_ms: Some(0),
                text: "x".into(),
                translation: None,
                romanization: None,
            }],
        }
    }
    fn track() -> TrackMeta {
        TrackMeta {
            spotify_id: Some("id1".into()),
            title: "t".into(),
            artist: "a".into(),
            album: None,
            duration_ms: 1000,
        }
    }

    #[test]
    fn returns_first_non_empty_in_order() {
        let cfg = crate::config::Config::new_for_test();
        let cache = LyricsCache::new(
            std::env::temp_dir().join(format!("lyr-mgr-{}-1", std::process::id())),
        );
        let mgr = LyricsManager::new(
            vec![
                Box::new(Mock { id: ProviderId::Lrclib, result: None }),
                Box::new(Mock {
                    id: ProviderId::Netease,
                    result: Some(ly(ProviderId::Netease)),
                }),
            ],
            cache,
        );
        let order = [ProviderId::Lrclib, ProviderId::Netease];
        match mgr.fetch(cfg.as_ref(), &order, &track()) {
            FetchOutcome::Found(l) => assert_eq!(l.provider, ProviderId::Netease),
            _ => panic!("expected Found"),
        }
    }

    #[test]
    fn not_found_lists_tried() {
        let cfg = crate::config::Config::new_for_test();
        let cache = LyricsCache::new(
            std::env::temp_dir().join(format!("lyr-mgr-{}-2", std::process::id())),
        );
        let mgr = LyricsManager::new(
            vec![Box::new(Mock { id: ProviderId::Lrclib, result: None })],
            cache,
        );
        match mgr.fetch(cfg.as_ref(), &[ProviderId::Lrclib], &track()) {
            FetchOutcome::NotFound { tried } => assert_eq!(tried, vec![ProviderId::Lrclib]),
            _ => panic!("expected NotFound"),
        }
    }

    #[test]
    fn provider_errors_surface_as_error() {
        let cfg = crate::config::Config::new_for_test();
        let cache = LyricsCache::new(
            std::env::temp_dir().join(format!("lyr-mgr-{}-3", std::process::id())),
        );
        let mgr = LyricsManager::new(vec![Box::new(ErrMock { id: ProviderId::Lrclib })], cache);
        match mgr.fetch(cfg.as_ref(), &[ProviderId::Lrclib], &track()) {
            FetchOutcome::Error { message } => assert!(message.contains("lrclib")),
            _ => panic!("expected Error"),
        }
    }
}
