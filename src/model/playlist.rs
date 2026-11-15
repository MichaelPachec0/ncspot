use parking_lot::RwLock;
use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::{cmp::Ordering, iter::Iterator};

use rand::{rng, seq::IteratorRandom};

use log::{debug, warn};
use rspotify::model::Id;
use rspotify::model::playlist::{FullPlaylist, SimplifiedPlaylist};

use crate::internal_api::Area;
use crate::internal_api::fallback::{Action, decide_after, run_with_fallback};
use crate::internal_api::playlist_write::{move_target, moved_position};
use crate::library::{edit_failed, write_through};
use crate::model::playable::Playable;
use crate::model::track::Track;
use crate::playlist_edit::{self, EditError};
use crate::queue::Queue;
use crate::spotify::Spotify;
use crate::traits::{IntoBoxedViewExt, ListItem, ViewExt};
use crate::ui::{listview::ListView, playlist::PlaylistView};
use crate::{command::SortDirection, command::SortKey, library::Library};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Playlist {
    pub id: String,
    pub name: String,
    pub owner_id: String,
    pub owner_name: Option<String>,
    pub snapshot_id: String,
    pub num_tracks: usize,
    pub tracks: Option<Vec<Playable>>,
    pub collaborative: bool,
    /// Cover image URL (Spotify mosaic/custom), if the playlist has one.
    #[serde(default)]
    pub cover_url: Option<String>,
    /// The track count is not known: a search answer does not carry it. The row shows no
    /// count until the tracks are loaded.
    #[serde(default)]
    pub tracks_unknown: bool,
    /// The tracks came from the Web API, which gives no snapshot, onto a copy that had none
    /// (a search row). Such a copy is not read again on every use; a write that clears the
    /// snapshot ends this. Never saved.
    #[serde(skip)]
    pub web_loaded: bool,
}

impl Playlist {
    /// Load the tracks if they are not loaded yet; a loaded copy whose snapshot was cleared is
    /// reloaded instead when the internal API can (see [playlist_edit::reload_cleared]).
    /// Returns true if the tracks are loaded; on a failed request `tracks` is left untouched.
    pub fn load_tracks(&mut self, spotify: &Spotify) -> bool {
        if self.tracks.is_some() {
            playlist_edit::reload_cleared(&spotify.api, self);
            return true;
        }

        let loaded = self.load_all_tracks(spotify);
        self.store_loaded(loaded)
    }

    /// Forget the snapshot, so the copy is read again when the internal API can (see
    /// [playlist_edit::reload_cleared]).
    pub fn clear_snapshot(&mut self) {
        self.snapshot_id.clear();
        self.web_loaded = false;
    }

    /// Keep the tracks of a load (`None` = it failed, nothing changes) and the snapshot they
    /// were read at. A copy without a snapshot (a search row, a cleared copy) takes the load's,
    /// so it is not reloaded on every later use; a copy with one keeps it. Returns true if the
    /// tracks are loaded.
    fn store_loaded(&mut self, loaded: Option<(Vec<Playable>, String)>) -> bool {
        let Some((tracks, snapshot_id)) = loaded else {
            return false;
        };
        if self.snapshot_id.is_empty() {
            self.web_loaded = snapshot_id.is_empty();
            self.snapshot_id = snapshot_id;
        }
        self.tracks = Some(tracks);
        self.tracks_unknown = false;
        true
    }

    /// The internal API first when the playlists area is usable, then the Web API per the
    /// fallback classes (rate limits and transient errors fail without a Web call). With the
    /// tracks comes the snapshot they were read at: the internal read gives the revision of its
    /// head (empty when the head carried none), the Web read gives none (empty).
    fn load_all_tracks(&self, spotify: &Spotify) -> Option<(Vec<Playable>, String)> {
        let api = &spotify.api;
        let skip = !api.internal_usable(Area::Playlists);
        run_with_fallback(
            skip,
            || {
                api.playlist_playables_at(&self.id, &HashMap::new())
                    .map(|(head, tracks)| (tracks, head.snapshot_id))
                    .inspect_err(|err| {
                        if decide_after(err) == Action::Fail {
                            warn!("Could not load a playlist ({})", err.class());
                        }
                    })
            },
            || {
                if !skip {
                    api.count_fallback(Area::Playlists);
                }
                self.get_all_tracks(spotify)
                    .map(|tracks| (tracks, String::new()))
            },
            |err| api.latch_area(Area::Playlists, err),
        )
    }

    fn get_all_tracks(&self, spotify: &Spotify) -> Option<Vec<Playable>> {
        let tracks_result = spotify.api.user_playlist_tracks(&self.id);
        tracks_result.fetch_all();

        if tracks_result.failed() {
            return None;
        }
        let tracks = tracks_result.items.read();
        Some(tracks.clone())
    }

    /// The count column of the row: blank, as wide as "1234 tracks", while the count is
    /// unknown, so the rows stay aligned.
    fn count_text(&self) -> String {
        if self.tracks_unknown && self.tracks.is_none() {
            return " ".repeat(11);
        }
        let num_tracks = self.tracks.as_ref().map_or(self.num_tracks, Vec::len);
        format!("{num_tracks:>4} tracks")
    }

    pub fn has_track(&self, track_id: &str) -> bool {
        self.tracks.as_ref().is_some_and(|tracks| {
            tracks
                .iter()
                .any(|track| track.id() == Some(track_id.to_string()))
        })
    }

    /// Remove the track `row` (a row of a view of this playlist): the item at its server
    /// position (`list_index`, not its place in a sorted view) that is also its uri. On failure
    /// the error is the status note to show.
    pub fn delete_track(
        &mut self,
        row: &Playable,
        spotify: Spotify,
        library: &Library,
    ) -> Result<(), String> {
        let index = row.list_index();
        let uri = row.uri();
        let Some(playable) = self.tracks.as_ref().and_then(|tracks| {
            playlist_edit::server_item(tracks, index, &uri).map(|at| tracks[at].clone())
        }) else {
            return Err("Could not delete track.".to_string());
        };
        debug!("deleting track: {index} {playable:?}");

        if playable.track().map(|t| t.is_local) == Some(true) {
            warn!("track is a local file, can't delete");
            return Err("Could not delete track.".to_string());
        }

        let api = &spotify.api;
        let known = || library.known_playables();
        let note = Cell::new(None);
        let web_used = Cell::new(false);
        let id = self.id.clone();
        let snapshot_id = self.snapshot_id.clone();
        let written = write_through(
            api,
            Area::Playlists,
            || {
                playlist_edit::remove_track(api, self, index, &uri, &known).map_err(|err| {
                    // Also a copy reloaded before the failure (its snapshot changed).
                    if err.stores_copy() || self.snapshot_id != snapshot_id {
                        library.playlist_update(self);
                    }
                    note.set(err.note());
                    err.into()
                })
            },
            || {
                web_used.set(true);
                api.delete_tracks(&id, &snapshot_id, &[playable]).is_ok()
            },
        );
        if !written {
            if let Some(note) = note.take() {
                warn!("{note}");
                return Err(note);
            }
            return Err("Could not delete track.".to_string());
        }

        if web_used.get() {
            playlist_edit::remove_after_web(self, index, &uri);
        }
        library.playlist_update(self);
        Ok(())
    }

    /// Move the track `row` (a row of this playlist, which is in server order) up or down by
    /// `n` rows, clamped to the first or last row: the internal API first (one verified MOV),
    /// the Web API per the fallback classes. Returns the track's new server position, or
    /// `None` when the move failed (its one note is posted). Nothing is sent when the track is
    /// already at that end.
    pub fn move_track(
        &mut self,
        row: &Playable,
        n: usize,
        up: bool,
        spotify: &Spotify,
        library: &Library,
    ) -> Option<usize> {
        let index = row.list_index();
        let uri = row.uri();
        let note = Cell::new(None);
        let tracks = self.tracks.as_deref().unwrap_or_default();
        if playlist_edit::server_item(tracks, index, &uri).is_none() {
            note.set(EditError::Moved.note());
            library.settle_write(false, &note);
            return None;
        }
        let Some(to) = move_target(index, n, up, tracks.len()) else {
            return Some(index);
        };

        let api = &spotify.api;
        let known = || library.known_playables();
        let moved = Cell::new(None);
        let web_snapshot = Cell::new(None);
        let id = self.id.clone();
        let snapshot_id = self.snapshot_id.clone();
        let written = write_through(
            api,
            Area::Playlists,
            || {
                playlist_edit::move_track(api, self, index, &uri, n, up, &known)
                    .map(|at| moved.set(Some(at)))
                    .map_err(|err| {
                        // Also a copy reloaded before the failure (its snapshot changed).
                        if err.stores_copy() || self.snapshot_id != snapshot_id {
                            library.playlist_update(self);
                        }
                        edit_failed(err, &note)
                    })
            },
            || {
                // A cleared copy has no version for Spotify to apply the move to.
                if snapshot_id.is_empty() {
                    note.set(EditError::Moved.note());
                    return false;
                }
                match api.reorder_playlist_item(&id, index, to, &snapshot_id) {
                    Ok(snapshot) => {
                        web_snapshot.set(Some(snapshot));
                        true
                    }
                    Err(()) => false,
                }
            },
        );
        if !library.settle_write(written, &note) {
            return None;
        }
        if let Some(snapshot) = web_snapshot.take() {
            let at = moved_position(index, to);
            playlist_edit::move_after_web(self, index, at, &uri, &snapshot_id, &snapshot);
            moved.set(Some(at));
        }
        library.playlist_update(self);
        moved.get()
    }

    /// Append `new_tracks`: the internal API first (verified), the Web API per the fallback
    /// classes. True when the write landed; a failure posts its one note.
    pub fn append_tracks(
        &mut self,
        new_tracks: &[Playable],
        spotify: &Spotify,
        library: &Library,
    ) -> bool {
        let api = &spotify.api;
        let known = || library.known_playables();
        let web_used = Cell::new(false);
        let id = self.id.clone();
        let snapshot_id = self.snapshot_id.clone();
        let note = Cell::new(None);
        let written = write_through(
            api,
            Area::Playlists,
            || {
                playlist_edit::add_tracks(api, self, new_tracks, &known).map_err(|err| {
                    // Also a copy reloaded before the failure (its snapshot changed).
                    if err.stores_copy() || self.snapshot_id != snapshot_id {
                        library.playlist_update(self);
                    }
                    edit_failed(err, &note)
                })
            },
            || {
                web_used.set(true);
                api.append_tracks(&id, new_tracks, None).is_ok()
            },
        );

        if library.settle_write(written, &note) {
            if web_used.get() {
                match &mut self.tracks {
                    Some(tracks) => {
                        // The new tracks take the server positions after the old ones.
                        let start = tracks.len();
                        tracks.extend(new_tracks.iter().cloned().enumerate().map(
                            |(i, mut playable)| {
                                playable.set_list_index(start + i);
                                playable
                            },
                        ));
                        self.num_tracks = tracks.len();
                        self.tracks_unknown = false;
                    }
                    None => return true,
                }
            }
            library.playlist_update(self);
            return true;
        }
        false
    }

    pub fn sort(&mut self, key: &SortKey, direction: &SortDirection) {
        fn compare_artists(a: &[String], b: &[String]) -> Ordering {
            let sanitize_artists_name = |x: &[String]| -> Vec<String> {
                x.iter()
                    .map(|x| {
                        x.to_lowercase()
                            .split(' ')
                            .skip_while(|x| x == &"the")
                            .collect()
                    })
                    .collect()
            };

            let a = sanitize_artists_name(a);
            let b = sanitize_artists_name(b);

            a.cmp(&b)
        }

        fn compare_album(a: &Track, b: &Track) -> Ordering {
            a.album
                .as_ref()
                .map(|x| x.to_lowercase())
                .cmp(&b.album.as_ref().map(|x| x.to_lowercase()))
                .then_with(|| a.disc_number.cmp(&b.disc_number))
                .then_with(|| a.track_number.cmp(&b.track_number))
        }

        if let Some(c) = self.tracks.as_mut() {
            c.sort_by(|a, b| match (a.track(), b.track()) {
                (Some(a), Some(b)) => {
                    let (a, b) = match *direction {
                        SortDirection::Ascending => (a, b),
                        SortDirection::Descending => (b, a),
                    };
                    match *key {
                        SortKey::Title => a.title.to_lowercase().cmp(&b.title.to_lowercase()),
                        SortKey::Duration => a.duration.cmp(&b.duration),
                        SortKey::Album => compare_album(&a, &b),
                        SortKey::Added => a.added_at.cmp(&b.added_at),
                        SortKey::Custom => std::cmp::Ordering::Equal,
                        SortKey::Artist => compare_artists(&a.artists, &b.artists)
                            .then_with(|| compare_album(&a, &b)),
                    }
                }
                _ => std::cmp::Ordering::Equal,
            })
        }
    }
}

impl From<&SimplifiedPlaylist> for Playlist {
    fn from(list: &SimplifiedPlaylist) -> Self {
        Self {
            id: list.id.id().to_string(),
            name: list.name.clone(),
            owner_id: list.owner.id.id().to_string(),
            owner_name: list.owner.display_name.clone(),
            snapshot_id: list.snapshot_id.clone(),
            num_tracks: list.items.total as usize,
            tracks: None,
            collaborative: list.collaborative,
            cover_url: list.images.first().map(|i| i.url.clone()),
            tracks_unknown: false,
            web_loaded: false,
        }
    }
}

impl From<&FullPlaylist> for Playlist {
    fn from(list: &FullPlaylist) -> Self {
        Self {
            id: list.id.id().to_string(),
            name: list.name.clone(),
            owner_id: list.owner.id.id().to_string(),
            owner_name: list.owner.display_name.clone(),
            snapshot_id: list.snapshot_id.clone(),
            num_tracks: list.items.total as usize,
            tracks: None,
            collaborative: list.collaborative,
            cover_url: list.images.first().map(|i| i.url.clone()),
            tracks_unknown: false,
            web_loaded: false,
        }
    }
}

impl ListItem for Playlist {
    fn merge_loaded(&mut self, loaded: Self) {
        // A row without a snapshot (a search row) takes the one its tracks were read at.
        let same_version = self.snapshot_id.is_empty() || self.snapshot_id == loaded.snapshot_id;
        if self.tracks.is_none() && same_version && loaded.tracks.is_some() {
            self.tracks = loaded.tracks;
            self.snapshot_id = loaded.snapshot_id;
            self.tracks_unknown = false;
        }
    }

    fn is_playing(&self, queue: &Queue) -> bool {
        if let Some(tracks) = self.tracks.as_ref() {
            let playing: Vec<String> = queue.list().read().iter().filter_map(|t| t.id()).collect();
            let ids: Vec<String> = tracks.iter().filter_map(|t| t.id()).collect();
            !ids.is_empty() && playing == ids
        } else {
            false
        }
    }

    fn display_left(&self, library: &Library) -> String {
        let hide_owners = library.cfg.values().hide_display_names.unwrap_or(false);
        match (self.owner_name.as_ref(), hide_owners) {
            (Some(owner), false) => format!("{} • {}", self.name, owner),
            _ => self.name.clone(),
        }
    }

    fn display_right(&self, library: &Library) -> String {
        let saved = if library.is_saved_playlist(self) {
            if library.cfg.values().use_nerdfont.unwrap_or(false) {
                "\u{f012c} "
            } else {
                "✓ "
            }
        } else {
            ""
        };

        format!("{saved}{}", self.count_text())
    }

    fn play(&mut self, queue: &Queue) {
        self.load_tracks(&queue.get_spotify());

        if let Some(tracks) = &self.tracks {
            let index = queue.append_next(tracks);
            queue.play(index, true, true);
        }
    }

    fn play_next(&mut self, queue: &Queue) {
        self.load_tracks(&queue.get_spotify());

        if let Some(tracks) = self.tracks.as_ref() {
            for track in tracks.iter().rev() {
                queue.insert_after_current(track.clone());
            }
        }
    }

    fn queue(&mut self, queue: &Queue) {
        self.load_tracks(&queue.get_spotify());

        if let Some(tracks) = self.tracks.as_ref() {
            for track in tracks.iter() {
                queue.append(track.clone());
            }
        }
    }

    fn toggle_saved(&mut self, library: &Library) {
        // Don't allow users to unsave their own playlists with one keypress
        if !library.is_followed_playlist(self) {
            return;
        }

        if library.is_saved_playlist(self) {
            library.queue_unfollow(&self.id);
        } else {
            library.queue_follow(self.clone());
        }
    }

    fn save(&mut self, library: &Library) {
        library.queue_follow(self.clone());
    }

    fn unsave(&mut self, library: &Library) {
        library.queue_unfollow(&self.id);
    }

    fn open(&self, queue: Arc<Queue>, library: Arc<Library>) -> Option<Box<dyn ViewExt>> {
        Some(PlaylistView::new(queue, library, self).into_boxed_view_ext())
    }

    fn open_recommendations(
        &mut self,
        queue: Arc<Queue>,
        library: Arc<Library>,
    ) -> Option<Box<dyn ViewExt>> {
        self.load_tracks(&queue.get_spotify());
        const MAX_SEEDS: usize = 5;
        let track_ids: Vec<String> = self
            .tracks
            .as_ref()?
            .iter()
            .filter_map(|t| t.id())
            // only select unique tracks
            .collect::<HashSet<_>>()
            .into_iter()
            // spotify allows at max 5 seed items, so choose them at random
            .sample(&mut rng(), MAX_SEEDS);

        if track_ids.is_empty() {
            return None;
        }

        let spotify = queue.get_spotify();
        let recommendations: Option<Vec<Track>> = spotify
            .api
            .recommendations(
                None,
                None,
                Some(track_ids.iter().map(|t| t.as_ref()).collect()),
            )
            .ok()
            .map(|r| r.tracks)
            .map(|tracks| tracks.iter().map(Track::from).collect());

        recommendations.map(|tracks| {
            ListView::new(
                Arc::new(RwLock::new(tracks)),
                queue.clone(),
                library.clone(),
            )
            .with_title(&format!("Similar to Tracks in \"{}\"", self.name))
            .into_boxed_view_ext()
        })
    }

    fn share_url(&self) -> Option<String> {
        Some(format!(
            "https://open.spotify.com/user/{}/playlist/{}",
            self.owner_id, self.id
        ))
    }

    fn playlist_id(&self) -> Option<String> {
        Some(self.id.clone())
    }

    fn is_saved(&self, library: &Library) -> Option<bool> {
        // save status of personal playlists can't be toggled for safety
        if !library.is_followed_playlist(self) {
            return None;
        }

        Some(library.is_saved_playlist(self))
    }

    #[inline]
    fn is_playable(&self) -> bool {
        true
    }

    fn id(&self) -> Option<String> {
        Some(self.id.clone())
    }

    fn as_listitem(&self) -> Box<dyn ListItem> {
        Box::new(self.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unknown() -> Playlist {
        Playlist {
            id: "p1".into(),
            name: "Mix".into(),
            owner_id: "o".into(),
            owner_name: None,
            snapshot_id: String::new(),
            num_tracks: 0,
            tracks: None,
            collaborative: false,
            cover_url: None,
            tracks_unknown: true,
            web_loaded: false,
        }
    }

    #[test]
    fn unknown_count_is_blank_until_tracks_are_loaded() {
        let mut p = unknown();
        assert_eq!(p.count_text().trim(), "");
        assert_eq!(p.count_text().len(), "   3 tracks".len());

        let loaded = Playlist {
            tracks: Some(Vec::new()),
            tracks_unknown: false,
            web_loaded: false,
            ..unknown()
        };
        p.merge_loaded(loaded);
        assert!(!p.tracks_unknown);
        assert_eq!(p.count_text(), "   0 tracks");
    }

    #[test]
    fn a_failed_load_written_back_keeps_the_count_unknown() {
        let mut p = unknown();
        p.merge_loaded(unknown());
        assert!(p.tracks_unknown && p.tracks.is_none());
        assert_eq!(p.count_text().trim(), "");
    }

    #[test]
    fn a_search_row_takes_the_snapshot_of_its_loaded_tracks() {
        let mut p = unknown();
        let loaded = Playlist {
            snapshot_id: "s1".into(),
            tracks: Some(Vec::new()),
            tracks_unknown: false,
            web_loaded: false,
            ..unknown()
        };
        p.merge_loaded(loaded);
        assert_eq!(p.snapshot_id, "s1");
        assert!(p.tracks.is_some() && !p.tracks_unknown);
    }

    #[test]
    fn a_row_refreshed_to_another_snapshot_drops_the_load() {
        let mut p = Playlist {
            snapshot_id: "s2".into(),
            ..unknown()
        };
        let loaded = Playlist {
            snapshot_id: "s1".into(),
            tracks: Some(Vec::new()),
            ..unknown()
        };
        p.merge_loaded(loaded);
        assert_eq!(p.snapshot_id, "s2");
        assert!(p.tracks.is_none());
    }

    #[test]
    fn a_load_clears_the_unknown_count_and_sets_a_missing_snapshot() {
        let mut p = unknown();
        assert!(p.store_loaded(Some((Vec::new(), "s1".into()))));
        assert!(!p.tracks_unknown && p.tracks.is_some());
        assert_eq!(p.snapshot_id, "s1");
        assert_eq!(p.count_text(), "   0 tracks");
    }

    #[test]
    fn a_load_keeps_a_known_snapshot() {
        let mut p = Playlist {
            snapshot_id: "s0".into(),
            ..unknown()
        };
        assert!(p.store_loaded(Some((Vec::new(), "s1".into()))));
        assert_eq!(p.snapshot_id, "s0");
    }

    #[test]
    fn a_web_load_onto_a_copy_without_a_snapshot_is_remembered_until_a_write_clears_it() {
        // The Web read gives no snapshot.
        let mut p = unknown();
        assert!(p.store_loaded(Some((Vec::new(), String::new()))));
        assert!(p.web_loaded && p.snapshot_id.is_empty());
        p.clear_snapshot();
        assert!(!p.web_loaded);

        // An internal load (it has a snapshot) and a copy that already had one are not marked.
        let mut p = unknown();
        assert!(p.store_loaded(Some((Vec::new(), "s1".into()))));
        assert!(!p.web_loaded);
        let mut p = Playlist {
            snapshot_id: "s0".into(),
            ..unknown()
        };
        assert!(p.store_loaded(Some((Vec::new(), String::new()))));
        assert!(!p.web_loaded);
    }

    #[test]
    fn a_failed_load_leaves_the_row_unknown() {
        let mut p = unknown();
        assert!(!p.store_loaded(None));
        assert!(p.tracks_unknown && p.tracks.is_none() && p.snapshot_id.is_empty());
    }
}
