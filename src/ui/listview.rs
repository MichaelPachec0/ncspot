use cursive::view::scroll::Scroller;
use log::info;
use parking_lot::RwLock;
use std::cmp::{Ordering, max, min};
use std::sync::Arc;

use cursive::align::HAlign;
use cursive::event::{Callback, Event, EventResult, MouseButton, MouseEvent};
use cursive::theme::{ColorStyle, ColorType, PaletteColor};
use cursive::traits::View;
use cursive::view::scroll;
use cursive::{Cursive, Printer, Rect, Vec2, XY};
use unicode_width::UnicodeWidthStr;

use crate::command::{Command, GotoMode, InsertSource, JumpMode, MoveAmount, MoveMode, TargetMode};
use crate::commands::CommandResult;
use crate::ext_traits::CursiveExt;
use crate::library::Library;
use crate::model::album::Album;
use crate::model::artist::Artist;
use crate::model::episode::Episode;
use crate::model::playable::Playable;
use crate::model::playlist::Playlist;
use crate::model::show::Show;
use crate::model::track::Track;
use crate::queue::Queue;
#[cfg(feature = "share_clipboard")]
use crate::sharing::{read_share, write_share};
use crate::spotify::UriType;
use crate::traits::{IntoBoxedViewExt, ListItem, ViewExt};
use crate::ui::album::AlbumView;
use crate::ui::artist::ArtistView;
use crate::ui::contextmenu::ContextMenu;
use crate::ui::pagination::Pagination;

/// Columns of the tag column of a row with a tag.
pub const TAG_WIDTH: usize = 8;
/// Columns kept clear right of the tag column, so tags line up whatever the right text is.
pub const TAG_RIGHT_RESERVE: usize = 20;

pub enum MouseHandleResult {
    Handled(EventResult),
    Unhandled(Command),
}

pub struct ListView<I: ListItem> {
    content: Arc<RwLock<Vec<I>>>,
    last_content_len: usize,
    selected: usize,
    search_query: String,
    search_indexes: Vec<usize>,
    search_selected_index: usize,
    last_size: Vec2,
    scroller: scroll::Core,
    queue: Arc<Queue>,
    library: Arc<Library>,
    pagination: Pagination<I>,
    title: String,
}

impl<I: ListItem> Scroller for ListView<I> {
    fn get_scroller_mut(&mut self) -> &mut scroll::Core {
        &mut self.scroller
    }

    fn get_scroller(&self) -> &scroll::Core {
        &self.scroller
    }
}

impl<I: ListItem + Clone> ListView<I> {
    pub fn new(content: Arc<RwLock<Vec<I>>>, queue: Arc<Queue>, library: Arc<Library>) -> Self {
        let result = Self {
            content,
            last_content_len: 0,
            selected: 0,
            search_query: String::new(),
            search_indexes: Vec::new(),
            search_selected_index: 0,
            last_size: Vec2::new(0, 0),
            scroller: scroll::Core::new(),
            queue,
            library,
            pagination: Pagination::default(),
            title: "".to_string(),
        };
        result.try_paginate();
        result
    }

    pub fn with_title(mut self, title: &str) -> Self {
        self.title = title.to_string();
        self
    }

    pub fn get_pagination(&self) -> &Pagination<I> {
        &self.pagination
    }

    /// Return the current amount of items in `content`
    ///
    /// If `include_paginator` is `true`, the pagination entry will be included
    /// in the count.
    pub fn content_len(&self, include_paginator: bool) -> usize {
        let content_len = self.content.read().len();

        // add 1 more row for paginator if we can paginate
        if self.can_paginate() && include_paginator {
            content_len + 1
        } else {
            content_len
        }
    }

    /// Return wether there are still items that aren't shown in the listview.
    ///
    /// `true` if there are unloaded items
    /// `false` if all items are loaded
    fn can_paginate(&self) -> bool {
        self.get_pagination().can_load_more()
    }

    /// Try to load more items into the list if neccessary.
    #[inline]
    fn try_paginate(&self) {
        // Paginate if there are more items
        //  AND
        //   The selected item is the current last item (keyboard scrolling)
        //    OR
        //   The scroller can't scroll further down (mouse scrolling)
        if self.can_paginate()
            && (self.selected == self.content.read().len().saturating_sub(1)
                || !self.scroller.can_scroll_down())
        {
            self.pagination.call(&self.content, self.library.clone());
        }
    }

    /// Return whether this [ListView] has visible scrollbars.
    pub fn has_visible_scrollbars(&self) -> bool {
        self.scroller.get_show_scrollbars()
    }

    pub fn get_selected_index(&self) -> usize {
        self.selected
    }

    pub fn get_indexes_of(&self, query: &str) -> Vec<usize> {
        // A copy, as in [ListView::draw]: a custom track format asks the library too.
        let content = self.content.read().clone();
        content
            .iter()
            .enumerate()
            .filter(|(_, i)| {
                i.rule_label().is_none()
                    && i.display_left(&self.library)
                        .to_lowercase()
                        .contains(&query[..].to_lowercase())
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// The content was replaced in place: keep the scroll state and the selection (clamped to
    /// the new length) and redo the search matches, whose positions are stale.
    pub fn content_replaced(&mut self) {
        let last = self.content_len(false).saturating_sub(1);
        self.selected = min(self.selected, last);
        if self.is_rule_row(self.selected) {
            self.move_focus_to(self.selected);
        }
        if !self.search_query.is_empty() {
            self.search_indexes = self.get_indexes_of(&self.search_query);
            self.search_selected_index = min(
                self.search_selected_index,
                self.search_indexes.len().saturating_sub(1),
            );
        }
    }

    #[cfg(test)]
    pub fn set_search_for_test(&mut self, query: &str, indexes: Vec<usize>) {
        self.search_query = query.to_string();
        self.search_indexes = indexes;
    }

    #[cfg(test)]
    pub fn search_for_test(&self) -> (String, Vec<usize>) {
        (self.search_query.clone(), self.search_indexes.clone())
    }

    /// Whether the row at `index` is a rule line.
    fn is_rule_row(&self, index: usize) -> bool {
        self.content
            .read()
            .get(index)
            .is_some_and(|row| row.rule_label().is_some())
    }

    /// Select the row at `target` (clamped to the list). A rule row is never selected: the
    /// nearest other row in the direction of travel is taken instead, else the nearest one the
    /// other way, else the selection stays.
    pub fn move_focus_to(&mut self, target: usize) {
        let down = target >= self.selected;
        let chosen = {
            // Only `rule_label` runs under the guard; it never asks the library.
            let content = self.content.read();
            let target = min(target, content.len().saturating_sub(1));
            let is_rule = |i: usize| content.get(i).is_some_and(|row| row.rule_label().is_some());
            if is_rule(target) {
                let below = (target + 1..content.len()).find(|&i| !is_rule(i));
                let above = (0..target).rev().find(|&i| !is_rule(i));
                if down {
                    below.or(above)
                } else {
                    above.or(below)
                }
            } else {
                Some(target)
            }
        };
        if let Some(index) = chosen {
            self.selected = index;
        }
        self.scroller.scroll_to_y(self.selected);
    }

    pub fn move_focus(&mut self, delta: i32) {
        let new = self.selected as i32 + delta;
        self.move_focus_to(max(new, 0) as usize);
    }

    /// Append the currently selected item and all the following ones to the queue after the
    /// currently playing track and start playing them. Returns true if adding and playing the
    /// tracks succeeded, false otherwhise.
    fn attempt_play_all_tracks(&self) -> bool {
        if let Some(playables) = self.playable_copy() {
            let index = self.queue.append_next(&playables);
            self.queue.play(index + self.selected, true, false);
            true
        } else {
            false
        }
    }

    /// Copy the list out as playables under the read guard, which is released on return. `None`
    /// if the list does not hold tracks or playables.
    fn playable_copy(&self) -> Option<Vec<Playable>> {
        let content = self.content.read();
        let any = &(*content) as &dyn std::any::Any;
        any.downcast_ref::<Vec<Playable>>().cloned().or_else(|| {
            any.downcast_ref::<Vec<Track>>().map(|t| {
                t.iter()
                    .map(|t| Playable::Track(t.clone()))
                    .collect::<Vec<Playable>>()
            })
        })
    }

    /// Run `f` on a clone of the selected item with no list guard held, then
    /// merge what `f` loaded into that slot if it still holds the same item
    /// (same `id()`, both `Some`). The slot is never replaced, so a sync that
    /// refreshed it while `f` ran keeps its data. `None` if nothing is selected.
    fn with_selected_item<R>(&self, f: impl FnOnce(&mut I) -> R) -> Option<R> {
        let index = self.selected;
        let mut item = self.content.read().get(index).cloned()?;
        let result = f(&mut item);
        if let Some(id) = item.id() {
            let mut content = self.content.write();
            if let Some(slot) = content.get_mut(index)
                && slot.id() == Some(id)
            {
                slot.merge_loaded(item);
            }
        }
        Some(result)
    }

    /// The view the selected item opens, if any. No list guard is held while opening.
    fn open_selected(&self) -> Option<Box<dyn ViewExt>> {
        self.with_selected_item(|item| item.prepare_open(&self.queue));
        let target = self
            .content
            .read()
            .get(self.selected)
            .map(ListItem::as_listitem);
        target?.open(self.queue.clone(), self.library.clone())
    }

    /// Appends the currently focused item after the currently playing item and starts playing it.
    fn play_current_item(&mut self) {
        self.with_selected_item(|item| item.play(&self.queue));
    }

    /// Get the selected row from a mouse position and offset
    fn get_selected_row(&self, position: XY<usize>, offset: XY<usize>) -> Option<usize> {
        let viewport = self.scroller.content_viewport().top_left();
        let selected_row = position.checked_sub(offset).map(|p| p.y + viewport.y);
        selected_row.filter(|row| *row < self.content_len(false))
    }

    fn run_play_command(&mut self) {
        self.queue.clear();

        if !self.attempt_play_all_tracks() {
            self.play_current_item();
        }
    }

    /// Takes an incoming mouse event from Cursive and tries to act appropriately.
    ///
    /// Returns a MouseHandleResult which indicates whether the event has been handled by
    /// the function or if a command needs further processing.
    pub fn handle_mouse_event(&mut self, e: Event) -> MouseHandleResult {
        match e {
            Event::Mouse {
                event: MouseEvent::WheelUp,
                ..
            } => self.scroller.scroll_up(3),
            Event::Mouse {
                event: MouseEvent::WheelDown,
                ..
            } => {
                self.scroller.scroll_down(3);
                self.try_paginate();
            }
            Event::Mouse {
                event: MouseEvent::Press(MouseButton::Left),
                position,
                offset,
            } => {
                // This is safe as a mouse event is only propagated to a view when it is inside the
                // view. Therefore underflow shouldn't occur.
                let view_coordinates_click_position = position - offset;

                let drag_started = if self.has_visible_scrollbars() {
                    self.scroller.start_drag(view_coordinates_click_position)
                } else {
                    false
                };

                if drag_started {
                    log::debug!("grabbing scroller");
                } else if let Some(clicked_row_index) = self.get_selected_row(position, offset)
                    && !self.is_rule_row(clicked_row_index)
                {
                    let currently_selected_listitem = self
                        .content
                        .read()
                        .get(clicked_row_index)
                        .map(ListItem::as_listitem);
                    let currently_selected_is_individual = currently_selected_listitem
                        .filter(|item| item.track().is_some())
                        .is_some();
                    if self.selected == clicked_row_index && currently_selected_is_individual {
                        return MouseHandleResult::Unhandled(Command::Play);
                    } else {
                        // The clicked position wasn't focused yet or the item is a collection
                        // that can be opened.
                        self.move_focus_to(clicked_row_index);
                        if let Some(view) = self.open_selected() {
                            return MouseHandleResult::Handled(EventResult::Consumed(Some(
                                Callback::from_fn_once(move |s| {
                                    s.on_layout(|_, mut l| l.push_view(view));
                                }),
                            )));
                        }
                    }
                }
            }
            Event::Mouse {
                event: MouseEvent::Press(MouseButton::Right),
                position,
                offset,
            } => {
                if let Some(y) = self.get_selected_row(position, offset)
                    && !self.is_rule_row(y)
                {
                    self.move_focus_to(y);

                    let queue = self.queue.clone();
                    let library = self.library.clone();
                    if let Some(target) = {
                        let content = self.content.read();
                        content.get(self.selected).map(|t| t.as_listitem())
                    } {
                        let contextmenu = ContextMenu::new(&*target, queue, library);
                        return MouseHandleResult::Handled(EventResult::Consumed(Some(
                            Callback::from_fn_once(move |s| s.add_layer(contextmenu)),
                        )));
                    }
                }
            }
            Event::Mouse {
                event: MouseEvent::Hold(MouseButton::Left),
                position,
                offset,
            } => {
                if self.has_visible_scrollbars() {
                    self.scroller.drag(position.saturating_sub(offset));
                }
            }
            Event::Mouse {
                event: MouseEvent::Release(MouseButton::Left),
                ..
            } => {
                log::debug!("releasing scroller");
                self.scroller.release_grab();
            }
            _ => {
                return MouseHandleResult::Handled(EventResult::Ignored);
            }
        }

        MouseHandleResult::Handled(EventResult::Consumed(None))
    }
}

impl<I: ListItem + Clone> View for ListView<I> {
    fn draw(&self, printer: &Printer<'_, '_>) {
        scroll::draw_lines(self, printer, |_, printer, i| {
            // A copy, with the lock released: a row asks the library whether it is saved, and
            // when the content is a library store that is a second read of this lock. A second
            // read waits for any waiting writer, who waits for the first: a deadlock.
            let (len, item) = {
                let content = self.content.read();
                (content.len(), content.get(i).cloned())
            };

            // draw paginator after content
            if i == len && self.can_paginate() {
                let style = ColorStyle::secondary();

                let buf = format!("{} more items, scroll to load", self.pagination.remaining());
                printer.with_color(style, |printer| {
                    printer.print((0, 0), &buf);
                });
            } else if let Some(label) = item.as_ref().and_then(ListItem::rule_label) {
                let text = format!("--- {label} ");
                printer.with_color(ColorStyle::secondary(), |printer| {
                    printer.print_hline((0, 0), printer.size.x, "-");
                    printer.print((0, 0), &text);
                });
            } else if let Some(item) = item {
                let currently_playing =
                    item.is_playing(&self.queue) && self.queue.get_current_index() == Some(i);
                let is_local = item.track().map(|t| t.is_local).unwrap_or_default();
                let is_playable = item.track().map(|t| t.is_playable).unwrap_or_default();

                let style = if self.selected == i {
                    if currently_playing {
                        ColorStyle::new(
                            *printer.theme.palette.custom("playing_selected").unwrap(),
                            ColorType::Palette(PaletteColor::Highlight),
                        )
                    } else if is_local {
                        ColorStyle::new(
                            ColorType::Palette(PaletteColor::Secondary),
                            ColorType::Palette(PaletteColor::Highlight),
                        )
                    } else {
                        ColorStyle::highlight()
                    }
                } else if currently_playing {
                    ColorStyle::new(
                        ColorType::Color(*printer.theme.palette.custom("playing").unwrap()),
                        ColorType::Color(*printer.theme.palette.custom("playing_bg").unwrap()),
                    )
                } else if is_local || is_playable == Some(false) {
                    ColorStyle::secondary()
                } else {
                    ColorStyle::primary()
                };

                let left = item.display_left(&self.library);
                let center = item.display_center(&self.library);
                let right = item.display_right(&self.library);
                let tag = item.display_tag(&self.library);
                // A row with a tag draws no center; the tag needs room for a left text too.
                let draw_center = tag.is_none() && !center.is_empty();
                let tag = tag.and_then(|tag| {
                    let reserve = max(right.width(), TAG_RIGHT_RESERVE) + 1 + TAG_WIDTH;
                    let tag_x = printer.size.x.saturating_sub(reserve);
                    (tag_x >= 4).then_some((tag_x, tag))
                });

                // draw left string
                printer.with_color(style, |printer| {
                    printer.print_hline((0, 0), printer.size.x, " ");
                    printer.print((0, 0), &left);
                });

                // if line contains search query match, draw on top with
                // highlight color
                if self.search_indexes.contains(&i) {
                    let fg = *printer.theme.palette.custom("search_match").unwrap();
                    let matched_style = ColorStyle::new(fg, style.back);

                    let matches: Vec<(usize, usize)> = left
                        .to_lowercase()
                        .match_indices(&self.search_query)
                        .map(|i| (i.0, i.0 + i.1.len()))
                        .collect();

                    for m in matches {
                        printer.with_color(matched_style, |printer| {
                            printer.print((left[0..m.0].width(), 0), &left[m.0..m.1]);
                        });
                    }
                }

                // left string cut off indicator
                let center_offset = printer.size.x / 2;
                let left_max_length = if let Some((tag_x, _)) = tag {
                    tag_x - 1
                } else if draw_center {
                    center_offset.saturating_sub(1)
                } else {
                    printer.size.x.saturating_sub(right.width() + 1)
                };

                if left_max_length < left.width() {
                    // Before a tag, one blank column stays between the mark and the tag.
                    let offset = match tag {
                        Some((tag_x, _)) => tag_x - 3,
                        None => left_max_length.saturating_sub(1),
                    };
                    printer.with_color(style, |printer| {
                        printer.print_hline((offset, 0), printer.size.x, " ");
                        printer.print((offset, 0), "..");
                    });
                }

                // draw center string
                if draw_center {
                    printer.with_color(style, |printer| {
                        printer.print((center_offset, 0), &center);
                    });

                    // center string cut off indicator
                    let max_length = printer.size.x.saturating_sub(right.width() + 1);
                    if max_length < center_offset + center.width() {
                        let offset = max_length.saturating_sub(1);
                        printer.with_color(style, |printer| {
                            printer.print((offset, 0), "..");
                        });
                    }
                }

                if let Some((tag_x, tag)) = &tag {
                    printer.with_color(style, |printer| {
                        printer.print((*tag_x, 0), tag);
                    });
                }

                // draw right string
                let offset = HAlign::Right.get_offset(right.width(), printer.size.x);

                printer.with_color(style, |printer| {
                    printer.print((offset, 0), &right);
                });
            }
        });
    }

    fn layout(&mut self, size: Vec2) {
        self.last_size = size;

        let relayout_scroller = self.content_len(false) != self.last_content_len;
        self.last_content_len = self.content_len(true);

        scroll::layout(
            self,
            size,
            relayout_scroller,
            |_, _| {},
            |s, c| Vec2::new(c.x, s.content_len(true)),
        );

        if relayout_scroller && self.is_rule_row(self.selected) {
            self.move_focus_to(self.selected);
        }
    }

    fn needs_relayout(&self) -> bool {
        self.scroller.needs_relayout()
    }

    fn required_size(&mut self, constraint: Vec2) -> Vec2 {
        constraint
    }

    fn on_event(&mut self, e: Event) -> EventResult {
        match self.handle_mouse_event(e) {
            MouseHandleResult::Handled(event_result) => event_result,
            MouseHandleResult::Unhandled(command) => match command {
                Command::Play => {
                    self.run_play_command();
                    EventResult::consumed()
                }
                _ => EventResult::Ignored,
            },
        }
    }

    fn important_area(&self, view_size: Vec2) -> Rect {
        if self.content_len(false) > 0 {
            Rect::from_point((view_size.x, self.selected))
        } else {
            Rect::from_point((0, 0))
        }
    }
}

#[cfg(feature = "harness")]
fn tab_state<I: ListItem>(
    name: &str,
    items: &[I],
    max: Option<usize>,
    busy: bool,
) -> crate::harness::TabState {
    crate::harness::TabState {
        name: name.to_string(),
        rows: items.iter().filter(|i| i.rule_label().is_none()).count(),
        playable: items.iter().filter(|i| i.is_playable()).count(),
        max,
        busy,
    }
}

/// Whether `cmd` acts on the selected row (as opposed to the queue's current track or
/// nothing in the list).
fn acts_on_selected_row(cmd: &Command) -> bool {
    match cmd {
        Command::Play
        | Command::PlayNext
        | Command::Queue
        | Command::Save
        | Command::Add
        | Command::Delete
        | Command::Goto(_) => true,
        Command::Open(mode) | Command::ShowRecommendations(mode) => {
            matches!(mode, TargetMode::Selected)
        }
        #[cfg(feature = "share_clipboard")]
        Command::Share(mode) => matches!(mode, TargetMode::Selected),
        _ => false,
    }
}

impl<I: ListItem + Clone> ViewExt for ListView<I> {
    fn title(&self) -> String {
        self.title.clone()
    }

    #[cfg(feature = "harness")]
    fn harness_state(&self) -> Option<crate::harness::HarnessState> {
        let content = self.content.read();
        Some(crate::harness::HarnessState {
            view: "list".into(),
            tab: None,
            tabs: vec![tab_state(
                &self.title,
                &content,
                self.pagination.max_content(),
                self.pagination.is_busy(),
            )],
        })
    }

    fn on_command(&mut self, _s: &mut Cursive, cmd: &Command) -> Result<CommandResult, String> {
        // A rule row takes no row action; every other command goes its usual way.
        if acts_on_selected_row(cmd) && self.is_rule_row(self.selected) {
            return Ok(CommandResult::Consumed(None));
        }

        match cmd {
            Command::Play => {
                self.run_play_command();
                return Ok(CommandResult::Consumed(None));
            }
            Command::PlayNext => {
                info!("played next");
                self.with_selected_item(|item| item.play_next(&self.queue));

                return Ok(CommandResult::Consumed(None));
            }
            Command::Queue => {
                self.with_selected_item(|item| item.queue(&self.queue));

                return Ok(CommandResult::Consumed(None));
            }
            Command::Save => {
                let mut item = {
                    let content = self.content.read();
                    content.get(self.selected).cloned()
                };

                if let Some(item) = item.as_mut() {
                    item.save(&self.library);
                }

                return Ok(CommandResult::Consumed(None));
            }
            Command::Add => {
                let item = {
                    let content = self.content.read();
                    content.get(self.selected).cloned()
                };

                if let Some(track) = item
                    && let Some(track) = track.track()
                {
                    let dialog = ContextMenu::add_track_dialog(self.library.clone(), track);
                    return Ok(CommandResult::Modal(Box::new(dialog)));
                }

                return Ok(CommandResult::Consumed(None));
            }
            Command::Delete => {
                let mut item = {
                    let content = self.content.read();
                    content.get(self.selected).cloned()
                };

                if let Some(item) = item.as_mut() {
                    item.unsave(&self.library);
                }

                return Ok(CommandResult::Consumed(None));
            }
            #[cfg(feature = "share_clipboard")]
            Command::Share(mode) => {
                let url = match mode {
                    TargetMode::Selected => self
                        .content
                        .read()
                        .get(self.selected)
                        .and_then(ListItem::share_url),
                    TargetMode::Current => self
                        .queue
                        .get_current()
                        .and_then(|t| t.as_listitem().share_url()),
                };

                if let Some(url) = url {
                    write_share(url).ok();
                }

                return Ok(CommandResult::Consumed(None));
            }
            Command::Jump(mode) => match mode {
                JumpMode::Query(query) => {
                    self.search_query = query.to_lowercase();
                    self.search_indexes = self.get_indexes_of(query);
                    self.search_selected_index = 0;
                    match self.search_indexes.first() {
                        Some(&index) => {
                            self.move_focus_to(index);
                            return Ok(CommandResult::Consumed(None));
                        }
                        None => return Ok(CommandResult::Consumed(None)),
                    }
                }
                JumpMode::Next => {
                    let len = self.search_indexes.len();
                    if len == 0 {
                        return Ok(CommandResult::Ignored);
                    }
                    let index = self.search_selected_index;
                    let next_index = match index.cmp(&(len - 1)) {
                        Ordering::Equal => 0,
                        _ => index + 1,
                    };
                    self.move_focus_to(self.search_indexes[next_index]);
                    self.search_selected_index = next_index;
                    return Ok(CommandResult::Consumed(None));
                }
                JumpMode::Previous => {
                    let len = self.search_indexes.len();
                    if len == 0 {
                        return Ok(CommandResult::Ignored);
                    }
                    let index = self.search_selected_index;
                    let prev_index = match index.cmp(&0) {
                        Ordering::Equal => len - 1,
                        _ => index - 1,
                    };
                    self.move_focus_to(self.search_indexes[prev_index]);
                    self.search_selected_index = prev_index;
                    return Ok(CommandResult::Consumed(None));
                }
            },
            Command::Move(mode, amount) => {
                let last_idx = self.content.read().len().saturating_sub(1);

                match mode {
                    MoveMode::Up => {
                        if self.selected > 0 {
                            match amount {
                                MoveAmount::Extreme => self.move_focus_to(0),
                                MoveAmount::Float(scale) => {
                                    let amount = (self.last_size.y as f32) * scale;
                                    self.move_focus(-(amount as i32))
                                }
                                MoveAmount::Integer(amount) => self.move_focus(-(*amount)),
                            }
                        }
                        return Ok(CommandResult::Consumed(None));
                    }
                    MoveMode::Down => {
                        if self.selected < last_idx {
                            match amount {
                                MoveAmount::Extreme => self.move_focus_to(last_idx),
                                MoveAmount::Float(scale) => {
                                    let amount = (self.last_size.y as f32) * scale;
                                    self.move_focus(amount as i32)
                                }
                                MoveAmount::Integer(amount) => self.move_focus(*amount),
                            }
                        }
                        self.try_paginate();
                        return Ok(CommandResult::Consumed(None));
                    }
                    _ => return Ok(CommandResult::Consumed(None)),
                }
            }
            Command::Open(mode) => {
                let queue = self.queue.clone();
                let library = self.library.clone();
                let target: Option<Box<dyn ListItem>> = match mode {
                    TargetMode::Current => self.queue.get_current().map(|t| t.as_listitem()),
                    TargetMode::Selected => {
                        self.with_selected_item(|item| item.prepare_open(&self.queue));
                        let content = self.content.read();
                        content.get(self.selected).map(|t| t.as_listitem())
                    }
                };

                // if item has a dedicated view, show it; otherwise open the context menu
                if let Some(target) = target {
                    let view = target.open(queue.clone(), library.clone());
                    return match view {
                        Some(view) => Ok(CommandResult::View(view)),
                        None => {
                            let contextmenu = ContextMenu::new(&*target, queue, library);
                            Ok(CommandResult::Modal(Box::new(contextmenu)))
                        }
                    };
                }
            }
            Command::Goto(mode) => {
                let item = self.content.read().get(self.selected).cloned();
                if let Some(item) = item {
                    let queue = self.queue.clone();
                    let library = self.library.clone();

                    match mode {
                        GotoMode::Album => {
                            if let Some(album) = item.album(&queue) {
                                let view =
                                    AlbumView::new(queue, library, &album).into_boxed_view_ext();
                                return Ok(CommandResult::View(view));
                            }
                        }
                        GotoMode::Artist => {
                            if let Some(artists) = item.artists() {
                                return match artists.len() {
                                    0 => Ok(CommandResult::Consumed(None)),
                                    1 => {
                                        let view = ArtistView::new(queue, library, &artists[0])
                                            .into_boxed_view_ext();
                                        Ok(CommandResult::View(view))
                                    }
                                    _ => {
                                        let dialog = ContextMenu::select_artist_dialog(
                                            library, queue, artists,
                                        );
                                        Ok(CommandResult::Modal(Box::new(dialog)))
                                    }
                                };
                            }
                        }
                    }
                }
            }
            Command::Insert(source) => {
                let url = match source {
                    InsertSource::Input(url) => Some(url.clone()),
                    #[cfg(feature = "share_clipboard")]
                    InsertSource::Clipboard => read_share()
                        .ok()
                        .and_then(crate::spotify_url::SpotifyUrl::from_url),
                };

                let spotify = self.queue.get_spotify();

                if let Some(url) = url {
                    let target: Option<Box<dyn ListItem>> = match url.uri_type {
                        UriType::Track => spotify
                            .api
                            .track(&url.id)
                            .map(|track| Track::from(&track).as_listitem())
                            .ok(),
                        UriType::Album => spotify
                            .api
                            .album(&url.id)
                            .map(|album| Album::from(&album).as_listitem())
                            .ok(),
                        UriType::Playlist => spotify
                            .api
                            .playlist(&url.id)
                            .map(|playlist| Playlist::from(&playlist).as_listitem())
                            .ok(),
                        UriType::Artist => spotify
                            .api
                            .artist(&url.id)
                            .map(|artist| Artist::from(&artist).as_listitem())
                            .ok(),
                        UriType::Episode => spotify
                            .api
                            .episode(&url.id)
                            .map(|episode| Episode::from(&episode).as_listitem())
                            .ok(),
                        UriType::Show => spotify
                            .api
                            .show(&url.id)
                            .map(|show| Show::from(&show).as_listitem())
                            .ok(),
                    };

                    let queue = self.queue.clone();
                    let library = self.library.clone();
                    // if item has a dedicated view, show it; otherwise open the context menu
                    if let Some(target) = target {
                        let view = target.open(queue.clone(), library.clone());
                        return match view {
                            Some(view) => Ok(CommandResult::View(view)),
                            None => {
                                let contextmenu = ContextMenu::new(target.as_ref(), queue, library);
                                Ok(CommandResult::Modal(Box::new(contextmenu)))
                            }
                        };
                    }
                }

                return Ok(CommandResult::Consumed(None));
            }
            Command::ShowRecommendations(mode) => {
                let queue = self.queue.clone();
                let library = self.library.clone();
                let target: Option<Box<dyn ListItem>> = match mode {
                    TargetMode::Current => self.queue.get_current().map(|t| t.as_listitem()),
                    TargetMode::Selected => {
                        let content = self.content.read();
                        content.get(self.selected).map(|t| t.as_listitem())
                    }
                };

                if let Some(mut target) = target {
                    let view = target.open_recommendations(queue, library);
                    return match view {
                        Some(view) => Ok(CommandResult::View(view)),
                        None => Ok(CommandResult::Consumed(None)),
                    };
                }
            }
            _ => {}
        };

        Ok(CommandResult::Ignored)
    }
}

#[cfg(all(test, feature = "harness"))]
mod harness_tests {
    use super::*;
    use crate::model::artist::Artist;

    #[test]
    fn tab_state_counts_rows_and_reports_paging() {
        let items = vec![
            Artist::new("a".into(), "A".into()),
            Artist::new("b".into(), "B".into()),
        ];
        let expected_playable = items.iter().filter(|i| i.is_playable()).count();
        let busy = tab_state("Artists", &items, Some(10), true);
        assert_eq!(busy.rows, 2);
        assert_eq!(busy.playable, expected_playable);
        assert_eq!(busy.max, Some(10));
        assert!(busy.busy);
        let done = tab_state("Artists", &items, None, false);
        assert_eq!((done.max, done.busy), (None, false));
    }

    /// A row that is a rule line when `rule` is set.
    #[derive(Clone)]
    struct Row {
        rule: bool,
    }

    impl ListItem for Row {
        fn is_playing(&self, _queue: &Queue) -> bool {
            false
        }
        fn display_left(&self, _library: &Library) -> String {
            String::new()
        }
        fn display_right(&self, _library: &Library) -> String {
            String::new()
        }
        fn play(&mut self, _queue: &Queue) {}
        fn play_next(&mut self, _queue: &Queue) {}
        fn queue(&mut self, _queue: &Queue) {}
        fn toggle_saved(&mut self, _library: &Library) {}
        fn save(&mut self, _library: &Library) {}
        fn unsave(&mut self, _library: &Library) {}
        fn open(&self, _queue: Arc<Queue>, _library: Arc<Library>) -> Option<Box<dyn ViewExt>> {
            None
        }
        fn share_url(&self) -> Option<String> {
            None
        }
        fn rule_label(&self) -> Option<String> {
            self.rule.then(|| "Rule".to_string())
        }
        fn as_listitem(&self) -> Box<dyn ListItem> {
            Box::new(self.clone())
        }
    }

    #[test]
    fn tab_state_does_not_count_rule_rows() {
        let items = vec![
            Row { rule: true },
            Row { rule: false },
            Row { rule: true },
            Row { rule: false },
        ];
        assert_eq!(tab_state("Top", &items, None, false).rows, 2);
    }
}

#[cfg(test)]
mod draw_test {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use cursive::buffer::PrintBuffer;
    use cursive::theme::Theme;

    use super::*;
    use crate::config::Config;
    use crate::events::EventManager;
    use crate::save_log::test_items::track;
    use crate::spotify::Spotify;

    /// A list over the library's own track store, as the Tracks tab shows it.
    fn track_list() -> (ListView<Track>, Arc<Library>) {
        let cfg = Config::new_for_test();
        let ev = EventManager::new_for_test();
        let spotify = Spotify::new_for_test(cfg.clone(), ev.clone());
        let library = Library::new_for_test(ev, spotify.clone(), cfg.clone());
        *library.tracks.write() = (0..30).map(|n| track(&n.to_string(), &[])).collect();
        let queue = Arc::new(Queue::new(spotify, cfg, library.clone()));
        let list = ListView::new(library.tracks.clone(), queue, library.clone());
        (list, library)
    }

    /// A row asks the library if it is saved. With the list's read guard still held, that is a
    /// second read of the store, which waits for any waiting writer, who waits for the guard.
    #[test]
    fn drawing_a_store_list_survives_a_waiting_writer() {
        let (mut list, library) = track_list();
        list.layout(Vec2::new(80, 30));
        let stop = Arc::new(AtomicBool::new(false));
        let writer = {
            let (library, stop) = (library.clone(), stop.clone());
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    drop(library.tracks.write());
                }
            })
        };
        let (done, drawn) = crossbeam_channel::bounded(1);
        std::thread::spawn(move || {
            let theme = Theme::default();
            let mut buffer = PrintBuffer::new();
            buffer.resize(Vec2::new(80, 30));
            let buffer = parking_lot::RwLock::new(buffer);
            let printer = Printer::new((80, 30), &theme, &buffer);
            for _ in 0..300 {
                list.draw(&printer);
            }
            let _ = done.send(());
        });
        let finished = drawn.recv_timeout(Duration::from_secs(30)).is_ok();
        stop.store(true, Ordering::Relaxed);
        assert!(finished, "the UI thread deadlocked on the store lock");
        writer.join().unwrap();
    }

    #[test]
    fn searching_a_store_list_survives_a_waiting_writer() {
        let (list, library) = track_list();
        let stop = Arc::new(AtomicBool::new(false));
        let writer = {
            let (library, stop) = (library.clone(), stop.clone());
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    drop(library.tracks.write());
                }
            })
        };
        let (done, searched) = crossbeam_channel::bounded(1);
        std::thread::spawn(move || {
            for _ in 0..300 {
                list.get_indexes_of("1");
            }
            let _ = done.send(());
        });
        let finished = searched.recv_timeout(Duration::from_secs(30)).is_ok();
        stop.store(true, Ordering::Relaxed);
        assert!(finished, "a search deadlocked on the store lock");
        writer.join().unwrap();
    }

    /// A list item that fails the test if any of its calls runs under a list guard.
    #[derive(Clone)]
    struct Probe {
        id: String,
        plays: u32,
        /// Stands for data a sync refreshes; no action changes it.
        revision: u32,
        list: Arc<std::sync::OnceLock<Arc<RwLock<Vec<Self>>>>>,
        rule: Option<String>,
        tag: Option<String>,
        center: String,
        right: String,
    }

    impl Probe {
        fn assert_unlocked(&self) {
            assert!(
                !self.list.get().unwrap().is_locked(),
                "list guard held across a call"
            );
        }
    }

    impl ListItem for Probe {
        fn is_playing(&self, _queue: &Queue) -> bool {
            false
        }
        fn display_left(&self, _library: &Library) -> String {
            self.id.clone()
        }
        fn display_center(&self, _library: &Library) -> String {
            self.center.clone()
        }
        fn display_right(&self, _library: &Library) -> String {
            self.right.clone()
        }
        fn rule_label(&self) -> Option<String> {
            self.rule.clone()
        }
        fn display_tag(&self, _library: &Library) -> Option<String> {
            self.assert_unlocked();
            self.tag.clone()
        }
        fn play(&mut self, _queue: &Queue) {
            self.assert_unlocked();
            self.plays += 1;
        }
        fn play_next(&mut self, _queue: &Queue) {
            self.assert_unlocked();
            self.plays += 1;
        }
        fn queue(&mut self, _queue: &Queue) {
            self.assert_unlocked();
            self.plays += 1;
        }
        fn toggle_saved(&mut self, _library: &Library) {}
        fn save(&mut self, _library: &Library) {}
        fn unsave(&mut self, _library: &Library) {}
        fn open(&self, _queue: Arc<Queue>, _library: Arc<Library>) -> Option<Box<dyn ViewExt>> {
            self.assert_unlocked();
            None
        }
        fn share_url(&self) -> Option<String> {
            None
        }
        fn album(&self, _queue: &Queue) -> Option<Album> {
            self.assert_unlocked();
            None
        }
        fn id(&self) -> Option<String> {
            Some(self.id.clone())
        }
        fn prepare_open(&mut self, _queue: &Queue) {
            self.assert_unlocked();
            self.plays += 1;
        }
        fn merge_loaded(&mut self, loaded: Self) {
            self.plays = loaded.plays;
        }
        fn as_listitem(&self) -> Box<dyn ListItem> {
            Box::new(self.clone())
        }
    }

    /// A list of `n` probes with row 1 selected, and its content handle.
    fn probe_list(n: usize) -> (ListView<Probe>, Arc<RwLock<Vec<Probe>>>) {
        let cfg = Config::new_for_test();
        let ev = EventManager::new_for_test();
        let spotify = Spotify::new_for_test(cfg.clone(), ev.clone());
        let library = Library::new_for_test(ev, spotify.clone(), cfg.clone());
        let queue = Arc::new(Queue::new(spotify, cfg, library.clone()));
        let cell = Arc::new(std::sync::OnceLock::new());
        let content = Arc::new(RwLock::new(
            (0..n)
                .map(|i| Probe {
                    id: format!("p{i}"),
                    plays: 0,
                    revision: 0,
                    list: cell.clone(),
                    rule: None,
                    tag: None,
                    center: String::new(),
                    right: String::new(),
                })
                .collect::<Vec<_>>(),
        ));
        cell.set(content.clone()).ok().unwrap();
        let mut list = ListView::new(content.clone(), queue, library);
        list.selected = 1;
        (list, content)
    }

    #[test]
    fn play_current_item_holds_no_list_guard() {
        let (mut list, content) = probe_list(3);
        list.play_current_item();
        assert_eq!(content.read()[1].plays, 1);
    }

    #[test]
    fn play_next_command_holds_no_list_guard() {
        let (mut list, content) = probe_list(3);
        let mut siv = cursive::Cursive::new();
        list.on_command(&mut siv, &Command::PlayNext).unwrap();
        assert_eq!(content.read()[1].plays, 1);
    }

    #[test]
    fn queue_command_holds_no_list_guard() {
        let (mut list, content) = probe_list(3);
        let mut siv = cursive::Cursive::new();
        list.on_command(&mut siv, &Command::Queue).unwrap();
        assert_eq!(content.read()[1].plays, 1);
    }

    #[test]
    fn goto_album_holds_no_list_guard() {
        let (mut list, _content) = probe_list(3);
        let mut siv = cursive::Cursive::new();
        let result = list.on_command(&mut siv, &Command::Goto(GotoMode::Album));
        assert!(result.is_ok());
    }

    /// The click path opens the clicked row through `open_selected`.
    #[test]
    fn opening_a_clicked_row_holds_no_list_guard() {
        let (mut list, _content) = probe_list(3);
        list.selected = 2;
        assert!(list.open_selected().is_none());
    }

    #[test]
    fn opening_a_row_keeps_what_the_open_loaded_in_the_row() {
        let (mut list, content) = probe_list(3);
        list.selected = 2;
        assert!(list.open_selected().is_none());
        assert_eq!(content.read()[2].plays, 1);
        let mut siv = cursive::Cursive::new();
        list.on_command(&mut siv, &Command::Open(TargetMode::Selected))
            .unwrap();
        assert_eq!(content.read()[2].plays, 2);
    }

    #[test]
    fn write_back_is_dropped_when_the_item_moved() {
        let (list, content) = probe_list(3);
        list.with_selected_item(|item| {
            let mut moved = content.read()[1].clone();
            moved.id = "other".into();
            *content.write() = vec![moved.clone(), moved.clone(), moved];
            item.plays += 5;
        });
        let content = content.read();
        assert_eq!(content[1].plays, 0);
        assert_eq!(content[1].id, "other");
    }

    #[test]
    fn write_back_keeps_a_refresh_of_the_same_slot() {
        let (list, content) = probe_list(3);
        list.with_selected_item(|item| {
            content.write()[1].revision = 7;
            item.plays += 5;
        });
        let content = content.read();
        assert_eq!(content[1].revision, 7);
        assert_eq!(content[1].plays, 5);
    }

    #[test]
    fn actions_on_a_selection_past_the_end_do_nothing() {
        let (mut list, content) = probe_list(3);
        content.write().truncate(1);
        let mut siv = cursive::Cursive::new();
        list.play_current_item();
        list.on_command(&mut siv, &Command::PlayNext).unwrap();
        list.on_command(&mut siv, &Command::Queue).unwrap();
        list.on_command(&mut siv, &Command::Goto(GotoMode::Album))
            .unwrap();
        assert!(list.with_selected_item(|_| ()).is_none());
        assert_eq!(content.read()[0].plays, 0);
    }

    /// The queue calls in `attempt_play_all_tracks` run after `playable_copy` returns, so this
    /// proves the copy step hands back owned tracks and leaves no guard behind. It does not stop
    /// someone from re-inlining the queue calls under a new guard; the copy step is the only
    /// place that reads the list.
    #[test]
    fn playable_copy_returns_owned_tracks_and_releases_the_guard() {
        let (list, library) = track_list();
        let copy = list.playable_copy().expect("a Vec<Track> list copies");
        assert_eq!(copy.len(), 30);
        assert!(!library.tracks.is_locked());
    }

    /// A plain row whose left text is `left`.
    fn row(left: &str) -> Probe {
        Probe {
            id: left.to_string(),
            plays: 0,
            revision: 0,
            list: Arc::new(std::sync::OnceLock::new()),
            rule: None,
            tag: None,
            center: String::new(),
            right: String::new(),
        }
    }

    /// A rule row; its left text holds a dash and the label, so only the rule check keeps it
    /// out of a search.
    fn rule(label: &str) -> Probe {
        Probe {
            rule: Some(label.to_string()),
            ..row(&format!("rule-{label}"))
        }
    }

    /// A list over `rows` with row 0 selected, and its content handle.
    fn list_of(rows: Vec<Probe>) -> (ListView<Probe>, Arc<RwLock<Vec<Probe>>>) {
        let (mut list, _) = probe_list(0);
        let cell = Arc::new(std::sync::OnceLock::new());
        let rows = rows
            .into_iter()
            .map(|p| Probe {
                list: cell.clone(),
                ..p
            })
            .collect();
        let content = Arc::new(RwLock::new(rows));
        cell.set(content.clone()).ok().unwrap();
        list.content = content.clone();
        list.selected = 0;
        (list, content)
    }

    /// The text of each row of `list` drawn `width` columns wide, one cell per column.
    fn draw_rows(list: &mut ListView<Probe>, width: usize) -> Vec<String> {
        let height = list.content_len(true).max(1);
        let size = Vec2::new(width, height);
        list.layout(size);
        let theme = Theme::default();
        let mut buffer = PrintBuffer::new();
        buffer.resize(size);
        let buffer = parking_lot::RwLock::new(buffer);
        let printer = Printer::new(size, &theme, &buffer);
        list.draw(&printer);
        let buffer = buffer.read();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer.cell_text(Vec2::new(x, y)).unwrap_or(" ").to_string())
                    .collect()
            })
            .collect()
    }

    fn down(list: &mut ListView<Probe>, amount: MoveAmount) {
        let mut siv = cursive::Cursive::new();
        list.on_command(&mut siv, &Command::Move(MoveMode::Down, amount))
            .unwrap();
    }

    fn up(list: &mut ListView<Probe>, amount: MoveAmount) {
        let mut siv = cursive::Cursive::new();
        list.on_command(&mut siv, &Command::Move(MoveMode::Up, amount))
            .unwrap();
    }

    #[test]
    fn a_rule_row_is_drawn_across_the_width_without_a_cut_off_mark() {
        let (mut list, _content) = list_of(vec![rule("Featured playlists")]);
        let lines = draw_rows(&mut list, 30);
        assert_eq!(lines[0], "--- Featured playlists -------");
    }

    /// The column a tag starts in on a row `width` columns wide whose right text fits the
    /// reserve.
    fn tag_column(width: usize) -> usize {
        width - (TAG_RIGHT_RESERVE + 1 + TAG_WIDTH)
    }

    #[test]
    fn a_tag_row_keeps_the_left_text_up_to_the_tag_column() {
        let tagged = |left: String| Probe {
            tag: Some("Artist".into()),
            ..row(&left)
        };
        let tag_x = tag_column(70);
        let whole = "w".repeat(tag_x - 1);
        let long = "x".repeat(50);
        let (mut list, _content) = list_of(vec![tagged(whole.clone()), tagged(long)]);
        let lines = draw_rows(&mut list, 70);
        assert_eq!(&lines[0][..tag_x - 1], whole);
        assert_eq!(&lines[0][tag_x - 1..tag_x], " ");
        assert_eq!(&lines[0][tag_x..tag_x + 6], "Artist");
        assert_eq!(&lines[1][..tag_x - 3], "x".repeat(tag_x - 3));
        assert_eq!(&lines[1][tag_x - 3..tag_x - 1], "..");
        assert_eq!(&lines[1][tag_x - 1..tag_x], " ");
        assert_eq!(&lines[1][tag_x..tag_x + 6], "Artist");
    }

    #[test]
    fn tags_start_in_the_same_column_for_every_right_text() {
        let rights = [
            "3:41".to_string(),
            "2019".into(),
            String::new(),
            "r".repeat(18),
            "1:02:03 [2024-05-01]".into(),
        ];
        let rows = rights
            .iter()
            .enumerate()
            .map(|(i, right)| Probe {
                tag: Some("Track".into()),
                right: right.clone(),
                ..row(&format!("r{i}"))
            })
            .collect();
        let (mut list, _content) = list_of(rows);
        let lines = draw_rows(&mut list, 70);
        let columns: Vec<Option<usize>> = lines.iter().map(|l| l.find("Track")).collect();
        assert_eq!(rights[4].len(), 20);
        assert_eq!(columns, vec![Some(tag_column(70)); rights.len()]);
    }

    #[test]
    fn a_tag_row_ignores_the_center_text() {
        let (mut list, _content) = list_of(vec![Probe {
            tag: Some("Track".into()),
            center: "Album X".into(),
            ..row("left")
        }]);
        let lines = draw_rows(&mut list, 70);
        assert!(!lines[0].contains("Album X"));
        assert!(lines[0].contains("Track"));
    }

    #[test]
    fn a_terminal_too_narrow_for_the_tag_draws_no_tag_and_does_not_panic() {
        for width in [0, 1, 5, 12, 30] {
            let (mut list, _content) = list_of(vec![Probe {
                tag: Some("Tag".into()),
                right: "3:41".into(),
                ..row("left text")
            }]);
            let lines = draw_rows(&mut list, width);
            assert!(!lines[0].contains("Tag"), "width {width}");
        }
    }

    #[test]
    fn rows_without_tag_or_rule_draw_as_before() {
        let (mut list, _content) = list_of(vec![Probe {
            center: "C".into(),
            right: "R".into(),
            ..row("L")
        }]);
        let lines = draw_rows(&mut list, 40);
        assert_eq!(&lines[0][0..1], "L");
        assert_eq!(&lines[0][20..21], "C");
        assert_eq!(&lines[0][39..40], "R");
        assert_eq!(lines[0].matches(|c: char| c != ' ').count(), 3);
    }

    #[test]
    fn moving_down_and_up_steps_over_a_rule_row() {
        let (mut list, _content) = list_of(vec![row("a"), rule("R"), row("b")]);
        down(&mut list, MoveAmount::Integer(1));
        assert_eq!(list.get_selected_index(), 2);
        up(&mut list, MoveAmount::Integer(1));
        assert_eq!(list.get_selected_index(), 0);
    }

    #[test]
    fn page_and_extreme_moves_never_land_on_a_rule_row() {
        let (mut list, _content) = list_of(vec![row("a"), row("b"), rule("R")]);
        down(&mut list, MoveAmount::Extreme);
        assert_eq!(list.get_selected_index(), 1);

        let (mut list, _content) = list_of(vec![rule("R"), row("a")]);
        list.layout(Vec2::new(40, 10));
        assert_eq!(list.get_selected_index(), 1);
        up(&mut list, MoveAmount::Extreme);
        assert_eq!(list.get_selected_index(), 1);

        let rows = vec![row("a"), row("b"), row("c"), rule("R"), row("d"), row("e")];
        let (mut list, _content) = list_of(rows);
        list.layout(Vec2::new(40, 3));
        down(&mut list, MoveAmount::Float(1.0));
        assert_eq!(list.get_selected_index(), 4);
        list.selected = 5;
        list.last_size = Vec2::new(40, 2);
        up(&mut list, MoveAmount::Float(1.0));
        assert_eq!(list.get_selected_index(), 2);
    }

    #[test]
    fn a_list_filled_later_with_a_rule_first_selects_the_row_below() {
        let (mut list, content) = list_of(vec![rule("R"), row("a")]);
        let rows = std::mem::take(&mut *content.write());
        list.layout(Vec2::new(40, 10));
        *content.write() = rows;
        list.layout(Vec2::new(40, 10));
        assert_eq!(list.get_selected_index(), 1);
    }

    #[test]
    fn content_replaced_moves_off_a_rule_row() {
        let (mut list, content) = list_of(vec![row("a"), row("b"), row("c")]);
        list.selected = 1;
        {
            let mut rows = content.write();
            rows[1].rule = Some("R".into());
        }
        list.content_replaced();
        assert_eq!(list.get_selected_index(), 2);
    }

    #[test]
    fn a_content_replace_without_rule_rows_keeps_the_scroll_position() {
        let rows = (0..30).map(|i| row(&format!("p{i}"))).collect();
        let (mut list, _content) = list_of(rows);
        list.layout(Vec2::new(40, 5));
        list.scroller.set_offset((0, 20));
        list.content_replaced();
        assert_eq!(list.get_selected_index(), 0);
        assert_eq!(list.scroller.content_viewport().top_left().y, 20);
    }

    #[test]
    fn a_list_of_only_rule_rows_keeps_its_selection_and_takes_no_command() {
        let (mut list, _content) = list_of(vec![rule("A"), rule("B")]);
        list.layout(Vec2::new(40, 10));
        down(&mut list, MoveAmount::Integer(1));
        assert_eq!(list.get_selected_index(), 0);
        up(&mut list, MoveAmount::Integer(1));
        assert_eq!(list.get_selected_index(), 0);
        down(&mut list, MoveAmount::Extreme);
        assert_eq!(list.get_selected_index(), 0);

        list.queue.append(Playable::Track(track("t1", &[])));
        let mut siv = cursive::Cursive::new();
        let played = list.on_command(&mut siv, &Command::Play);
        assert!(matches!(played, Ok(CommandResult::Consumed(None))));
        assert_eq!(list.queue.len(), 1);

        let opened = list.on_command(&mut siv, &Command::Open(TargetMode::Selected));
        assert!(matches!(opened, Ok(CommandResult::Consumed(None))));

        for cmd in [Command::Quit, Command::TogglePlay] {
            let result = list.on_command(&mut siv, &cmd);
            assert!(matches!(result, Ok(CommandResult::Ignored)));
        }
        assert_eq!(list.queue.len(), 1);
    }

    #[test]
    fn the_in_list_search_does_not_match_a_rule_row() {
        let (list, _content) = list_of(vec![row("a"), rule("Featured playlists")]);
        assert!(list.get_indexes_of("featured").is_empty());
        assert!(!list.get_indexes_of("-").contains(&1));
    }

    #[test]
    fn a_left_or_right_click_on_a_rule_row_changes_nothing() {
        let (mut list, content) = list_of(vec![row("a"), rule("R"), row("b")]);
        list.layout(Vec2::new(40, 10));
        for button in [MouseButton::Left, MouseButton::Right] {
            let result = list.handle_mouse_event(Event::Mouse {
                offset: Vec2::new(0, 0),
                position: Vec2::new(1, 1),
                event: MouseEvent::Press(button),
            });
            assert!(matches!(
                result,
                MouseHandleResult::Handled(EventResult::Consumed(None))
            ));
            assert_eq!(list.get_selected_index(), 0);
        }
        assert!(content.read().iter().all(|p| p.plays == 0));
    }
}
