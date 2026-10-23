//! Drive harness (feature `harness`): IPC commands that open a view by URI and write a JSON
//! snapshot of the current view, so a script can drive ncspot and check what it shows.

use serde::Serialize;

/// One list in the current view.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TabState {
    pub name: String,
    /// Rows loaded so far.
    pub rows: usize,
    /// Rows that can be played.
    pub playable: usize,
    /// Total rows the source reports, `None` when the list has no pagination (fully loaded).
    pub max: Option<usize>,
    /// Whether a page is being fetched.
    pub busy: bool,
}

/// What a view reports to the harness.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HarnessState {
    pub view: String,
    /// Name of the selected tab, for tabbed views.
    pub tab: Option<String>,
    pub tabs: Vec<TabState>,
}
