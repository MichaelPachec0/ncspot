use std::sync::{Arc, RwLock};

use cursive::Cursive;
use cursive::view::ViewWrapper;

use crate::command::Command;
use crate::commands::CommandResult;
use crate::library::Library;
use crate::model::category::Category;
use crate::queue::Queue;
use crate::traits::ViewExt;

use crate::ui::listview::ListView;
use crate::ui::pagination::fill_in_background;

pub struct BrowseView {
    list: ListView<Category>,
}

impl BrowseView {
    /// Builds the view at once; the categories load on a background thread, because the request
    /// can sleep in the rate limiter and this view is built while the app starts up.
    pub fn new(queue: Arc<Queue>, library: Arc<Library>) -> Self {
        let content = Arc::new(RwLock::new(Vec::new()));
        let spotify = queue.get_spotify();
        let list = ListView::new(content.clone(), queue, library.clone());
        fill_in_background(
            content,
            list.get_pagination().clone(),
            move || spotify.api.categories(),
            move || library.trigger_redraw(),
        );

        Self { list }
    }
}

impl ViewWrapper for BrowseView {
    wrap_impl!(self.list: ListView<Category>);
}

impl ViewExt for BrowseView {
    fn title(&self) -> String {
        "Browse".to_string()
    }

    fn on_command(&mut self, s: &mut Cursive, cmd: &Command) -> Result<CommandResult, String> {
        self.list.on_command(s, cmd)
    }
}
