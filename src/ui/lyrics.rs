use std::sync::{Arc, RwLock};

use cursive::Cursive;
use cursive::event::{Event, EventResult, Key, MouseButton, MouseEvent};
use cursive::theme::{ColorStyle, ColorType, Effect, PaletteColor};
use cursive::{Printer, Vec2, View};
use unicode_width::UnicodeWidthStr;

use crate::command::Command;
use crate::commands::CommandResult;
use crate::config::{Config, LyricsConfig};
use crate::events::EventManager;
use crate::lyrics::model::{Lyrics, LyricsState, TrackMeta};
use crate::lyrics::{FetchOutcome, LyricsManager};
use crate::queue::Queue;
use crate::traits::{ListItem, ViewExt};

/// How a single rendered line should be styled.
enum RowStyle {
    Active,
    Normal,
    Secondary,
}

/// Cached row→line layout from the last draw, used to hit-test clicks.
#[derive(Default)]
struct LineLayout {
    /// Index of the first rendered row currently visible at the top.
    top: usize,
    /// Owning `Lyrics::lines` index for every rendered row, in draw order.
    row_line: Vec<Option<usize>>,
}

/// Playback position (ms) to seek to so that a line with `start_ms` becomes the
/// active line under the current sync `offset_ms`. Inverts the `active_index`
/// comparison (`progress + offset >= start_ms`). Clamped at zero.
fn seek_target_ms(start_ms: u32, offset_ms: i64) -> u32 {
    (start_ms as i64 - offset_ms).max(0) as u32
}

/// Resolve a click at visible row `rel_y` (0 = top visible row) to the owning
/// lyric-line index, or `None` if the click lands below the last rendered row.
fn row_at(rel_y: usize, top: usize, row_line: &[Option<usize>]) -> Option<usize> {
    row_line.get(top + rel_y).copied().flatten()
}

/// Next cursor position. From `None`, reveal at the active line (or line 0)
/// without moving; from `Some`, step by `delta` clamped to `[0, len-1]`.
/// `None` when there are no lines.
fn move_cursor(
    current: Option<usize>,
    active: Option<usize>,
    len: usize,
    delta: i64,
) -> Option<usize> {
    if len == 0 {
        return None;
    }
    match current {
        None => Some(active.unwrap_or(0).min(len - 1)),
        Some(c) => Some((c as i64 + delta).clamp(0, len as i64 - 1) as usize),
    }
}

/// Full-screen page that renders the lyrics of the currently playing track.
///
/// Modeled on [`crate::ui::cover::CoverView`]: it polls the queue in `draw`,
/// detects track changes by comparing a cache key against [`Self::fetched_for`],
/// and kicks a background fetch whose result is written into [`Self::state`]
/// before nudging the Cursive event loop via [`EventManager::trigger`].
pub struct LyricsView {
    queue: Arc<Queue>,
    cfg: Arc<Config>,
    events: EventManager,
    manager: Arc<LyricsManager>,
    state: Arc<RwLock<LyricsState>>,
    /// Cache key of the track we last kicked a fetch for (track-change detection).
    fetched_for: RwLock<Option<String>>,
    /// Selected line index; `None` means auto-follow the active playing line.
    cursor_line: RwLock<Option<usize>>,
    /// Row→line layout cached by `draw_lyrics`, read by `on_event` for clicks.
    last_layout: RwLock<LineLayout>,
    /// Sync offset in ms, added to the playback position.
    offset_ms: RwLock<i64>,
}

impl LyricsView {
    pub fn new(
        queue: Arc<Queue>,
        cfg: Arc<Config>,
        events: EventManager,
        manager: Arc<LyricsManager>,
    ) -> Self {
        Self {
            queue,
            cfg,
            events,
            manager,
            state: Arc::new(RwLock::new(LyricsState::Idle)),
            fetched_for: RwLock::new(None),
            cursor_line: RwLock::new(None),
            last_layout: RwLock::new(LineLayout::default()),
            offset_ms: RwLock::new(0),
        }
    }

    /// Build the metadata needed to look up lyrics from the current queue item.
    ///
    /// Returns `None` when nothing is playing or the item carries no track
    /// metadata (e.g. a podcast episode), in which case no fetch is attempted.
    fn track_meta(&self) -> Option<TrackMeta> {
        let playable = self.queue.get_current()?;
        let track = playable.track()?;
        Some(TrackMeta {
            spotify_id: playable.id(),
            title: track.title,
            artist: track.artists.join(", "),
            album: track.album,
            duration_ms: playable.duration(),
        })
    }

    /// Stable key used to detect track changes and to drop stale fetch results.
    fn track_key(meta: &TrackMeta) -> String {
        meta.spotify_id
            .clone()
            .unwrap_or_else(|| format!("{}|{}", meta.title, meta.artist))
    }

    /// Lazily kick a background fetch whenever the current track changes.
    fn ensure_fetch(&self) {
        let Some(meta) = self.track_meta() else {
            return;
        };
        let key = Self::track_key(&meta);

        if self.fetched_for.read().unwrap().as_deref() == Some(key.as_str()) {
            return;
        }

        *self.fetched_for.write().unwrap() = Some(key.clone());
        *self.cursor_line.write().unwrap() = None;
        *self.offset_ms.write().unwrap() = 0;
        *self.state.write().unwrap() = LyricsState::Loading {
            track_id: key.clone(),
        };

        let manager = self.manager.clone();
        let cfg = self.cfg.clone();
        let events = self.events.clone();
        let state = self.state.clone();

        std::thread::spawn(move || {
            let order = cfg
                .values()
                .lyrics
                .clone()
                .unwrap_or_default()
                .provider_order();
            let next = match manager.fetch(&cfg, &order, &meta) {
                FetchOutcome::Found(lyrics) => LyricsState::Loaded {
                    track_id: key.clone(),
                    lyrics,
                },
                FetchOutcome::NotFound { tried } => LyricsState::NotFound {
                    track_id: key.clone(),
                    tried,
                },
                FetchOutcome::Error { message } => LyricsState::Error {
                    track_id: key.clone(),
                    message,
                },
            };

            let mut guard = state.write().unwrap();
            // Only apply if the user hasn't moved on to a different track.
            let still_current = matches!(
                &*guard,
                LyricsState::Loading { track_id }
                    | LyricsState::Loaded { track_id, .. }
                    | LyricsState::NotFound { track_id, .. }
                    | LyricsState::Error { track_id, .. }
                    if *track_id == key
            );
            if still_current {
                *guard = next;
                drop(guard);
                events.trigger();
            }
        });
    }

    /// Current playback position in ms with the manual sync offset applied.
    fn position_ms(&self) -> i64 {
        let progress = self.queue.get_spotify().get_current_progress().as_millis() as i64;
        progress + *self.offset_ms.read().unwrap()
    }

    /// Move the selection cursor by `delta` lines, seeding from the active line.
    fn move_cursor_by(&self, delta: i64) {
        let (len, active) = {
            let guard = self.state.read().unwrap();
            match &*guard {
                LyricsState::Loaded { lyrics, .. } => {
                    let active = if lyrics.synced {
                        lyrics.active_index(self.position_ms().max(0) as u32)
                    } else {
                        None
                    };
                    (lyrics.lines.len(), active)
                }
                _ => (0, None),
            }
        };
        let current = *self.cursor_line.read().unwrap();
        *self.cursor_line.write().unwrap() = move_cursor(current, active, len, delta);
    }

    /// Seek to line `line_idx`'s timestamp (offset-adjusted), then resume
    /// auto-follow by clearing the cursor. No-op for unsynced lines or lines
    /// without a timestamp.
    fn seek_to_line(&self, line_idx: usize) {
        let target = {
            let guard = self.state.read().unwrap();
            match &*guard {
                LyricsState::Loaded { lyrics, .. } if lyrics.synced => lyrics
                    .lines
                    .get(line_idx)
                    .and_then(|l| l.start_ms)
                    .map(|start| seek_target_ms(start, *self.offset_ms.read().unwrap())),
                _ => None,
            }
        };
        if let Some(ms) = target {
            self.queue.get_spotify().seek(ms);
            *self.cursor_line.write().unwrap() = None;
        }
    }

    /// Seek to the selected line, or the active playing line if no cursor is set.
    fn seek_to_selected(&self) {
        let line = {
            let guard = self.state.read().unwrap();
            match &*guard {
                LyricsState::Loaded { lyrics, .. } if lyrics.synced => self
                    .cursor_line
                    .read()
                    .unwrap()
                    .or_else(|| lyrics.active_index(self.position_ms().max(0) as u32)),
                _ => None,
            }
        };
        if let Some(i) = line {
            self.seek_to_line(i);
        }
    }

    /// Handle a left-click at view-relative row `rel_y`: seek to the clicked line.
    fn handle_click(&self, rel_y: usize) {
        let line = {
            let layout = self.last_layout.read().unwrap();
            row_at(rel_y, layout.top, &layout.row_line)
        };
        if let Some(i) = line {
            self.seek_to_line(i);
        }
    }

    fn adjust_offset(&self, delta_ms: i64) {
        *self.offset_ms.write().unwrap() += delta_ms;
    }

    /// Force a re-evaluation on the next draw (used by refetch/provider cycle).
    fn force_refetch(&self) {
        *self.fetched_for.write().unwrap() = None;
        *self.cursor_line.write().unwrap() = None;
        *self.state.write().unwrap() = LyricsState::Idle;
    }

    #[cfg(feature = "share_clipboard")]
    fn copy_line(&self) {
        let line = {
            let guard = self.state.read().unwrap();
            match &*guard {
                LyricsState::Loaded { lyrics, .. } => lyrics
                    .active_index(self.position_ms().max(0) as u32)
                    .and_then(|i| lyrics.lines.get(i))
                    .map(|l| l.text.clone()),
                _ => None,
            }
        };
        if let Some(text) = line {
            let _ = crate::sharing::write_share(text);
        }
    }

    #[cfg(not(feature = "share_clipboard"))]
    fn copy_line(&self) {}

    #[cfg(feature = "share_clipboard")]
    fn copy_all(&self) {
        let all = {
            let guard = self.state.read().unwrap();
            match &*guard {
                LyricsState::Loaded { lyrics, .. } => Some(
                    lyrics
                        .lines
                        .iter()
                        .map(|l| l.text.as_str())
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
                _ => None,
            }
        };
        if let Some(text) = all {
            let _ = crate::sharing::write_share(text);
        }
    }

    #[cfg(not(feature = "share_clipboard"))]
    fn copy_all(&self) {}

    /// Render a centered single-line status string ("Loading…", etc.).
    fn draw_status(&self, printer: &Printer, msg: &str) {
        let x = printer.size.x.saturating_sub(msg.width()) / 2;
        let y = printer.size.y / 2;
        printer.print((x, y), msg);
    }

    fn draw_lyrics(&self, printer: &Printer, lyrics: &Lyrics, cfg: &LyricsConfig) {
        if lyrics.lines.is_empty() {
            self.draw_status(printer, "No lyrics found");
            return;
        }

        let height = printer.size.y;
        let width = printer.size.x;
        if height == 0 {
            return;
        }

        let show_translation = cfg.show_translation.unwrap_or(false);
        let show_romaji = cfg.show_romaji.unwrap_or(false);

        let active = if lyrics.synced {
            lyrics.active_index(self.position_ms().max(0) as u32)
        } else {
            None
        };

        // Flatten the lyrics into rendered rows so translation/romanization
        // lines participate in scrolling and centering, tracking which line
        // each row belongs to for click hit-testing.
        let cursor = *self.cursor_line.read().unwrap();
        let mut rows: Vec<(&str, RowStyle)> = Vec::new();
        let mut row_line: Vec<Option<usize>> = Vec::new();
        let mut active_row: Option<usize> = None;
        let mut cursor_row: Option<usize> = None;
        for (i, line) in lyrics.lines.iter().enumerate() {
            let is_active = Some(i) == active;
            if is_active {
                active_row = Some(rows.len());
            }
            if Some(i) == cursor {
                cursor_row = Some(rows.len());
            }
            rows.push((
                line.text.as_str(),
                if is_active {
                    RowStyle::Active
                } else {
                    RowStyle::Normal
                },
            ));
            row_line.push(Some(i));
            if show_translation && let Some(translation) = &line.translation {
                rows.push((translation.as_str(), RowStyle::Secondary));
                row_line.push(Some(i));
            }
            if show_romaji && let Some(romanization) = &line.romanization {
                rows.push((romanization.as_str(), RowStyle::Secondary));
                row_line.push(Some(i));
            }
        }

        // Center on the cursor when the user has selected a line, else on the
        // active playing line (auto-follow).
        let top = match cursor_row {
            Some(cr) => cr.saturating_sub(height / 2),
            None => active_row
                .map(|a| a.saturating_sub(height / 2))
                .unwrap_or(0),
        };

        *self.last_layout.write().unwrap() = LineLayout { top, row_line };

        let highlight = ColorStyle::new(
            ColorType::Color(*printer.theme.palette.custom("lyrics_highlight").unwrap()),
            ColorType::Palette(PaletteColor::Background),
        );
        let secondary = ColorStyle::new(
            ColorType::Color(*printer.theme.palette.custom("lyrics_secondary").unwrap()),
            ColorType::Palette(PaletteColor::Background),
        );
        let normal = ColorStyle::primary();

        for (visible_y, row_idx) in (top..rows.len()).take(height).enumerate() {
            let (text, style) = &rows[row_idx];
            let color = match style {
                RowStyle::Active => highlight,
                RowStyle::Secondary => secondary,
                RowStyle::Normal => normal,
            };
            let x = if lyrics.rtl {
                width.saturating_sub(text.width())
            } else {
                0
            };
            let is_cursor = Some(row_idx) == cursor_row;
            printer.with_color(color, |printer| {
                if is_cursor {
                    printer.with_effect(Effect::Reverse, |printer| {
                        printer.print((x, visible_y), text)
                    });
                } else {
                    printer.print((x, visible_y), text);
                }
            });
        }
    }
}

impl View for LyricsView {
    fn draw(&self, printer: &Printer<'_, '_>) {
        self.ensure_fetch();

        let lyrics_cfg = self.cfg.values().lyrics.clone().unwrap_or_default();
        let guard = self.state.read().unwrap();
        match &*guard {
            LyricsState::Loaded { lyrics, .. } => self.draw_lyrics(printer, lyrics, &lyrics_cfg),
            LyricsState::Loading { .. } => self.draw_status(printer, "Loading…"),
            LyricsState::NotFound { tried, .. } => {
                let msg = if tried.is_empty() {
                    "No lyrics found".to_string()
                } else {
                    let names: Vec<&str> = tried.iter().map(|p| p.as_str()).collect();
                    format!("No lyrics found (tried: {})", names.join(", "))
                };
                self.draw_status(printer, &msg);
            }
            LyricsState::Error { message, .. } => {
                self.draw_status(printer, &format!("Lyrics unavailable: {message}"));
            }
            LyricsState::Idle => {
                let msg = match self.queue.get_current() {
                    None => "Nothing playing",
                    Some(playable) if playable.track().is_none() => "No lyrics for this item",
                    Some(_) => "Loading…",
                };
                self.draw_status(printer, msg);
            }
        }
    }

    fn required_size(&mut self, constraint: Vec2) -> Vec2 {
        constraint
    }

    fn layout(&mut self, _: Vec2) {}

    fn on_event(&mut self, event: Event) -> EventResult {
        let cfg = self.cfg.values().lyrics.clone().unwrap_or_default();
        let allow_scroll = cfg.allow_scroll.unwrap_or(true);
        let allow_offset = cfg.allow_offset.unwrap_or(true);
        let allow_copy = cfg.allow_copy.unwrap_or(true);
        let allow_seek = cfg.allow_seek.unwrap_or(true);

        match event {
            Event::Char('j') | Event::Key(Key::Down) if allow_scroll => {
                self.move_cursor_by(1);
                EventResult::Consumed(None)
            }
            Event::Char('k') | Event::Key(Key::Up) if allow_scroll => {
                self.move_cursor_by(-1);
                EventResult::Consumed(None)
            }
            Event::Key(Key::Enter) if allow_seek => {
                self.seek_to_selected();
                EventResult::Consumed(None)
            }
            // Clear the selection (resume auto-follow) only when one is set, so
            // an unselected Esc still falls through to the global handler.
            Event::Key(Key::Esc) if self.cursor_line.read().unwrap().is_some() => {
                *self.cursor_line.write().unwrap() = None;
                EventResult::Consumed(None)
            }
            Event::Char('[') if allow_offset => {
                self.adjust_offset(-500);
                EventResult::Consumed(None)
            }
            Event::Char(']') if allow_offset => {
                self.adjust_offset(500);
                EventResult::Consumed(None)
            }
            Event::Char('y') if allow_copy => {
                self.copy_line();
                EventResult::Consumed(None)
            }
            Event::Char('Y') if allow_copy => {
                self.copy_all();
                EventResult::Consumed(None)
            }
            Event::Mouse {
                offset,
                position,
                event,
            } => {
                let rel = position.saturating_sub(offset);
                match event {
                    MouseEvent::WheelUp if allow_scroll => {
                        self.move_cursor_by(-1);
                        EventResult::Consumed(None)
                    }
                    MouseEvent::WheelDown if allow_scroll => {
                        self.move_cursor_by(1);
                        EventResult::Consumed(None)
                    }
                    MouseEvent::Press(MouseButton::Left) if allow_seek => {
                        self.handle_click(rel.y);
                        EventResult::Consumed(None)
                    }
                    _ => EventResult::Ignored,
                }
            }
            _ => EventResult::Ignored,
        }
    }
}

impl ViewExt for LyricsView {
    fn title(&self) -> String {
        "Lyrics".to_string()
    }

    fn on_command(&mut self, _s: &mut Cursive, cmd: &Command) -> Result<CommandResult, String> {
        let cfg = self.cfg.values().lyrics.clone().unwrap_or_default();

        // Intercept every lyrics command on this screen so the global handler's
        // "unsupported in this view" error never surfaces. Each action is gated
        // by its `allow_*` flag, but the command is always consumed.
        match cmd {
            Command::LyricsScrollUp => {
                if cfg.allow_scroll.unwrap_or(true) {
                    self.move_cursor_by(-1);
                }
                Ok(CommandResult::Consumed(None))
            }
            Command::LyricsScrollDown => {
                if cfg.allow_scroll.unwrap_or(true) {
                    self.move_cursor_by(1);
                }
                Ok(CommandResult::Consumed(None))
            }
            Command::LyricsSeek => {
                if cfg.allow_seek.unwrap_or(true) {
                    self.seek_to_selected();
                }
                Ok(CommandResult::Consumed(None))
            }
            Command::LyricsOffset(delta) => {
                if cfg.allow_offset.unwrap_or(true) {
                    self.adjust_offset(*delta);
                }
                Ok(CommandResult::Consumed(None))
            }
            Command::LyricsProviderCycle => {
                if cfg.allow_provider_switch.unwrap_or(true) {
                    self.force_refetch();
                }
                Ok(CommandResult::Consumed(None))
            }
            Command::LyricsRefetch => {
                if let Some(meta) = self.track_meta() {
                    self.manager.invalidate(&meta);
                }
                self.force_refetch();
                Ok(CommandResult::Consumed(None))
            }
            Command::LyricsCopyLine => {
                if cfg.allow_copy.unwrap_or(true) {
                    self.copy_line();
                }
                Ok(CommandResult::Consumed(None))
            }
            Command::LyricsCopyAll => {
                if cfg.allow_copy.unwrap_or(true) {
                    self.copy_all();
                }
                Ok(CommandResult::Consumed(None))
            }
            _ => Ok(CommandResult::Ignored),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seek_target_subtracts_offset_and_clamps() {
        assert_eq!(seek_target_ms(10_000, 0), 10_000);
        assert_eq!(seek_target_ms(10_000, 500), 9_500); // positive offset -> earlier seek
        assert_eq!(seek_target_ms(10_000, -500), 10_500); // negative offset -> later seek
        assert_eq!(seek_target_ms(300, 1_000), 0); // clamp at zero
    }

    #[test]
    fn row_at_resolves_visible_row_to_owning_line() {
        // Rows: line0 main, line0 translation, line1 main, line2 main.
        let row_line = vec![Some(0), Some(0), Some(1), Some(2)];
        // top = 1: first visible row is index 1 (line0's translation).
        assert_eq!(row_at(0, 1, &row_line), Some(0)); // click translation -> parent line 0
        assert_eq!(row_at(1, 1, &row_line), Some(1));
        assert_eq!(row_at(2, 1, &row_line), Some(2));
        assert_eq!(row_at(3, 1, &row_line), None); // below the last row
    }

    #[test]
    fn move_cursor_reveals_at_active_then_steps_and_clamps() {
        // From None: reveal at the active line, ignoring delta.
        assert_eq!(move_cursor(None, Some(3), 10, 1), Some(3));
        assert_eq!(move_cursor(None, None, 10, -1), Some(0)); // no active -> line 0
        // From Some: step and clamp to [0, len-1].
        assert_eq!(move_cursor(Some(3), Some(3), 10, 1), Some(4));
        assert_eq!(move_cursor(Some(0), None, 10, -1), Some(0)); // clamp low
        assert_eq!(move_cursor(Some(9), None, 10, 1), Some(9)); // clamp high
        assert_eq!(move_cursor(Some(0), None, 0, 1), None); // empty list
    }
}
