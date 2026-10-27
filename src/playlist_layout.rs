//! The Playlists tab layout from the rootlist (folders, pinned playlists first), and the
//! spacing between playlist downloads in a sync.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::internal_api::collection::Item;
use crate::internal_api::playlist::{RootEntry, Rootlist};
use crate::library::Library;
use crate::model::album::Album;
use crate::model::artist::Artist;
use crate::model::playlist::Playlist;
use crate::model::track::Track;
use crate::queue::Queue;
use crate::traits::{ListItem, ViewExt};

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

/// One row of the Playlists tab.
#[derive(Clone)]
pub enum PlaylistRow {
    /// A folder name; inert, every item command on it does nothing.
    Header { name: String, depth: usize },
    /// A playlist; `pinned` marks the copies in the pinned block at the top.
    Playlist { playlist: Playlist, pinned: bool },
}

impl PlaylistRow {
    /// The playlist of this row, `None` for a header.
    pub fn playlist(&self) -> Option<&Playlist> {
        match self {
            Self::Playlist { playlist, .. } => Some(playlist),
            Self::Header { .. } => None,
        }
    }

    fn playlist_mut(&mut self) -> Option<&mut Playlist> {
        match self {
            Self::Playlist { playlist, .. } => Some(playlist),
            Self::Header { .. } => None,
        }
    }
}

impl ListItem for PlaylistRow {
    fn is_playing(&self, queue: &Queue) -> bool {
        self.playlist().is_some_and(|p| p.is_playing(queue))
    }

    fn display_left(&self, library: &Library) -> String {
        match self {
            Self::Header { name, depth } => format!("{}{name}/", "  ".repeat(*depth)),
            Self::Playlist { playlist, pinned } => {
                let name = playlist.display_left(library);
                if *pinned { format!("* {name}") } else { name }
            }
        }
    }

    fn display_right(&self, library: &Library) -> String {
        self.playlist()
            .map(|p| p.display_right(library))
            .unwrap_or_default()
    }

    fn play(&mut self, queue: &Queue) {
        if let Some(p) = self.playlist_mut() {
            p.play(queue);
        }
    }

    fn play_next(&mut self, queue: &Queue) {
        if let Some(p) = self.playlist_mut() {
            p.play_next(queue);
        }
    }

    fn queue(&mut self, queue: &Queue) {
        if let Some(p) = self.playlist_mut() {
            p.queue(queue);
        }
    }

    fn toggle_saved(&mut self, library: &Library) {
        if let Some(p) = self.playlist_mut() {
            p.toggle_saved(library);
        }
    }

    fn save(&mut self, library: &Library) {
        if let Some(p) = self.playlist_mut() {
            p.save(library);
        }
    }

    fn unsave(&mut self, library: &Library) {
        if let Some(p) = self.playlist_mut() {
            p.unsave(library);
        }
    }

    fn open(&self, queue: Arc<Queue>, library: Arc<Library>) -> Option<Box<dyn ViewExt>> {
        self.playlist()?.open(queue, library)
    }

    fn open_recommendations(
        &mut self,
        queue: Arc<Queue>,
        library: Arc<Library>,
    ) -> Option<Box<dyn ViewExt>> {
        self.playlist_mut()?.open_recommendations(queue, library)
    }

    fn share_url(&self) -> Option<String> {
        self.playlist()?.share_url()
    }

    fn album(&self, queue: &Queue) -> Option<Album> {
        self.playlist()?.album(queue)
    }

    fn artists(&self) -> Option<Vec<Artist>> {
        self.playlist()?.artists()
    }

    fn track(&self) -> Option<Track> {
        self.playlist()?.track()
    }

    fn is_saved(&self, library: &Library) -> Option<bool> {
        self.playlist()?.is_saved(library)
    }

    fn is_playable(&self) -> bool {
        self.playlist().is_some_and(ListItem::is_playable)
    }

    fn as_listitem(&self) -> Box<dyn ListItem> {
        Box::new(self.clone())
    }
}

/// The Playlists tab rows for the store `playlists` and `layout`. An empty layout gives the
/// flat store list. Otherwise the pinned playlists come first, then the layout with a header
/// per folder (a pinned playlist appears again at its place), then the store playlists the
/// layout does not list. Ids missing from the store are skipped.
pub fn rows(playlists: &[Playlist], layout: &[LayoutEntry]) -> Vec<PlaylistRow> {
    let row = |playlist: &Playlist, pinned| PlaylistRow::Playlist {
        playlist: playlist.clone(),
        pinned,
    };
    if layout.is_empty() {
        return playlists.iter().map(|p| row(p, false)).collect();
    }
    let by_id: HashMap<&str, &Playlist> = playlists.iter().map(|p| (p.id.as_str(), p)).collect();
    let mut out = Vec::new();
    for entry in layout {
        if let LayoutEntry::Pinned(id) = entry
            && let Some(p) = by_id.get(id.as_str())
        {
            out.push(row(p, true));
        }
    }
    let mut listed = HashSet::new();
    for entry in layout {
        match entry {
            LayoutEntry::FolderStart { name, depth } => out.push(PlaylistRow::Header {
                name: name.clone(),
                depth: *depth,
            }),
            LayoutEntry::Playlist(id) => {
                listed.insert(id.as_str());
                if let Some(p) = by_id.get(id.as_str()) {
                    out.push(row(p, false));
                }
            }
            LayoutEntry::Pinned(_) | LayoutEntry::FolderEnd => {}
        }
    }
    out.extend(
        playlists
            .iter()
            .filter(|p| !listed.contains(p.id.as_str()))
            .map(|p| row(p, false)),
    );
    out
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

    fn p(id: &str) -> Playlist {
        Playlist {
            id: id.into(),
            name: String::new(),
            owner_id: String::new(),
            owner_name: None,
            snapshot_id: String::new(),
            num_tracks: 0,
            tracks: None,
            collaborative: false,
            cover_url: None,
        }
    }

    fn describe(rows: &[PlaylistRow]) -> Vec<String> {
        rows.iter()
            .map(|row| match row {
                PlaylistRow::Header { name, depth } => format!("header {name} {depth}"),
                PlaylistRow::Playlist {
                    playlist,
                    pinned: true,
                } => format!("pin {}", playlist.id),
                PlaylistRow::Playlist { playlist, .. } => playlist.id.clone(),
            })
            .collect()
    }

    fn folder(name: &str, depth: usize) -> LayoutEntry {
        LayoutEntry::FolderStart {
            name: name.into(),
            depth,
        }
    }

    #[test]
    fn no_layout_gives_the_flat_store_list() {
        let store = vec![p("p2"), p("p1")];
        assert_eq!(describe(&rows(&store, &[])), vec!["p2", "p1"]);
    }

    #[test]
    fn pinned_rows_come_first_and_again_in_their_folder() {
        let store = vec![p("p1"), p("p2")];
        let layout = vec![
            LayoutEntry::Pinned("p2".into()),
            LayoutEntry::FolderStart {
                name: "A".into(),
                depth: 0,
            },
            LayoutEntry::Playlist("p2".into()),
            LayoutEntry::FolderEnd,
            LayoutEntry::Playlist("p1".into()),
        ];
        let r = rows(&store, &layout);
        assert_eq!(describe(&r), vec!["pin p2", "header A 0", "p2", "p1"]);
    }

    #[test]
    fn nested_folders_keep_their_depth() {
        let store = vec![p("p1"), p("p2")];
        let layout = vec![
            folder("A", 0),
            LayoutEntry::Playlist("p1".into()),
            folder("B", 1),
            LayoutEntry::Playlist("p2".into()),
            LayoutEntry::FolderEnd,
            LayoutEntry::FolderEnd,
        ];
        assert_eq!(
            describe(&rows(&store, &layout)),
            vec!["header A 0", "p1", "header B 1", "p2"]
        );
    }

    #[test]
    fn store_playlists_missing_from_the_layout_are_appended() {
        let store = vec![p("p3"), p("p1"), p("p2")];
        let layout = vec![LayoutEntry::Playlist("p1".into())];
        assert_eq!(describe(&rows(&store, &layout)), vec!["p1", "p3", "p2"]);
    }

    #[test]
    fn ids_missing_from_the_store_are_skipped() {
        let store = vec![p("p1")];
        let layout = vec![
            LayoutEntry::Pinned("gone".into()),
            LayoutEntry::Playlist("gone".into()),
            LayoutEntry::Playlist("p1".into()),
        ];
        assert_eq!(describe(&rows(&store, &layout)), vec!["p1"]);
    }

    #[test]
    fn header_rows_are_inert() {
        let header = PlaylistRow::Header {
            name: "A".into(),
            depth: 1,
        };
        assert!(!header.is_playable());
        assert!(header.share_url().is_none());
        assert!(header.track().is_none());
        assert!(header.artists().is_none());
        assert!(header.playlist().is_none());
        assert!(!header.as_listitem().is_playable());
        assert!(
            PlaylistRow::Playlist {
                playlist: p("p1"),
                pinned: true,
            }
            .is_playable()
        );
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
