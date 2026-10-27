//! The Playlists tab layout from the rootlist (folders, pinned playlists first), and the
//! spacing between playlist downloads in a sync.

use std::time::{Duration, Instant};

use crate::internal_api::collection::Item;
use crate::internal_api::playlist::{RootEntry, Rootlist};

/// Least time between two playlist content downloads in one sync.
pub const PLAYLIST_FETCH_SPACING: Duration = Duration::from_millis(250);

const PLAYLIST_PREFIX: &str = "spotify:playlist:";

/// One entry of the persisted Playlists tab layout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LayoutEntry {
    /// A pinned playlist id; all pins come first, in pin order.
    Pinned(String),
    /// A folder opens; `depth` is the number of folders it is nested in.
    FolderStart {
        name: String,
        depth: usize,
    },
    Playlist(String),
    FolderEnd,
}

/// The layout for `rootlist`: the `pinned` ids first, then the rootlist in order with the
/// nesting depth of each folder.
pub fn layout_from(rootlist: &Rootlist, pinned: &[String]) -> Vec<LayoutEntry> {
    let mut layout: Vec<LayoutEntry> = pinned
        .iter()
        .map(|id| LayoutEntry::Pinned(id.clone()))
        .collect();
    let mut depth = 0usize;
    for entry in &rootlist.entries {
        layout.push(match entry {
            RootEntry::Playlist(p) => LayoutEntry::Playlist(p.id.clone()),
            RootEntry::FolderStart { name } => {
                depth += 1;
                LayoutEntry::FolderStart {
                    name: name.clone(),
                    depth: depth - 1,
                }
            }
            RootEntry::FolderEnd => {
                depth = depth.saturating_sub(1);
                LayoutEntry::FolderEnd
            }
        });
    }
    layout
}

/// The playlist ids among the `ylpin` items, in set order. Other pins (Liked Songs, Your
/// Episodes) are left out.
pub fn pinned_ids(items: &[Item]) -> Vec<String> {
    items
        .iter()
        .filter_map(|item| item.uri.strip_prefix(PLAYLIST_PREFIX))
        .map(str::to_string)
        .collect()
}

/// Keeps calls at least `gap` apart.
pub struct Spacer {
    gap: Duration,
    last: Option<Instant>,
}

impl Spacer {
    pub fn new(gap: Duration) -> Self {
        Self { gap, last: None }
    }

    /// How long to wait before the next call at `now`; records `now + wait` as the call time.
    pub fn next_wait(&mut self, now: Instant) -> Duration {
        let wait = self.last.map_or(Duration::ZERO, |last| {
            (last + self.gap).saturating_duration_since(now)
        });
        self.last = Some(now + wait);
        wait
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::internal_api::playlist::RootPlaylist;

    fn pl(id: &str) -> RootEntry {
        RootEntry::Playlist(RootPlaylist {
            id: id.into(),
            name: String::new(),
            snapshot_id: String::new(),
            length: 0,
            owner_id: String::new(),
            collaborative: false,
            cover_url: None,
            status: 200,
        })
    }

    #[test]
    fn layout_tracks_folder_depth_and_pins_first() {
        let root = Rootlist {
            length: 6,
            entries: vec![
                RootEntry::FolderStart { name: "A".into() },
                pl("p1"),
                RootEntry::FolderStart { name: "B".into() },
                pl("p2"),
                RootEntry::FolderEnd,
                RootEntry::FolderEnd,
            ],
            ..Default::default()
        };
        assert_eq!(
            layout_from(&root, &["p2".into()]),
            vec![
                LayoutEntry::Pinned("p2".into()),
                LayoutEntry::FolderStart {
                    name: "A".into(),
                    depth: 0
                },
                LayoutEntry::Playlist("p1".into()),
                LayoutEntry::FolderStart {
                    name: "B".into(),
                    depth: 1
                },
                LayoutEntry::Playlist("p2".into()),
                LayoutEntry::FolderEnd,
                LayoutEntry::FolderEnd,
            ]
        );
    }

    #[test]
    fn pinned_ids_keep_only_playlists() {
        use crate::internal_api::collection::Item;
        let it = |uri: &str| Item {
            uri: uri.into(),
            added_at: 0,
        };
        let items = [
            it("spotify:collection:tracks"),
            it("spotify:playlist:p2"),
            it("spotify:collection:your-episodes"),
            it("spotify:playlist:p1"),
        ];
        assert_eq!(pinned_ids(&items), vec!["p2".to_string(), "p1".to_string()]);
    }

    #[test]
    fn spacer_waits_out_the_gap() {
        let t0 = Instant::now();
        let mut s = Spacer::new(Duration::from_millis(250));
        assert_eq!(s.next_wait(t0), Duration::ZERO);
        assert_eq!(
            s.next_wait(t0 + Duration::from_millis(100)),
            Duration::from_millis(150)
        );
        assert_eq!(s.next_wait(t0 + Duration::from_secs(5)), Duration::ZERO);
    }
}
