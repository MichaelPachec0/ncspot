use std::sync::{Arc, RwLock};

use cursive::Cursive;
use cursive::event::{Event, EventResult, MouseButton, MouseEvent};
use cursive::view::{Margins, View, ViewWrapper};
use cursive::views::Dialog;

use crate::command::{Command, TargetMode};
use crate::commands::CommandResult;
use crate::internal_api::Area;
use crate::library::Library;
use crate::playlist_layout::{PlaylistRow, rows};
use crate::queue::Queue;
use crate::spotify::Spotify;
use crate::traits::ViewExt;
use crate::ui::listview::ListView;
use crate::ui::modal::Modal;

/// What the rows are built from: per store playlist its id, snapshot id and whether its
/// tracks are loaded, and the length of the layout in use.
#[derive(PartialEq, Eq, Default)]
struct Fingerprint {
    playlists: Vec<(String, String, bool)>,
    layout_len: usize,
}

pub struct PlaylistsView {
    list: ListView<PlaylistRow>,
    rows: Arc<RwLock<Vec<PlaylistRow>>>,
    fingerprint: Option<Fingerprint>,
    library: Arc<Library>,
    spotify: Spotify,
}

impl PlaylistsView {
    pub fn new(queue: Arc<Queue>, library: Arc<Library>) -> Self {
        let rows = Arc::new(RwLock::new(Vec::new()));
        let spotify = queue.get_spotify();
        let mut view = Self {
            list: ListView::new(rows.clone(), queue, library.clone()),
            rows,
            fingerprint: None,
            library,
            spotify,
        };
        view.refresh_rows();
        view
    }

    /// Rebuild the rows when the store or the layout in use changed. The layout is used only
    /// while the internal API playlists area is usable; otherwise the list is flat.
    fn refresh_rows(&mut self) {
        let playlists = self.library.playlists.read().unwrap();
        let usable = self.spotify.api.internal_usable(Area::Playlists);
        let layout = self.library.playlist_layout.read().unwrap();
        let layout: &[_] = if usable { &layout } else { &[] };
        let fingerprint = Fingerprint {
            playlists: playlists
                .iter()
                .map(|p| (p.id.clone(), p.snapshot_id.clone(), p.tracks.is_some()))
                .collect(),
            layout_len: layout.len(),
        };
        if self.fingerprint.as_ref() != Some(&fingerprint) {
            *self.rows.write().unwrap() = rows(&playlists, layout);
            self.fingerprint = Some(fingerprint);
        }
    }

    fn selected_is_header(&self) -> bool {
        matches!(
            self.rows
                .read()
                .unwrap()
                .get(self.list.get_selected_index()),
            Some(PlaylistRow::Header { .. })
        )
    }

    pub fn delete_dialog(&mut self) -> Option<Modal<Dialog>> {
        let id = self
            .rows
            .read()
            .unwrap()
            .get(self.list.get_selected_index())
            .and_then(PlaylistRow::playlist)
            .map(|p| p.id.clone())?;
        let library = self.library.clone();
        let dialog = Dialog::text("Are you sure you want to delete this playlist?")
            .padding(Margins::lrtb(1, 1, 1, 0))
            .title("Delete playlist")
            .dismiss_button("No")
            .button("Yes", move |s: &mut Cursive| {
                library.delete_playlist(&id);
                s.pop_layer();
            });
        Some(Modal::new(dialog))
    }
}

/// Whether `cmd` acts on the selected row (and so does nothing on a header row).
fn acts_on_selection(cmd: &Command) -> bool {
    match cmd {
        Command::Play
        | Command::PlayNext
        | Command::Queue
        | Command::Save
        | Command::Add
        | Command::Delete
        | Command::Goto(_) => true,
        Command::Open(TargetMode::Selected)
        | Command::ShowRecommendations(TargetMode::Selected) => true,
        #[cfg(feature = "share_clipboard")]
        Command::Share(TargetMode::Selected) => true,
        _ => false,
    }
}

impl ViewWrapper for PlaylistsView {
    wrap_impl!(self.list: ListView<PlaylistRow>);

    fn wrap_layout(&mut self, size: cursive::Vec2) {
        self.refresh_rows();
        self.list.layout(size);
    }

    fn wrap_on_event(&mut self, event: Event) -> EventResult {
        let right_click = matches!(
            event,
            Event::Mouse {
                event: MouseEvent::Press(MouseButton::Right),
                ..
            }
        );
        let result = self.list.on_event(event);
        // A right click selects the row; on a header it opens no (empty) context menu.
        if right_click && self.selected_is_header() {
            return EventResult::Consumed(None);
        }
        result
    }
}

impl ViewExt for PlaylistsView {
    fn title(&self) -> String {
        "Playlists".to_string()
    }

    fn on_command(&mut self, s: &mut Cursive, cmd: &Command) -> Result<CommandResult, String> {
        if acts_on_selection(cmd) && self.selected_is_header() {
            return Ok(CommandResult::Consumed(None));
        }

        if let Command::Delete = cmd {
            if let Some(dialog) = self.delete_dialog() {
                s.add_layer(dialog);
            }
            return Ok(CommandResult::Consumed(None));
        }

        self.list.on_command(s, cmd)
    }
}
