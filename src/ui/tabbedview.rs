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

/// Blank columns on each side of a tab title when the bar scrolls.
pub(crate) const TAB_PAD: usize = 2;
/// Columns taken by each of the arrows that mark hidden tabs.
pub(crate) const ARROW_WIDTH: usize = 3;
const LEFT_ARROW: &str = "<- ";
const RIGHT_ARROW: &str = " ->";

/// One tab as drawn in the bar.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct BarSlot {
    pub tab: usize,
    pub x: usize,
    pub width: usize,
}

/// What the tab bar shows.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Bar {
    pub slots: Vec<BarSlot>,
    /// Tabs are hidden to the left / right.
    pub left_arrow: bool,
    pub right_arrow: bool,
    /// Index of the first tab shown (0 in fit mode).
    pub first: usize,
}

/// Whether every title is at most as wide as its equal share of `width`, so the bar gives
/// every tab that share, as it did before it could scroll.
fn fits(titles: &[usize], width: usize) -> bool {
    !titles.is_empty() && titles.iter().all(|title| *title <= width / titles.len())
}

/// Pure. `titles` are the display widths of the tab titles, `first` the first tab shown
/// by the previous layout.
///
/// When every title is at most as wide as its equal share of `width`, every tab gets that
/// share. Otherwise each tab is its title padded by `TAB_PAD` on each side, the
/// bar shows as many tabs from `first` on as fit, scrolls just far enough to show
/// `selected`, and marks hidden tabs with an arrow on that side.
pub(crate) fn bar(titles: &[usize], width: usize, selected: usize, first: usize) -> Bar {
    let count = titles.len();
    if count == 0 {
        return Bar {
            slots: Vec::new(),
            left_arrow: false,
            right_arrow: false,
            first: 0,
        };
    }
    let selected = min(selected, count - 1);

    if fits(titles, width) {
        let share = width / count;
        let slots = (0..count)
            .map(|tab| BarSlot {
                tab,
                x: tab * share,
                width: if tab == count - 1 {
                    share + width % count
                } else {
                    share
                },
            })
            .collect();
        return Bar {
            slots,
            left_arrow: false,
            right_arrow: false,
            first: 0,
        };
    }

    let shows = |slots: &[BarSlot], tab: usize| slots.iter().any(|slot| slot.tab == tab);
    let mut first = min(first, selected);
    let mut slots = scrolled_slots(titles, width, first);
    while !shows(&slots, selected) && first < selected {
        first += 1;
        slots = scrolled_slots(titles, width, first);
    }
    if !shows(&slots, selected) {
        slots = cut_slot(width, count, selected, first)
            .into_iter()
            .collect();
    } else {
        while first > 0 {
            let wider = scrolled_slots(titles, width, first - 1);
            if !shows(&wider, selected) || !slots.iter().all(|slot| shows(&wider, slot.tab)) {
                break;
            }
            first -= 1;
            slots = wider;
        }
    }

    // With no slot at all, the selected tab stands for the tab shown.
    let last_shown = slots.last().map_or(selected, |slot| slot.tab);
    Bar {
        slots,
        left_arrow: first > 0,
        right_arrow: last_shown != count - 1,
        first,
    }
}

/// The scrolled tabs from `first` on that fit `width`, left to right.
fn scrolled_slots(titles: &[usize], width: usize, first: usize) -> Vec<BarSlot> {
    let mut x = if first > 0 { ARROW_WIDTH } else { 0 };
    let mut slots = Vec::new();
    for (tab, title) in titles.iter().enumerate().skip(first) {
        let end = x + title + 2 * TAB_PAD;
        if end > slot_limit(width, titles.len(), tab) {
            break;
        }
        slots.push(BarSlot {
            tab,
            x,
            width: end - x,
        });
        x = end;
    }
    slots
}

/// The selected tab alone, cut to the columns between the arrows, or `None` if no column
/// is left.
fn cut_slot(width: usize, count: usize, selected: usize, first: usize) -> Option<BarSlot> {
    let x = if first > 0 { ARROW_WIDTH } else { 0 };
    let end = slot_limit(width, count, selected);
    (end > x).then(|| BarSlot {
        tab: selected,
        x,
        width: end - x,
    })
}

/// The column a slot for `tab` must end by: the last tab may take the whole row, any
/// other leaves room for the right arrow.
fn slot_limit(width: usize, count: usize, tab: usize) -> usize {
    if tab == count - 1 {
        width
    } else {
        width.saturating_sub(ARROW_WIDTH)
    }
}

/// Pure. What a left click at column `x` of the bar means.
#[derive(Debug, PartialEq)]
pub(crate) enum BarClick {
    Tab(usize),
    Left,
    Right,
    Nothing,
}

/// Pure. What a left click at column `x` of `bar`, drawn `width` columns wide, means.
/// The left arrow is drawn over the right one, so it wins where they overlap.
pub(crate) fn bar_click(bar: &Bar, x: usize, width: usize) -> BarClick {
    if bar.left_arrow && x < ARROW_WIDTH {
        BarClick::Left
    } else if bar.right_arrow && x < width && x >= width.saturating_sub(ARROW_WIDTH) {
        BarClick::Right
    } else if let Some(slot) = bar
        .slots
        .iter()
        .find(|slot| slot.x <= x && x < slot.x + slot.width)
    {
        BarClick::Tab(slot.tab)
    } else {
        BarClick::Nothing
    }
}

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
    /// The first tab the bar showed at the last `layout()`.
    first: usize,
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

    /// The display width of each tab title.
    fn title_widths(&self) -> Vec<usize> {
        self.tabs.iter().map(|tab| tab.name().width()).collect()
    }

    /// The bar as the last layout left it. `draw` and the mouse handler both use this, so
    /// what is drawn and what a click hits never differ.
    fn current_bar(&self) -> Bar {
        bar(
            &self.title_widths(),
            self.last_layout_size.x,
            self.selected,
            self.first,
        )
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

        let width = self.last_layout_size.x;
        let fit = fits(&self.title_widths(), width);
        let bar = self.current_bar();
        if !fit {
            printer.with_color(ColorStyle::primary(), |printer| {
                printer.print_hline((0, 0), width, " ");
            });
        }
        for slot in &bar.slots {
            let style = if self.selected == slot.tab {
                ColorStyle::highlight()
            } else {
                ColorStyle::primary()
            };
            let title = self.tabs[slot.tab].name();
            let offset = if fit {
                HAlign::Center.get_offset(title.width(), slot.width)
            } else {
                TAB_PAD
            };

            printer.with_color(style, |printer| {
                let printer = printer.offset((slot.x, 0)).cropped((slot.width, 1));
                printer.print_hline((0, 0), slot.width, " ");
                printer.print((offset, 0), title);
            });
        }
        printer.with_color(ColorStyle::primary(), |printer| {
            if bar.right_arrow {
                printer.print((width.saturating_sub(ARROW_WIDTH), 0), RIGHT_ARROW);
            }
            if bar.left_arrow {
                printer.print((0, 0), LEFT_ARROW);
            }
        });

        if let Some(tab) = self.tabs.get(self.selected) {
            let printer = printer
                .offset((0, 1))
                .cropped((printer.size.x, printer.size.y - 1));

            tab.draw(&printer);
        }
    }

    fn layout(&mut self, size: Vec2) {
        self.last_layout_size = size;
        self.first = self.current_bar().first;
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
                        let Some(position) = position else { return };
                        let width = tabs.last_layout_size.x;
                        match bar_click(&tabs.current_bar(), position.x, width) {
                            BarClick::Tab(tab) => tabs.set_selected(tab),
                            BarClick::Left => tabs.move_left(),
                            BarClick::Right => tabs.move_right(),
                            BarClick::Nothing => {}
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
    use cursive::buffer::PrintBuffer;
    use cursive::event::Event;
    use cursive::theme::{Color, ColorPair, Theme};

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

    const SEARCH_TITLES: [&str; 7] = [
        "Top",
        "Tracks",
        "Albums",
        "Artists",
        "Playlists",
        "Shows",
        "Episodes",
    ];

    /// Display widths of `SEARCH_TITLES`.
    const WIDTHS: [usize; 7] = [3, 6, 6, 7, 9, 5, 8];

    /// A view with one tab per title, laid out `width` columns wide.
    fn named_tabs(titles: &[&str], width: usize) -> TabbedView {
        let mut tabs = TabbedView::new();
        for title in titles {
            tabs.add_tab(*title, Blank);
        }
        tabs.layout(Vec2::new(width, 10));
        tabs
    }

    /// The text and the color pair of each cell of the bar row of `tabs` drawn `width`
    /// columns wide, plus the color pairs the primary and highlight styles resolve to.
    fn draw_bar(
        tabs: &mut TabbedView,
        width: usize,
    ) -> (String, Vec<Option<ColorPair>>, ColorPair, ColorPair) {
        let size = Vec2::new(width, 2);
        tabs.layout(size);
        let theme = Theme::default();
        let mut buffer = PrintBuffer::new();
        buffer.resize(size);
        let buffer = parking_lot::RwLock::new(buffer);
        let printer = Printer::new(size, &theme, &buffer);
        tabs.draw(&printer);
        let buffer = buffer.read();
        let text = (0..width)
            .map(|x| buffer.cell_text(Vec2::new(x, 0)).unwrap_or(" ").to_string())
            .collect();
        let colors = (0..width)
            .map(|x| buffer.cell_style(Vec2::new(x, 0)).map(|style| style.color))
            .collect();
        let default = ColorPair {
            front: Color::TerminalDefault,
            back: Color::TerminalDefault,
        };
        let primary = ColorStyle::primary().resolve(&theme.palette, default);
        let highlight = ColorStyle::highlight().resolve(&theme.palette, default);
        (text, colors, primary, highlight)
    }

    #[test]
    fn the_fit_mode_bar_draws_as_before() {
        let mut tabs = named_tabs(&SEARCH_TITLES[..3], 30);
        tabs.set_selected(1);
        let (text, colors, primary, highlight) = draw_bar(&mut tabs, 30);
        assert_eq!(text, "   Top      Tracks    Albums  ");
        for (x, color) in colors.iter().enumerate() {
            let expected = if (10..20).contains(&x) {
                highlight
            } else {
                primary
            };
            assert_eq!(*color, Some(expected), "column {x}");
        }
    }

    #[test]
    fn the_scroll_mode_bar_draws_titles_padding_and_arrows() {
        let mut tabs = named_tabs(&SEARCH_TITLES, 56);
        tabs.first = 2;
        tabs.set_selected(4);
        let (text, colors, primary, highlight) = draw_bar(&mut tabs, 56);
        assert_eq!(tabs.first, 2);
        let expected = format!(
            "<- {}{}{}{}{}{}",
            "  Albums  ",
            "  Artists  ",
            "  Playlists  ",
            "  Shows  ",
            " ".repeat(7),
            " ->"
        );
        assert_eq!(expected.len(), 56);
        assert_eq!(text, expected);
        for (x, color) in colors.iter().enumerate() {
            let expected = if (24..37).contains(&x) {
                highlight
            } else {
                primary
            };
            assert_eq!(*color, Some(expected), "column {x}");
        }
    }

    fn slot(tab: usize, x: usize, width: usize) -> BarSlot {
        BarSlot { tab, x, width }
    }

    /// The tabs `bar` shows, in order.
    fn shown(bar: &Bar) -> Vec<usize> {
        bar.slots.iter().map(|slot| slot.tab).collect()
    }

    #[test]
    fn the_bar_is_todays_equal_layout_when_every_title_fits() {
        let bar = bar(&WIDTHS, 100, 3, 0);
        let mut slots: Vec<BarSlot> = (0..7).map(|i| slot(i, i * 14, 14)).collect();
        slots[6].width = 16;
        assert_eq!(
            bar,
            Bar {
                slots,
                left_arrow: false,
                right_arrow: false,
                first: 0,
            }
        );
    }

    #[test]
    fn titles_as_wide_as_their_share_or_narrower_fit() {
        let titles = [12, 6, 6, 7, 15];
        let fit = bar(&titles, 80, 0, 0);
        assert_eq!(
            fit,
            Bar {
                slots: (0..5).map(|i| slot(i, i * 16, 16)).collect(),
                left_arrow: false,
                right_arrow: false,
                first: 0,
            }
        );

        let scroll = bar(&titles, 74, 0, 0);
        assert_eq!(
            scroll.slots,
            vec![
                slot(0, 0, 16),
                slot(1, 16, 10),
                slot(2, 26, 10),
                slot(3, 36, 11),
                slot(4, 47, 19),
            ],
            "scroll mode: each tab is its title padded on both sides"
        );
        assert!(!scroll.left_arrow && !scroll.right_arrow);

        let exact = bar(&[12, 6, 6, 7, 16], 80, 0, 0);
        assert_eq!(exact.slots.len(), 5, "a title as wide as its share fits");
        assert!(exact.slots.iter().all(|slot| slot.width == 16));
        assert!(!exact.left_arrow && !exact.right_arrow);
    }

    #[test]
    fn the_bar_scrolls_when_a_title_does_not_fit_its_share() {
        let bar = bar(&WIDTHS, 56, 0, 0);
        assert_eq!(bar.first, 0);
        assert!(!bar.left_arrow);
        assert!(bar.right_arrow);
        assert_eq!(bar.slots[0].x, 0);
        let mut x = 0;
        for slot in &bar.slots {
            assert_eq!(slot.x, x);
            assert_eq!(slot.width, WIDTHS[slot.tab] + 4);
            assert!(slot.x + slot.width <= 53);
            x += slot.width;
        }
        assert_eq!(shown(&bar), vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn the_bar_shifts_only_far_enough_to_show_the_selected_tab() {
        let start = bar(&WIDTHS, 56, 0, 0);
        let five = bar(&WIDTHS, 56, 5, start.first);
        assert_eq!(five.first, 2, "first 1 does not show tab 5");
        assert_eq!(shown(&five), vec![2, 3, 4, 5]);
        assert!(five.left_arrow);
        assert!(five.right_arrow);

        let six = bar(&WIDTHS, 56, 6, five.first);
        assert_eq!(six.first, 3, "first 2 does not show tab 6");
        assert_eq!(shown(&six), vec![3, 4, 5, 6]);
        assert!(six.left_arrow);
        assert!(!six.right_arrow);

        let four = bar(&WIDTHS, 56, 4, six.first);
        assert_eq!(four.first, 3, "tab 4 is still shown");
        assert_eq!(four, six);
    }

    #[test]
    fn moving_left_past_the_first_shown_tab_scrolls_back() {
        let bar = bar(&WIDTHS, 56, 2, 3);
        assert_eq!(bar.first, 2);
        assert_eq!(bar.slots[0], slot(2, 3, 10));
    }

    #[test]
    fn a_wider_terminal_shows_as_many_tabs_as_fit_again() {
        let narrow = bar(&WIDTHS, 56, 6, 3);
        assert_eq!(narrow.first, 3);
        let wider = bar(&WIDTHS, 62, 6, narrow.first);
        assert_eq!(wider.first, 2, "first 1 would hide tab 6");
        assert_eq!(shown(&wider), vec![2, 3, 4, 5, 6]);
        assert!(wider.left_arrow);
        assert!(!wider.right_arrow);
        let wide = bar(&WIDTHS, 100, 6, wider.first);
        assert_eq!(wide.first, 0);
        assert_eq!(shown(&wide), vec![0, 1, 2, 3, 4, 5, 6]);
        assert!(!wide.left_arrow && !wide.right_arrow);
    }

    #[test]
    fn a_bar_too_narrow_for_the_selected_tab_shows_it_cut_and_never_panics() {
        for count in [0, 1, 7] {
            for width in [0, 1, 3, 6, 7] {
                let mut tabs = named_tabs(&SEARCH_TITLES[..count], width);
                let mut first = 0;
                for (selected, title) in WIDTHS.iter().enumerate().take(count.max(1)) {
                    let bar = bar(&WIDTHS[..count], width, selected, first);
                    first = bar.first;
                    if count == 0 || width == 0 {
                        assert!(bar.slots.is_empty(), "{count} tabs, width {width}");
                    }
                    if let [only] = bar.slots.as_slice()
                        && only.width < title + 4
                    {
                        assert_eq!(only.tab, selected, "{count} tabs, width {width}");
                        let start = if bar.first > 0 { ARROW_WIDTH } else { 0 };
                        let end = if selected + 1 == count {
                            width
                        } else {
                            width.saturating_sub(ARROW_WIDTH)
                        };
                        assert_eq!(*only, slot(selected, start, end - start));
                    }
                    tabs.set_selected(selected);
                    draw_bar(&mut tabs, width);
                    for x in 0..width + 2 {
                        on_bar(&mut tabs, x, MouseEvent::Press(MouseButton::Left));
                        tabs.set_selected(selected);
                    }
                }
            }
        }
        let cut = bar(&WIDTHS, 7, 0, 0);
        assert_eq!(cut.slots, vec![slot(0, 0, 4)]);
        assert!(!cut.left_arrow && cut.right_arrow);
        let cut = bar(&WIDTHS, 7, 3, 0);
        assert_eq!(cut.first, 3);
        assert_eq!(cut.slots, vec![slot(3, 3, 1)]);
        assert!(cut.left_arrow && cut.right_arrow);
        let none = bar(&WIDTHS, 6, 3, 0);
        assert!(
            none.slots.is_empty(),
            "zero columns left between the arrows"
        );
        assert!(none.left_arrow && none.right_arrow);
        let last = bar(&WIDTHS, 7, 6, 0);
        assert_eq!(last.slots, vec![slot(6, 3, 4)]);
        assert!(last.left_arrow && !last.right_arrow);
    }

    #[test]
    fn no_slot_overlaps_another_or_an_arrow() {
        for width in 0..=120 {
            let mut first = 0;
            for selected in 0..7 {
                let bar = bar(&WIDTHS, width, selected, first);
                first = bar.first;
                let low = if bar.left_arrow { ARROW_WIDTH } else { 0 };
                let high = if bar.right_arrow {
                    width.saturating_sub(ARROW_WIDTH)
                } else {
                    width
                };
                let mut end = low;
                for slot in &bar.slots {
                    assert!(slot.width > 0, "width {width}, selected {selected}");
                    assert!(slot.x >= end, "width {width}, selected {selected}");
                    end = slot.x + slot.width;
                    assert!(end <= high, "width {width}, selected {selected}");
                }
                assert!(bar.slots.windows(2).all(|pair| pair[0].tab < pair[1].tab));
                if !bar.slots.is_empty() {
                    assert!(
                        shown(&bar).contains(&selected),
                        "width {width}, selected {selected}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_click_maps_to_the_tab_under_it_or_to_an_arrow() {
        let scroll = bar(&WIDTHS, 56, 4, 2);
        assert!(scroll.left_arrow && scroll.right_arrow);
        for slot in &scroll.slots {
            assert_eq!(bar_click(&scroll, slot.x, 56), BarClick::Tab(slot.tab));
            let last = slot.x + slot.width - 1;
            assert_eq!(bar_click(&scroll, last, 56), BarClick::Tab(slot.tab));
        }
        for x in 0..3 {
            assert_eq!(bar_click(&scroll, x, 56), BarClick::Left);
        }
        for x in 53..56 {
            assert_eq!(bar_click(&scroll, x, 56), BarClick::Right);
        }
        assert_eq!(bar_click(&scroll, 46, 56), BarClick::Nothing);
        assert_eq!(bar_click(&scroll, 52, 56), BarClick::Nothing);

        let start = bar(&WIDTHS, 56, 0, 0);
        assert!(!start.left_arrow);
        assert_eq!(bar_click(&start, 0, 56), BarClick::Tab(0));

        let fit = bar(&WIDTHS, 100, 0, 0);
        for x in 0..100 {
            assert_eq!(bar_click(&fit, x, 100), BarClick::Tab(min(x / 14, 6)));
        }
    }

    #[test]
    fn clicking_an_arrow_moves_one_tab_and_counts_as_a_user_move() {
        let mut tabs = named_tabs(&SEARCH_TITLES, 56);
        tabs.set_selected(6);
        tabs.layout(Vec2::new(56, 10));
        assert!(!tabs.moved_by_user());
        on_bar(&mut tabs, 1, MouseEvent::Press(MouseButton::Left));
        assert_eq!(tabs.selected(), 5);
        assert!(tabs.moved_by_user());

        let mut tabs = named_tabs(&SEARCH_TITLES, 56);
        on_bar(&mut tabs, 55, MouseEvent::Press(MouseButton::Left));
        assert_eq!(tabs.selected(), 1);
        assert!(tabs.moved_by_user());
    }

    #[test]
    fn clicking_the_selected_tab_does_not_count_as_a_user_move() {
        let mut tabs = named_tabs(&SEARCH_TITLES, 56);
        on_bar(&mut tabs, 1, MouseEvent::Press(MouseButton::Left));
        assert_eq!(tabs.selected(), 0);
        assert!(!tabs.moved_by_user());

        let mut tabs = named_tabs(&SEARCH_TITLES, 56);
        tabs.set_selected(4);
        tabs.layout(Vec2::new(56, 10));
        on_bar(&mut tabs, 52, MouseEvent::Press(MouseButton::Left));
        assert_eq!(tabs.selected(), 4, "a gap selects nothing");
        assert!(!tabs.moved_by_user());
    }
}
