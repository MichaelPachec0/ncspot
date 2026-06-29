use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderId {
    Lrclib,
    Spotify,
    Netease,
    Musixmatch,
}

impl ProviderId {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Lrclib => "lrclib",
            Self::Spotify => "spotify",
            Self::Netease => "netease",
            Self::Musixmatch => "musixmatch",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "lrclib" => Some(Self::Lrclib),
            "spotify" => Some(Self::Spotify),
            "netease" => Some(Self::Netease),
            "musixmatch" => Some(Self::Musixmatch),
            _ => None,
        }
    }

    pub fn display_name(&self) -> &'static str {
        match self {
            Self::Lrclib => "LRCLIB",
            Self::Spotify => "Spotify",
            Self::Netease => "NetEase",
            Self::Musixmatch => "Musixmatch",
        }
    }
}

#[derive(Clone, Debug)]
pub struct TrackMeta {
    pub spotify_id: Option<String>,
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    pub duration_ms: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LyricLine {
    pub start_ms: Option<u32>,
    pub text: String,
    pub translation: Option<String>,
    pub romanization: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lyrics {
    pub provider: ProviderId,
    pub synced: bool,
    pub rtl: bool,
    pub language: Option<String>,
    pub lines: Vec<LyricLine>,
}

impl Lyrics {
    /// Index of the last line whose timestamp is <= pos_ms. None if unsynced
    /// or position precedes the first timestamped line.
    pub fn active_index(&self, pos_ms: u32) -> Option<usize> {
        if !self.synced {
            return None;
        }
        self.lines
            .iter()
            .enumerate()
            .rev()
            .find(|(_, l)| l.start_ms.is_some_and(|s| s <= pos_ms))
            .map(|(i, _)| i)
    }
}

#[derive(Clone, Debug)]
pub enum LyricsState {
    Idle,
    Loading {
        track_id: String,
    },
    Loaded {
        track_id: String,
        lyrics: Lyrics,
    },
    NotFound {
        track_id: String,
        tried: Vec<ProviderId>,
    },
    Error {
        track_id: String,
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(ms: u32, t: &str) -> LyricLine {
        LyricLine {
            start_ms: Some(ms),
            text: t.into(),
            translation: None,
            romanization: None,
        }
    }

    fn synced(lines: Vec<LyricLine>) -> Lyrics {
        Lyrics {
            provider: ProviderId::Lrclib,
            synced: true,
            rtl: false,
            language: None,
            lines,
        }
    }

    #[test]
    fn active_index_picks_last_line_at_or_before_position() {
        let l = synced(vec![line(0, "a"), line(1000, "b"), line(2000, "c")]);
        assert_eq!(l.active_index(0), Some(0));
        assert_eq!(l.active_index(999), Some(0));
        assert_eq!(l.active_index(1000), Some(1));
        assert_eq!(l.active_index(2500), Some(2));
    }

    #[test]
    fn active_index_none_before_first_line() {
        let l = synced(vec![line(500, "a")]);
        assert_eq!(l.active_index(0), None);
        assert_eq!(l.active_index(499), None);
    }

    #[test]
    fn active_index_none_when_unsynced() {
        let mut l = synced(vec![line(0, "a")]);
        l.synced = false;
        assert_eq!(l.active_index(10_000), None);
    }

    #[test]
    fn provider_id_roundtrips() {
        for p in [
            ProviderId::Lrclib,
            ProviderId::Spotify,
            ProviderId::Netease,
            ProviderId::Musixmatch,
        ] {
            assert_eq!(ProviderId::from_str(p.as_str()), Some(p));
        }
        assert_eq!(ProviderId::from_str("nope"), None);
    }
}
