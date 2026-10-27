use std::sync::Arc;

use cursive::Cursive;
use cursive::view::ViewWrapper;
use strum::IntoEnumIterator;

use crate::command::Command;
use crate::commands::CommandResult;
use crate::config::LibraryTab;
use crate::library::{Library, TabUpdate};
use crate::queue::Queue;
use crate::traits::ViewExt;
use crate::ui::browse::BrowseView;
use crate::ui::listview::ListView;
use crate::ui::playlists::PlaylistsView;
use crate::ui::tabbedview::TabbedView;

/// Status note for `update tab` where it has nothing to update.
pub const UPDATE_TAB_HINT: &str = "u updates the current library tab";

pub struct LibraryView {
    tabs: TabbedView,
    /// The tabs added to `tabs`, in order.
    added: Vec<LibraryTab>,
    display_name: Option<String>,
    library: Arc<Library>,
}

impl LibraryView {
    pub fn new(queue: Arc<Queue>, library: Arc<Library>) -> Self {
        let mut tabview = TabbedView::new();
        let selected_tabs = library
            .cfg
            .values()
            .library_tabs
            .clone()
            .unwrap_or_else(|| Vec::from_iter(LibraryTab::iter()));

        let mut added = Vec::new();
        for tab in selected_tabs {
            if tab != LibraryTab::Episodes || library.episodes_enabled {
                added.push(tab.clone());
            }
            match tab {
                LibraryTab::Tracks => tabview.add_tab(
                    "Tracks",
                    ListView::new(library.tracks.clone(), queue.clone(), library.clone()),
                ),
                LibraryTab::Albums => tabview.add_tab(
                    "Albums",
                    ListView::new(library.albums.clone(), queue.clone(), library.clone()),
                ),
                LibraryTab::Artists => tabview.add_tab(
                    "Artists",
                    ListView::new(library.artists.clone(), queue.clone(), library.clone()),
                ),
                LibraryTab::Playlists => tabview.add_tab(
                    "Playlists",
                    PlaylistsView::new(queue.clone(), library.clone()),
                ),
                LibraryTab::Podcasts => tabview.add_tab(
                    "Podcasts",
                    ListView::new(library.shows.clone(), queue.clone(), library.clone()),
                ),
                LibraryTab::Episodes => {
                    if library.episodes_enabled {
                        tabview.add_tab(
                            "Your Episodes",
                            ListView::new(library.episodes.clone(), queue.clone(), library.clone()),
                        )
                    }
                }
                LibraryTab::Browse => {
                    tabview.add_tab("Browse", BrowseView::new(queue.clone(), library.clone()))
                }
            }
        }

        Self {
            tabs: tabview,
            added,
            display_name: {
                let hide_username = library.cfg.values().hide_display_names.unwrap_or(false);
                if hide_username {
                    None
                } else {
                    library.display_name.clone()
                }
            },
            library,
        }
    }

    /// Sync the visible tab's part of the library; a status note when nothing was started.
    fn update_tab(&self) -> Option<String> {
        let update = match self.added.get(self.tabs.selected()) {
            Some(tab) => self.library.update_tab(tab),
            None => TabUpdate::NoSet,
        };
        match update {
            TabUpdate::Started => None,
            TabUpdate::Busy => Some("library update already running".to_string()),
            TabUpdate::NoSet => Some(UPDATE_TAB_HINT.to_string()),
        }
    }

    #[cfg(feature = "harness")]
    /// Show `tab`; false when this library does not have it.
    pub fn select_tab(&mut self, tab: &LibraryTab) -> bool {
        match self.added.iter().position(|t| t == tab) {
            Some(index) => {
                self.tabs.set_selected(index);
                true
            }
            None => false,
        }
    }
}

impl ViewWrapper for LibraryView {
    wrap_impl!(self.tabs: TabbedView);
}

impl ViewExt for LibraryView {
    fn title(&self) -> String {
        if let Some(name) = &self.display_name {
            format!("Library of {name}")
        } else {
            "Library".to_string()
        }
    }

    #[cfg(feature = "harness")]
    fn harness_state(&self) -> Option<crate::harness::HarnessState> {
        self.tabs.harness_state()
    }

    fn on_command(&mut self, s: &mut Cursive, cmd: &Command) -> Result<CommandResult, String> {
        if let Command::UpdateTab = cmd {
            return Ok(CommandResult::Consumed(self.update_tab()));
        }
        self.tabs.on_command(s, cmd)
    }
}
