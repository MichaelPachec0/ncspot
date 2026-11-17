use std::cmp::min;

use cursive::{
    Cursive, Printer, Vec2, View,
    align::HAlign,
    event::{Event, EventResult, MouseButton, MouseEvent},
    theme::ColorStyle,
    view::Nameable,
    views::NamedView,
};
use unicode_width::UnicodeWidthStr;

use crate::{
    command::{Command, MoveAmount, MoveMode},
    commands::CommandResult,
    traits::{BoxedViewExt, IntoBoxedViewExt, ViewExt},
};

/// A view that displays other views in a tab layout.
#[derive(Default)]
pub struct TabbedView {
    /// The list of tabs
    tabs: Vec<NamedView<BoxedViewExt>>,
    /// The index of the currently visible tab from `tabs`
    selected: usize,
    /// The size given to the last call to `layout()`
    last_layout_size: Vec2,
    /// Whether the user has changed the selected tab since this view was built.
    moved_by_user: bool,
}

impl TabbedView {
    pub fn new() -> Self {
        Default::default()
    }

    /// Add `view` as a new tab to the end of this [TabbedView].
    pub fn add_tab(&mut self, title: impl Into<String>, view: impl IntoBoxedViewExt) {
        let tab = BoxedViewExt::new(view.into_boxed_view_ext()).with_name(title);
        self.tabs.push(tab);
    }

    /// Return a mutable reference to the tab at `index`, or None if there is no tab at `index`.
    pub fn tab_mut(&mut self, index: usize) -> Option<&mut NamedView<BoxedViewExt>> {
        self.tabs.get_mut(index)
    }

    /// Return a mutable reference to the selected tab, or None if there is no selected tab
    /// currently.
    pub fn selected_tab_mut(&mut self) -> Option<&mut NamedView<BoxedViewExt>> {
        self.tab_mut(self.selected)
    }

    /// Return the index of the currently visible tab.
    pub fn selected(&self) -> usize {
        self.selected
    }

    /// Return the amount of tabs in this view.
    pub fn len(&self) -> usize {
        self.tabs.len()
    }

    /// Check whether there are tabs in this [TabbedView].
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Set the tab at `index` as currently visible.
    pub fn set_selected(&mut self, index: usize) {
        self.selected = min(self.len().saturating_sub(1), index);
    }

    /// Move the focus by `amount`, clipping at the edges.
    pub fn move_selected(&mut self, amount: isize) {
        self.selected = min(
            self.selected.saturating_add_signed(amount),
            self.len().saturating_sub(1),
        );
    }

    pub fn move_left(&mut self) {
        self.move_selected(-1);
    }

    pub fn move_right(&mut self) {
        self.move_selected(1);
    }

    /// Move the focus to the first tab.
    pub fn select_first(&mut self) {
        self.selected = 0;
    }

    /// Move the focus to the last tab.
    pub fn select_last(&mut self) {
        self.selected = self.len() - 1;
    }

    /// Return whether we are on the first tab.
    pub fn on_first_tab(&mut self) -> bool {
        self.selected == 0
    }

    /// Return whether we are on the last tab.
    pub fn on_last_tab(&mut self) -> bool {
        self.selected == self.len() - 1
    }

    /// Return the width of a single tab.
    ///
    /// Keep in mind that this is an average. It's only provided to make sure all functions use the
    /// same calculation for tab width to prevent off-by-one errors.
    pub fn tab_width(&self) -> usize {
        self.last_layout_size.x / self.len()
    }

    /// Whether the user has asked for another tab since this view was built, also if they
    /// came back. Code that selects a tab does not count.
    pub fn moved_by_user(&self) -> bool {
        self.moved_by_user
    }

    /// Run `user_action` and remember it as a user move if it changed the selected tab.
    fn by_user(&mut self, user_action: impl FnOnce(&mut Self)) {
        let before = self.selected;
        user_action(self);
        if self.selected != before {
            self.moved_by_user = true;
        }
    }

    #[cfg(test)]
    pub(crate) fn names_for_test(&self) -> Vec<String> {
        self.tabs.iter().map(|tab| tab.name().to_string()).collect()
    }
}

impl View for TabbedView {
    fn draw(&self, printer: &Printer<'_, '_>) {
        if self.is_empty() {
            return;
        }

        let tabwidth = self.tab_width();
        for (i, tab) in self.tabs.iter().enumerate() {
            let style = if self.selected == i {
                ColorStyle::highlight()
            } else {
                ColorStyle::primary()
            };

            let mut width = tabwidth;
            if i == self.tabs.len() - 1 {
                width += printer.size.x % self.tabs.len();
            }

            let title = tab.name();
            let offset = HAlign::Center.get_offset(title.width(), width);

            printer.with_color(style, |printer| {
                printer.print_hline((i * tabwidth, 0), width, " ");
                printer.print((i * tabwidth + offset, 0), title);
            });
        }

        if let Some(tab) = self.tabs.get(self.selected) {
            let printer = printer
                .offset((0, 1))
                .cropped((printer.size.x, printer.size.y - 1));

            tab.draw(&printer);
        }
    }

    fn layout(&mut self, size: Vec2) {
        self.last_layout_size = size;
        if let Some(tab) = self.tab_mut(self.selected) {
            tab.layout((size.x, size.y - 1).into())
        }
    }

    fn on_event(&mut self, event: Event) -> EventResult {
        if let Event::Mouse {
            offset,
            position,
            event,
        } = event
        {
            let position = position.checked_sub(offset);
            if let Some(0) = position.map(|p| p.y) {
                self.by_user(|tabs| match event {
                    MouseEvent::WheelUp => tabs.move_left(),
                    MouseEvent::WheelDown => tabs.move_right(),
                    MouseEvent::Press(MouseButton::Left) => {
                        let tabwidth = tabs.tab_width();
                        if let Some(selected_tab) = position.and_then(|p| p.x.checked_div(tabwidth))
                        {
                            tabs.set_selected(selected_tab);
                        }
                    }
                    _ => {}
                });
                return EventResult::consumed();
            }
        }

        if let Some(tab) = self.tab_mut(self.selected) {
            tab.on_event(event.relativized((0, 1)))
        } else {
            EventResult::Ignored
        }
    }
}

impl ViewExt for TabbedView {
    #[cfg(feature = "harness")]
    fn harness_state(&self) -> Option<crate::harness::HarnessState> {
        let tabs = self
            .tabs
            .iter()
            .filter_map(|tab| {
                let state = tab.harness_state()?;
                let mut first = state.tabs.into_iter().next()?;
                first.name = tab.name().to_string();
                Some(first)
            })
            .collect();
        Some(crate::harness::HarnessState {
            view: "tabs".into(),
            tab: self
                .tabs
                .get(self.selected)
                .map(|tab| tab.name().to_string()),
            tabs,
        })
    }

    fn on_command(&mut self, s: &mut Cursive, cmd: &Command) -> Result<CommandResult, String> {
        match cmd {
            Command::Move(mode, amount) if matches!(mode, MoveMode::Left | MoveMode::Right) => {
                self.by_user(|tabs| {
                    if matches!(mode, MoveMode::Left) && !tabs.on_first_tab() {
                        match amount {
                            MoveAmount::Extreme => tabs.select_first(),
                            MoveAmount::Integer(amount) => tabs.move_selected(-(*amount) as isize),
                            _ => (),
                        }
                    } else if matches!(mode, MoveMode::Right) && !tabs.on_last_tab() {
                        match amount {
                            MoveAmount::Extreme => tabs.select_last(),
                            MoveAmount::Integer(amount) => tabs.move_selected(*amount as isize),
                            _ => (),
                        }
                    }
                });
                Ok(CommandResult::Consumed(None))
            }
            _ => {
                if let Some(tab) = self.selected_tab_mut() {
                    tab.on_command(s, cmd)
                } else {
                    Ok(CommandResult::Ignored)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cursive::event::Event;

    struct Blank;

    impl View for Blank {
        fn draw(&self, _printer: &Printer<'_, '_>) {}
    }

    impl ViewExt for Blank {}

    /// Six tabs, each 10 columns wide once laid out.
    fn six_tabs() -> TabbedView {
        let mut tabs = TabbedView::new();
        for name in ["a", "b", "c", "d", "e", "f"] {
            tabs.add_tab(name, Blank);
        }
        tabs.layout(Vec2::new(60, 10));
        tabs
    }

    fn mv(tabs: &mut TabbedView, mode: MoveMode) {
        let mut siv = Cursive::new();
        tabs.on_command(&mut siv, &Command::Move(mode, MoveAmount::Integer(1)))
            .unwrap();
    }

    fn on_bar(tabs: &mut TabbedView, x: usize, event: MouseEvent) {
        tabs.on_event(Event::Mouse {
            offset: Vec2::zero(),
            position: Vec2::new(x, 0),
            event,
        });
    }

    #[test]
    fn a_user_tab_move_is_remembered_and_a_programmatic_one_is_not() {
        let mut tabs = six_tabs();
        assert!(!tabs.moved_by_user());
        mv(&mut tabs, MoveMode::Right);
        mv(&mut tabs, MoveMode::Left);
        assert_eq!(tabs.selected(), 0);
        assert!(tabs.moved_by_user(), "coming back still counts");

        let mut tabs = six_tabs();
        tabs.set_selected(3);
        tabs.select_last();
        tabs.select_first();
        assert!(!tabs.moved_by_user());

        let mut tabs = six_tabs();
        on_bar(&mut tabs, 5, MouseEvent::WheelDown);
        assert_eq!(tabs.selected(), 1);
        assert!(tabs.moved_by_user());

        let mut tabs = six_tabs();
        on_bar(&mut tabs, 25, MouseEvent::Press(MouseButton::Left));
        assert_eq!(tabs.selected(), 2);
        assert!(tabs.moved_by_user());

        let mut tabs = six_tabs();
        on_bar(&mut tabs, 5, MouseEvent::Press(MouseButton::Left));
        mv(&mut tabs, MoveMode::Left);
        on_bar(&mut tabs, 5, MouseEvent::WheelUp);
        assert_eq!(tabs.selected(), 0);
        assert!(!tabs.moved_by_user());
    }
}
