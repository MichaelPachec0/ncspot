use crate::config::Config;
use crate::lyrics::model::{Lyrics, ProviderId, TrackMeta};

pub trait LyricsProvider: Send + Sync {
    fn id(&self) -> ProviderId;
    fn enabled(&self, cfg: &Config) -> bool;
    /// Ok(None) = reachable but no match; Err = transient error (chain falls through).
    fn fetch(&self, track: &TrackMeta) -> anyhow::Result<Option<Lyrics>>;
}
