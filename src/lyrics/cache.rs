use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::RwLock;

use crate::lyrics::model::Lyrics;

pub struct LyricsCache {
    dir: PathBuf,
    mem: RwLock<HashMap<String, Lyrics>>,
}

impl LyricsCache {
    pub fn new(dir: PathBuf) -> Self {
        let _ = std::fs::create_dir_all(&dir);
        Self { dir, mem: RwLock::new(HashMap::new()) }
    }

    fn path(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.json"))
    }

    pub fn get(&self, key: &str) -> Option<Lyrics> {
        if let Some(hit) = self.mem.read().unwrap().get(key).cloned() {
            return Some(hit);
        }
        let bytes = std::fs::read(self.path(key)).ok()?;
        let lyrics: Lyrics = serde_json::from_slice(&bytes).ok()?;
        self.mem.write().unwrap().insert(key.to_string(), lyrics.clone());
        Some(lyrics)
    }

    pub fn put(&self, key: &str, lyrics: &Lyrics) {
        self.mem.write().unwrap().insert(key.to_string(), lyrics.clone());
        if let Ok(json) = serde_json::to_vec(lyrics) {
            let _ = std::fs::write(self.path(key), json);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lyrics::model::{Lyrics, LyricLine, ProviderId};

    fn sample() -> Lyrics {
        Lyrics {
            provider: ProviderId::Lrclib,
            synced: true,
            rtl: false,
            language: Some("en".into()),
            lines: vec![LyricLine { start_ms: Some(0), text: "hi".into(), translation: None, romanization: None }],
        }
    }

    #[test]
    fn put_then_get_from_disk_after_dropping_memory() {
        let dir = std::env::temp_dir().join(format!("ncspot-lyrics-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // First cache instance writes to disk.
        {
            let c = LyricsCache::new(dir.clone());
            c.put("abc", &sample());
        }
        // Fresh instance (empty memory) must read it back from disk.
        let c2 = LyricsCache::new(dir.clone());
        assert_eq!(c2.get("abc"), Some(sample()));
        assert_eq!(c2.get("missing"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
