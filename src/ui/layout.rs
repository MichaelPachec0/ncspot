use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

use cursive::align::HAlign;
use cursive::direction::Direction;
use cursive::event::{AnyCb, Event, EventResult, Key, MouseButton, MouseEvent};
use cursive::theme::{ColorStyle, ColorType, Theme};
use cursive::traits::Nameable;
use cursive::traits::View;
use cursive::vec::Vec2;
use cursive::view::{CannotFocus, Selector};
use cursive::views::EditView;
use cursive::views::stack_view::{Fullscreen, Transparent};
use cursive::{Cursive, Printer};
use unicode_width::UnicodeWidthStr;

use crate::application::UserData;
use crate::command::{self, ALL_COMMANDS, Command, JumpMode};
use crate::commands::CommandResult;
use crate::config::{self, Config};
use crate::events;
use crate::ext_traits::CursiveExt;
use crate::traits::{IntoBoxedViewExt, ViewExt};
use crate::ui::statusbar::StatusBar;

/// Draw everything: the default, and what any redraw Cursive starts gets.
const DRAW_ALL: u8 = 0;
/// Draw the status bar only.
const DRAW_STATUSBAR: u8 = 1;
/// Draw the status bar and the current view.
const DRAW_STATUSBAR_AND_VIEW: u8 = 2;

pub struct Layout {
    screens: HashMap<String, Box<dyn ViewExt>>,
    stack: HashMap<String, Vec<Box<dyn ViewExt>>>,
    statusbar: StatusBar,
    /// What the next draw covers, one of the `DRAW_` values; every draw sets it back to
    /// `DRAW_ALL`. The screen buffer keeps what is not drawn again, so the parts left out
    /// stay as they are. That only holds while nothing paints over the layout before it
    /// draws, which is why [Layout::show] adds it as a transparent layer.
    next_draw: AtomicU8,
    /// Rows the command line and result took at the last draw. A change moves the status
    /// bar, so the next draw covers everything.
    drawn_cmdline_height: AtomicUsize,
    focus: Option<String>,
    previous_focus: Option<String>,
    cmdline: EditView,
    cmdline_focus: bool,
    result: Result<Option<String>, String>,
    result_time: Option<SystemTime>,
    last_size: Vec2,
    ev: events::EventManager,
    theme: Theme,
    configuration: Arc<Config>,
    /// Pending count prefix, cleared when a prompt opens so it can't leak onto
    /// the next keypress.
    pending_count: Arc<AtomicUsize>,
}

impl Layout {
    pub fn new(
        status: StatusBar,
        ev: &events::EventManager,
        theme: Theme,
        configuration: Arc<Config>,
        pending_count: Arc<AtomicUsize>,
    ) -> Self {
        let style = ColorStyle::new(
            ColorType::Color(*theme.palette.custom("cmdline").unwrap()),
            ColorType::Color(*theme.palette.custom("cmdline_bg").unwrap()),
        );
        let mut command_line_input = EditView::new().filler(" ").style(style);

        let event_manager = ev.clone();
        // 1. When a search was submitted on the commandline...
        command_line_input.set_on_submit(move |s, cmd| {
            // 2. Clear the commandline on Layout...
            s.on_layout(|_, mut layout| layout.clear_cmdline());

            // 3. Get the actual command without the prefix (like `:` or `/`)...
            let mut command_characters = cmd.chars();
            command_characters.next();
            let cmd_without_prefix = command_characters.as_str();
            if cmd.strip_prefix('/').is_some() {
                // 4. If it is a search command...

                // 5. Send a jump command with the search query to the command manager.
                let command = Command::Jump(JumpMode::Query(cmd_without_prefix.to_string()));
                if let Some(data) = s.user_data::<UserData>().cloned() {
                    data.cmd.handle(s, command);
                }
            } else {
                // 4. If it is an actual command...

                // 5. Parse the command and...
                match command::parse(cmd_without_prefix) {
                    Ok(commands) => {
                        // 6. Send the parsed command to the command manager.
                        if let Some(data) = s.user_data::<UserData>().cloned() {
                            for cmd in commands {
                                data.cmd.handle(s, cmd);
                            }
                        }
                    }
                    Err(err) => {
                        // 6. Set an error message on the global layout.
                        s.on_layout(|_, mut layout| layout.set_result(Err(err.to_string())));
                    }
                }
            }
            event_manager.trigger();
        });

        Self {
            screens: HashMap::new(),
            stack: HashMap::new(),
            statusbar: status,
            next_draw: AtomicU8::new(DRAW_ALL),
            drawn_cmdline_height: AtomicUsize::new(0),
            focus: None,
            previous_focus: None,
            cmdline: command_line_input,
            cmdline_focus: false,
            result: Ok(None),
            result_time: None,
            last_size: Vec2::new(0, 0),
            ev: ev.clone(),
            theme,
            configuration,
            pending_count,
        }
    }

    /// Add the layout to `siv` as its full screen layer, named "main".
    ///
    /// The layer is transparent: Cursive fills the background of any other layer on every
    /// draw, which would blank the parts a playback redraw leaves out. The layout fills its
    /// own background instead, when it draws everything.
    pub fn show(self, siv: &mut Cursive) {
        siv.screen_mut()
            .add_layer(Transparent(Fullscreen(self.with_name("main"))));
    }

    /// Draw the parts the next draw covers, in the view colours `printer` is set to.
    fn draw_parts(&self, printer: &Printer<'_, '_>) {
        let result = self.get_result();

        let cmdline_visible = !self.cmdline.get_content().is_empty();
        let mut cmdline_height = usize::from(cmdline_visible);
        if result.as_ref().map(Option::is_some).unwrap_or(true) {
            cmdline_height += 1;
        }

        // A draw that leaves parts out needs them where the last draw put them.
        let moved = self
            .drawn_cmdline_height
            .swap(cmdline_height, Ordering::Relaxed)
            != cmdline_height;
        let draw = match self.next_draw.swap(DRAW_ALL, Ordering::Relaxed) {
            _ if moved => DRAW_ALL,
            draw => draw,
        };
        if draw != DRAW_ALL {
            if draw == DRAW_STATUSBAR_AND_VIEW
                && let Some(view) = self.get_top_view()
            {
                let printer = &printer
                    .offset((0, 1))
                    .cropped((printer.size.x, printer.size.y - 3 - cmdline_height))
                    .focused(true);
                view.draw(printer);
            }
            self.statusbar
                .draw(&printer.offset((0, printer.size.y - 2 - cmdline_height)));
            return;
        }

        // The background, for the cells no part prints to.
        for y in 0..printer.size.y {
            printer.print_hline((0, y), printer.size.x, " ");
        }

        let screen_title = self
            .get_current_screen()
            .map(|screen| screen.title())
            .unwrap_or_default();

        if let Some(view) = self.get_top_view() {
            // back button + title
            if !self.is_current_stack_empty() {
                printer.with_color(ColorStyle::title_secondary(), |printer| {
                    printer.print((1, 0), &format!("< {screen_title}"));
                });
            }

            // view title
            printer.with_color(ColorStyle::title_primary(), |printer| {
                let offset = HAlign::Center.get_offset(view.title().width(), printer.size.x);
                printer.print((offset, 0), &view.title());
            });

            printer.with_color(ColorStyle::secondary(), |printer| {
                let offset = HAlign::Right.get_offset(view.title_sub().width(), printer.size.x);
                printer.print((offset, 0), &view.title_sub());
            });

            // screen content
            let printer = &printer
                .offset((0, 1))
                .cropped((printer.size.x, printer.size.y - 3 - cmdline_height))
                .focused(true);
            view.draw(printer);
        }

        self.statusbar
            .draw(&printer.offset((0, printer.size.y - 2 - cmdline_height)));

        if let Ok(Some(r)) = result {
            printer.print_hline((0, printer.size.y - cmdline_height), printer.size.x, " ");
            printer.print((0, printer.size.y - cmdline_height), &r);
        } else if let Err(e) = result {
            let style = ColorStyle::new(
                ColorType::Color(*self.theme.palette.custom("error").unwrap()),
                ColorType::Color(*self.theme.palette.custom("error_bg").unwrap()),
            );

            printer.with_color(style, |printer| {
                printer.print_hline((0, printer.size.y - cmdline_height), printer.size.x, " ");
                printer.print((0, printer.size.y - cmdline_height), &format!("ERROR: {e}"));
            });
        }

        if cmdline_visible {
            let printer = &printer.offset((0, printer.size.y - 1));
            self.cmdline.draw(printer);
        }
    }

    /// Get ready for a redraw asked for only because playback moved on. Returns false when
    /// nothing on screen shows a change, and no redraw is needed. Otherwise the next draw
    /// covers the status bar, and the current view if it follows playback, and leaves out
    /// the rest.
    pub fn prepare_playback_redraw(&mut self) -> bool {
        let view = self
            .get_top_view()
            .is_some_and(|view| view.follows_playback());
        if !view && !self.statusbar.playback_changed() {
            return false;
        }
        *self.next_draw.get_mut() = if view {
            DRAW_STATUSBAR_AND_VIEW
        } else {
            DRAW_STATUSBAR
        };
        true
    }

    pub fn enable_cmdline(&mut self, prefix: char) {
        if !self.cmdline_focus {
            self.pending_count.store(0, Ordering::Relaxed);
            self.cmdline.set_content(prefix);
            self.cmdline_focus = true;
        }
    }

    pub fn enable_jump(&mut self) {
        if !self.cmdline_focus {
            self.pending_count.store(0, Ordering::Relaxed);
            self.cmdline.set_content("/");
            self.cmdline_focus = true;
        }
    }

    pub fn add_screen<S: Into<String>, T: IntoBoxedViewExt>(&mut self, id: S, view: T) {
        if let Some(view) = self.get_top_view() {
            view.on_leave();
        }

        let s = id.into();
        self.screens.insert(s.clone(), view.into_boxed_view_ext());
        self.stack.insert(s.clone(), Vec::new());
        self.focus = Some(s);
    }

    pub fn screen<S: Into<String>, T: IntoBoxedViewExt>(mut self, id: S, view: T) -> Self {
        self.add_screen(id, view);
        self
    }

    pub fn has_screen(&self, id: &str) -> bool {
        self.screens.contains_key(id)
    }

    pub fn set_screen<S: Into<String>>(&mut self, id: S) {
        if let Some(view) = self.get_top_view() {
            view.on_leave();
        }

        let s = id.into();
        if let Some(current_focus) = &self.focus
            && current_focus != &s
        {
            self.previous_focus = Some(current_focus.clone());
        }
        self.focus = Some(s);
        self.cmdline_focus = false;

        // trigger a redraw
        self.ev.trigger();
    }

    pub fn set_result(&mut self, result: Result<Option<String>, String>) {
        self.result = result;
        self.result_time = Some(SystemTime::now());
    }

    pub fn clear_cmdline(&mut self) {
        self.cmdline.set_content("");
        self.cmdline_focus = false;
        self.result = Ok(None);
        self.result_time = None;
    }

    /// The id of the playlist shown by the top view, if it is a playlist view.
    pub fn top_playlist_id(&self) -> Option<String> {
        self.get_top_view().and_then(|v| v.playlist_id())
    }

    fn get_result(&self) -> Result<Option<String>, String> {
        if let Some(t) = self.result_time
            && t.elapsed().unwrap() > Duration::from_secs(5)
        {
            return Ok(None);
        }
        self.result.clone()
    }

    pub fn push_view(&mut self, view: Box<dyn ViewExt>) {
        if let Some(view) = self.get_top_view() {
            view.on_leave();
        }

        if let Some(stack) = self.get_focussed_stack_mut() {
            stack.push(view)
        }
    }

    pub fn pop_view(&mut self) {
        if let Some(view) = self.get_top_view() {
            view.on_leave();
        }

        self.get_focussed_stack_mut().map(|stack| stack.pop());
    }

    fn back(&mut self) {
        if !self.is_current_stack_empty() {
            self.pop_view();
            return;
        }

        if self.focus.as_deref() == Some("search")
            && let Some(previous_focus) = self.previous_focus.clone()
            && self.screens.contains_key(&previous_focus)
        {
            self.set_screen(previous_focus);
        }
    }

    #[allow(clippy::borrowed_box)]
    fn get_current_screen(&self) -> Option<&Box<dyn ViewExt>> {
        self.focus
            .as_ref()
            .and_then(|focus| self.screens.get(focus))
    }

    fn get_focussed_stack_mut(&mut self) -> Option<&mut Vec<Box<dyn ViewExt>>> {
        let focus = self.focus.clone();
        if let Some(focus) = &focus {
            self.stack.get_mut(focus)
        } else {
            None
        }
    }

    fn get_focussed_stack(&self) -> Option<&Vec<Box<dyn ViewExt>>> {
        self.focus.as_ref().and_then(|focus| self.stack.get(focus))
    }

    fn is_current_stack_empty(&self) -> bool {
        self.get_focussed_stack()
            .map(|s| s.is_empty())
            .unwrap_or(false)
    }

    /// Title and harness state of the top view, and the last command error.
    #[cfg(feature = "harness")]
    pub fn harness_view(&self) -> (String, Option<crate::harness::HarnessState>, Option<String>) {
        let top = self.get_top_view();
        (
            top.map(|v| v.title()).unwrap_or_default(),
            top.and_then(|v| v.harness_state()),
            self.result.as_ref().err().cloned(),
        )
    }

    #[allow(clippy::borrowed_box)]
    fn get_top_view(&self) -> Option<&Box<dyn ViewExt>> {
        let focussed_stack = self.get_focussed_stack();
        if focussed_stack.map(|s| s.len()).unwrap_or_default() > 0 {
            focussed_stack.unwrap().last()
        } else if let Some(id) = &self.focus {
            self.screens.get(id)
        } else {
            None
        }
    }

    fn get_current_view_mut(&mut self) -> Option<&mut Box<dyn ViewExt>> {
        if let Some(focus) = &self.focus {
            let last_view = self
                .stack
                .get_mut(focus)
                .filter(|stack| !stack.is_empty())
                .and_then(|stack| stack.last_mut());
            if last_view.is_some() {
                last_view
            } else {
                self.screens.get_mut(focus)
            }
        } else {
            None
        }
    }

    /// Propagate the given event to the command line.
    fn command_line_handle_event(&mut self, event: Event) -> EventResult {
        let is_left_right_event = matches!(event, Event::Key(Key::Left) | Event::Key(Key::Right));
        let result = match event {
            // [Key::Tab] isn't actually handled by EditView internally.
            Event::Key(Key::Tab) => self.command_completion(),
            _ => self.cmdline.on_event(event),
        };

        if self.cmdline.get_content().is_empty() {
            self.clear_cmdline();
        }

        if is_left_right_event {
            EventResult::consumed()
        } else {
            result
        }
    }

    /// Completes the command line when the `cmdline` input uniquely matches a command.
    ///
    /// TODO: Add support for aliases.
    ///
    /// # Returns
    ///
    /// [EventResult::Consumed]
    fn command_completion(&mut self) -> EventResult {
        let content = self.cmdline.get_content();

        // Strip the prefix from the content and store it for further processing.
        let mut content_chars = content.chars();
        let prefix = content_chars.next();
        let command_without_prefix = content_chars.as_str();

        let command = word_completion(command_without_prefix, ALL_COMMANDS);
        let _ = match prefix {
            Some(prefix) => self.cmdline.set_content(format!("{prefix}{command}")),
            None => self.cmdline.set_content(command),
        };
        EventResult::consumed()
    }
}

impl View for Layout {
    fn draw(&self, printer: &Printer<'_, '_>) {
        // What Cursive's own layer background does for other layers: the view colours.
        printer.with_color(ColorStyle::view(), |printer| self.draw_parts(printer));
    }

    fn layout(&mut self, size: Vec2) {
        // A new size clears the screen buffer: the draw has to cover everything.
        if size != self.last_size {
            *self.next_draw.get_mut() = DRAW_ALL;
        }
        self.last_size = size;

        self.statusbar.layout(Vec2::new(size.x, 2));

        self.cmdline.layout(Vec2::new(size.x, 1));

        if let Some(view) = self.get_current_view_mut() {
            view.layout(Vec2::new(size.x, size.y - 3));
        }
    }

    fn required_size(&mut self, constraint: Vec2) -> Vec2 {
        Vec2::new(constraint.x, constraint.y)
    }

    fn on_event(&mut self, event: Event) -> EventResult {
        match event {
            Event::Key(Key::Esc) if self.cmdline_focus => {
                self.clear_cmdline();
                EventResult::consumed()
            }
            _ if self.cmdline_focus => self.command_line_handle_event(event),
            Event::Char(character)
                if !self.cmdline_focus
                    && (character
                        == self
                            .configuration
                            .values()
                            .command_key
                            .unwrap_or(config::DEFAULT_COMMAND_KEY)
                        || character == '/') =>
            {
                let result = self
                    .get_current_view_mut()
                    .map(|view| view.on_event(event))
                    .unwrap_or(EventResult::Ignored);

                if let EventResult::Ignored = result {
                    let command_key = self
                        .configuration
                        .values()
                        .command_key
                        .unwrap_or(config::DEFAULT_COMMAND_KEY);

                    if character == command_key {
                        self.enable_cmdline(command_key);
                        EventResult::consumed()
                    } else if character == '/' {
                        self.enable_jump();
                        EventResult::consumed()
                    } else {
                        EventResult::Ignored
                    }
                } else {
                    result
                }
            }
            Event::Mouse {
                position,
                event: mouse_event,
                ..
            } => {
                // Handle mouse events in the command/jump area.
                if position.y == 0 {
                    if mouse_event == MouseEvent::Press(MouseButton::Left)
                        && !self.is_current_stack_empty()
                        && position.x
                            < self
                                .get_current_screen()
                                .map(|screen| screen.title())
                                .unwrap_or_default()
                                .len()
                                + 3
                    {
                        self.pop_view();
                    }
                    return EventResult::consumed();
                }

                let result = self.get_result();

                let cmdline_visible = !self.cmdline.get_content().is_empty();
                let mut cmdline_height = usize::from(cmdline_visible);
                if result.as_ref().map(Option::is_some).unwrap_or(true) {
                    cmdline_height += 1;
                }

                if position.y >= self.last_size.y.saturating_sub(2 + cmdline_height)
                    && position.y < self.last_size.y - cmdline_height
                {
                    self.statusbar.on_event(
                        event.relativized(Vec2::new(0, self.last_size.y - 2 - cmdline_height)),
                    );
                    return EventResult::consumed();
                }

                if let Some(view) = self.get_current_view_mut() {
                    view.on_event(event.relativized((0, 1)))
                } else {
                    EventResult::Ignored
                }
            }
            _ => {
                if let Some(view) = self.get_current_view_mut() {
                    view.on_event(event.relativized((0, 1)))
                } else {
                    EventResult::Ignored
                }
            }
        }
    }

    fn call_on_any(&mut self, s: &Selector, c: AnyCb<'_>) {
        if let Some(view) = self.get_current_view_mut() {
            view.call_on_any(s, c);
        }
    }

    fn take_focus(&mut self, source: Direction) -> Result<EventResult, CannotFocus> {
        if self.cmdline_focus {
            return self.cmdline.take_focus(source);
        }

        if let Some(view) = self.get_current_view_mut() {
            view.take_focus(source)
        } else {
            Err(CannotFocus)
        }
    }
}

impl ViewExt for Layout {
    fn on_command(&mut self, s: &mut Cursive, cmd: &Command) -> Result<CommandResult, String> {
        match cmd {
            Command::Focus(view) => {
                // Clear search results and return to search bar
                // If trying to focus search screen while already on it
                let search_view_name = "search";
                if view == search_view_name
                    && self.focus == Some(search_view_name.into())
                    && let Some(stack) = self.stack.get_mut(search_view_name)
                {
                    stack.clear();
                }

                if self.screens.keys().any(|k| k == view) {
                    self.set_screen(view.clone());
                    let screen = self.screens.get_mut(view).unwrap();
                    screen.on_command(s, cmd)?;
                }

                Ok(CommandResult::Consumed(None))
            }
            Command::Back => {
                self.back();
                Ok(CommandResult::Consumed(None))
            }
            _ => {
                if let Some(view) = self.get_current_view_mut() {
                    view.on_command(s, cmd)
                } else {
                    Ok(CommandResult::Ignored)
                }
            }
        }
    }
}

/// Completes the `input` using words from the `dictionary`.
///
/// # Returns
///
/// If no words match, `input` is returned unchanged. \
/// If a single word matches, that word is returned. \
/// If multiple words match, their common prefix is returned.
fn word_completion(input: &str, dictionary: &[&str]) -> String {
    let matching_words: Vec<_> = dictionary
        .iter()
        .filter(|word| word.starts_with(input))
        .collect();
    let Some(first_word) = matching_words.first() else {
        return input.into();
    };

    let mut prefix: Vec<char> = first_word.chars().collect();

    for word in matching_words {
        let common_len = prefix
            .iter()
            .copied()
            .zip(word.chars())
            .take_while(|(a, b)| a == b)
            .count();

        prefix.truncate(common_len);

        if prefix.is_empty() {
            break;
        }
    }

    prefix.into_iter().collect::<String>()
}

#[cfg(test)]
mod draw_tests {
    use cursive::buffer::PrintBuffer;

    use super::*;
    use crate::events::EventManager;
    use crate::library::Library;
    use crate::queue::Queue;
    use crate::spotify::Spotify;

    /// A view that counts its draws.
    struct Probe {
        draws: Arc<AtomicUsize>,
        follows_playback: bool,
    }

    impl View for Probe {
        fn draw(&self, printer: &Printer<'_, '_>) {
            self.draws.fetch_add(1, Ordering::Relaxed);
            printer.print((0, 0), "PROBE");
        }
    }

    impl ViewExt for Probe {
        fn follows_playback(&self) -> bool {
            self.follows_playback
        }
    }

    /// A layout showing a probe view, and the probe's draw count.
    fn layout(follows_playback: bool) -> (Layout, Arc<AtomicUsize>) {
        let cfg = Config::new_for_test();
        let ev = EventManager::new_for_test();
        let spotify = Spotify::new_for_test(cfg.clone(), ev.clone());
        let library = Library::new_for_test(ev.clone(), spotify.clone(), cfg.clone());
        let queue = Arc::new(Queue::new(spotify, cfg.clone(), library.clone()));
        let count = Arc::new(AtomicUsize::new(0));
        let status = StatusBar::new(queue, library, count.clone());
        let draws = Arc::new(AtomicUsize::new(0));
        let probe = Probe {
            draws: draws.clone(),
            follows_playback,
        };
        let mut layout =
            Layout::new(status, &ev, crate::theme::load(&None), cfg, count).screen("probe", probe);
        layout.set_screen("probe");
        (layout, draws)
    }

    /// Lay out and draw at `size`, as one Cursive refresh does.
    fn refresh(layout: &mut Layout, size: Vec2) {
        layout.layout(size);
        let mut buffer = PrintBuffer::new();
        buffer.resize(size);
        let buffer = parking_lot::RwLock::new(buffer);
        layout.draw(&Printer::new(size, &layout.theme.clone(), &buffer));
    }

    const SIZE: Vec2 = Vec2::new(80, 24);

    #[test]
    fn a_playback_redraw_leaves_out_a_view_that_does_not_follow_playback() {
        let (mut layout, draws) = layout(false);
        refresh(&mut layout, SIZE);
        assert_eq!(draws.load(Ordering::Relaxed), 1);

        // Nothing moved since the draw: no redraw at all.
        assert!(!layout.prepare_playback_redraw());

        *layout.next_draw.get_mut() = DRAW_STATUSBAR;
        refresh(&mut layout, SIZE);
        assert_eq!(draws.load(Ordering::Relaxed), 1, "the view was left out");

        // The draw after it covers everything again.
        refresh(&mut layout, SIZE);
        assert_eq!(draws.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn a_playback_redraw_draws_a_view_that_follows_playback() {
        let (mut layout, draws) = layout(true);
        refresh(&mut layout, SIZE);
        assert!(layout.prepare_playback_redraw());
        refresh(&mut layout, SIZE);
        assert_eq!(draws.load(Ordering::Relaxed), 2);
    }

    /// Through Cursive itself, as the application draws: the view a playback redraw leaves
    /// out has to stay on screen.
    #[test]
    fn a_playback_redraw_keeps_the_view_on_screen() {
        let (layout, draws) = layout(false);
        let backend = cursive::backends::puppet::Backend::init(Some(SIZE));
        let screens = backend.stream();
        let mut siv = cursive::CursiveRunner::new(Cursive::new(), backend);
        siv.set_theme(crate::theme::load(&None));
        layout.show(&mut siv);
        siv.refresh();
        assert_eq!(draws.load(Ordering::Relaxed), 1);

        siv.call_on_name("main", |layout: &mut Layout| {
            *layout.next_draw.get_mut() = DRAW_STATUSBAR;
        });
        siv.refresh();
        assert_eq!(draws.load(Ordering::Relaxed), 1, "the view was left out");
        let screen = screens.try_iter().last().expect("a screen was drawn");
        assert!(
            !screen.find_occurences("PROBE").is_empty(),
            "the view left out is gone from the screen"
        );
    }

    #[test]
    fn a_new_size_draws_everything() {
        let (mut layout, draws) = layout(false);
        refresh(&mut layout, SIZE);
        *layout.next_draw.get_mut() = DRAW_STATUSBAR;
        refresh(&mut layout, Vec2::new(100, 30));
        assert_eq!(draws.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn a_moved_status_bar_draws_everything() {
        let (mut layout, draws) = layout(false);
        refresh(&mut layout, SIZE);
        // A result line takes a row and moves the status bar up.
        layout.set_result(Ok(Some("done".into())));
        *layout.next_draw.get_mut() = DRAW_STATUSBAR;
        refresh(&mut layout, SIZE);
        assert_eq!(draws.load(Ordering::Relaxed), 2);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_DICTIONARY: &[&str] = &["test1", "test2", "tset"];

    #[test]
    fn test_word_completion() {
        // One case from the dictionary matches the input.
        assert_eq!(word_completion("ts", TEST_DICTIONARY), "tset");

        // No cases from the dictionary match the input.
        assert_eq!(word_completion("Luna", TEST_DICTIONARY), "Luna");

        // Multiple cases from the dictionary with common prefix match the input.
        assert_eq!(word_completion("te", TEST_DICTIONARY), "test");

        // Multiple cases from the dictionary without common prefix match the input.
        assert_eq!(word_completion("t", TEST_DICTIONARY), "t");
    }
}
