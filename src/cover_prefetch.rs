//! Background download of playlist cover images into the cover cache, so the Playlists tab and
//! the cover view find them on disk.

use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use log::debug;

use crate::utils;

/// Minimum pause after each cover download.
pub const COVER_PREFETCH_SPACING: Duration = Duration::from_millis(100);

/// True while a prefetch thread is running, so a new sync does not start a second one.
static RUNNING: AtomicBool = AtomicBool::new(false);

/// The distinct `urls`, in their original order, whose cache file does not exist yet.
pub fn missing_covers(urls: &[String], exists: impl Fn(&Path) -> bool) -> Vec<String> {
    let mut seen = HashSet::new();
    urls.iter()
        .filter(|url| seen.insert(url.as_str()))
        .filter(|url| !exists(&utils::cache_path_for_url((*url).clone())))
        .cloned()
        .collect()
}

/// Download `urls` one at a time on a background thread, pausing [COVER_PREFETCH_SPACING]
/// after each. Failures are logged without the URL and skipped. Does nothing while a previous
/// prefetch is still running.
pub fn spawn_prefetch(urls: Vec<String>) {
    if urls.is_empty() || RUNNING.swap(true, Ordering::AcqRel) {
        return;
    }
    thread::spawn(move || {
        for url in urls {
            let path = utils::cache_path_for_url(url.clone());
            if let Err(e) = utils::download(url, path) {
                debug!("cover prefetch download failed: {:?}", e.kind());
            }
            thread::sleep(COVER_PREFETCH_SPACING);
        }
        RUNNING.store(false, Ordering::Release);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_covers_keeps_only_missing_distinct_in_order() {
        let have: HashSet<_> = [utils::cache_path_for_url("https://x/have".to_string())].into();
        let urls: Vec<String> = [
            "https://x/b",
            "https://x/have",
            "https://x/a",
            "https://x/b",
        ]
        .map(String::from)
        .into();
        let missing = missing_covers(&urls, |p| have.contains(p));
        assert_eq!(missing, vec!["https://x/b", "https://x/a"]);
    }
}
