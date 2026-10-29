use std::sync::{Arc, RwLock};

use cursive::Cursive;
use cursive::view::ViewWrapper;

use crate::command::Command;
use crate::commands::CommandResult;
use crate::library::Library;
use crate::model::playable::Playable;
use crate::model::playlist::Playlist;
use crate::queue::Queue;
use crate::spotify::Spotify;

use crate::traits::ViewExt;
use crate::ui::listview::ListView;

/// A playlist's tracks. `playlist` stays in server order (it is what edits work on and what
/// the library stores); only the rows of `list` follow the saved sort order.
pub struct PlaylistView {
    playlist: Playlist,
    rows: Arc<RwLock<Vec<Playable>>>,
    list: ListView<Playable>,
    spotify: Spotify,
    library: Arc<Library>,
    queue: Arc<Queue>,
}

impl PlaylistView {
    pub fn new(queue: Arc<Queue>, library: Arc<Library>, playlist: &Playlist) -> Self {
        let mut playlist = playlist.clone();
        let cleared = playlist.tracks.is_some() && playlist.snapshot_id.is_empty();
        playlist.load_tracks(&queue.get_spotify());
        if cleared && !playlist.snapshot_id.is_empty() {
            // Reloaded: store the current copy so the library matches the view, unless a
            // sync stored a newer one meanwhile.
            library.playlist_update_if_unchanged(&playlist, "");
        }

        let rows = Arc::new(RwLock::new(sorted_rows(&library, &playlist)));
        let spotify = queue.get_spotify();
        let list = ListView::new(rows.clone(), queue.clone(), library.clone());

        Self {
            playlist,
            rows,
            list,
            spotify,
            library,
            queue,
        }
    }

    /// Rebuild the list from the playlist's tracks in the saved sort order.
    fn show_tracks(&mut self) {
        self.rows = Arc::new(RwLock::new(sorted_rows(&self.library, &self.playlist)));
        self.list = ListView::new(self.rows.clone(), self.queue.clone(), self.library.clone());
    }
}

/// The tracks of `playlist` in its saved sort order (server order when none is saved).
fn sorted_rows(library: &Library, playlist: &Playlist) -> Vec<Playable> {
    let order = library
        .cfg
        .state()
        .playlist_orders
        .get(&playlist.id)
        .cloned();
    let mut sorted = playlist.clone();
    if let Some(order) = order {
        sorted.sort(&order.key, &order.direction);
    }
    sorted.tracks.unwrap_or_default()
}

impl ViewWrapper for PlaylistView {
    wrap_impl!(self.list: ListView<Playable>);
}

impl ViewExt for PlaylistView {
    #[cfg(feature = "harness")]
    fn harness_state(&self) -> Option<crate::harness::HarnessState> {
        let mut state = self.list.harness_state()?;
        state.view = "playlist".into();
        Some(state)
    }

    fn title(&self) -> String {
        self.playlist.name.clone()
    }

    fn title_sub(&self) -> String {
        if let Some(tracks) = self.playlist.tracks.as_ref() {
            let duration_secs = tracks.iter().map(|p| p.duration() as u64 / 1000).sum();
            let duration = std::time::Duration::from_secs(duration_secs);
            format!(
                "{} tracks, {}",
                tracks.len(),
                crate::utils::format_duration(&duration)
            )
        } else {
            "".to_string()
        }
    }

    fn on_command(&mut self, s: &mut Cursive, cmd: &Command) -> Result<CommandResult, String> {
        if let Command::Delete = cmd {
            let pos = self.list.get_selected_index();
            // The row itself: its server position and uri (the rows may be sorted, `playlist`
            // is not).
            let Some(row) = self.rows.read().unwrap().get(pos).cloned() else {
                return Ok(CommandResult::Consumed(None));
            };
            let snapshot_id = self.playlist.snapshot_id.clone();

            return match self
                .playlist
                .delete_track(&row, self.spotify.clone(), &self.library)
            {
                Ok(()) => {
                    // Rebuilt from the copy: a reload before the delete may have changed more
                    // than the removed row, and the later rows moved one server position up.
                    self.show_tracks();
                    self.list.move_focus_to(pos);
                    Ok(CommandResult::Consumed(None))
                }
                Err(note) => {
                    // A failed delete may have reloaded the copy (or cleared its snapshot).
                    if self.playlist.snapshot_id != snapshot_id {
                        self.show_tracks();
                    }
                    Err(note)
                }
            };
        }

        if let Command::Pin | Command::Unpin = cmd {
            let note = self
                .library
                .pin_command(&self.playlist.id, matches!(cmd, Command::Pin));
            return Ok(CommandResult::Consumed(note));
        }

        if let Command::Sort(key, direction) = cmd {
            self.library.cfg.with_state_mut(|state| {
                let order = crate::config::SortingOrder {
                    key: key.clone(),
                    direction: direction.clone(),
                };
                state
                    .playlist_orders
                    .insert(self.playlist.id.clone(), order);
            });

            self.show_tracks();
            return Ok(CommandResult::Consumed(None));
        }

        self.list.on_command(s, cmd)
    }
}
