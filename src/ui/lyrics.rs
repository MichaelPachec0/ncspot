use std::sync::{Arc, RwLock};

use cursive::Cursive;
use cursive::event::{Event, EventResult, Key};
use cursive::theme::{ColorStyle, ColorType, PaletteColor};
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
    /// Manual scroll offset as a top-line index; `None` means auto-follow.
    manual_scroll: RwLock<Option<usize>>,
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
            manual_scroll: RwLock::new(None),
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
        *self.manual_scroll.write().unwrap() = None;
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

    fn scroll_by(&self, delta: i64) {
        let mut manual = self.manual_scroll.write().unwrap();
        let current = manual.unwrap_or(0) as i64;
        *manual = Some((current + delta).max(0) as usize);
    }

    fn adjust_offset(&self, delta_ms: i64) {
        *self.offset_ms.write().unwrap() += delta_ms;
    }

    /// Force a re-evaluation on the next draw (used by refetch/provider cycle).
    fn force_refetch(&self) {
        *self.fetched_for.write().unwrap() = None;
        *self.manual_scroll.write().unwrap() = None;
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
        // lines participate in scrolling and centering.
        let mut rows: Vec<(&str, RowStyle)> = Vec::new();
        let mut active_row: Option<usize> = None;
        for (i, line) in lyrics.lines.iter().enumerate() {
            let is_active = Some(i) == active;
            if is_active {
                active_row = Some(rows.len());
            }
            rows.push((
                line.text.as_str(),
                if is_active {
                    RowStyle::Active
                } else {
                    RowStyle::Normal
                },
            ));
            if show_translation
                && let Some(translation) = &line.translation
            {
                rows.push((translation.as_str(), RowStyle::Secondary));
            }
            if show_romaji
                && let Some(romanization) = &line.romanization
            {
                rows.push((romanization.as_str(), RowStyle::Secondary));
            }
        }

        let top = match *self.manual_scroll.read().unwrap() {
            Some(scroll) => scroll.min(rows.len().saturating_sub(1)),
            None => active_row
                .map(|a| a.saturating_sub(height / 2))
                .unwrap_or(0),
        };

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
            printer.with_color(color, |printer| printer.print((x, visible_y), text));
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
            LyricsState::NotFound { .. } => self.draw_status(printer, "No lyrics found"),
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

        match event {
            Event::Char('j') | Event::Key(Key::Down) if allow_scroll => {
                self.scroll_by(1);
                EventResult::Consumed(None)
            }
            Event::Char('k') | Event::Key(Key::Up) if allow_scroll => {
                self.scroll_by(-1);
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
                    self.scroll_by(-1);
                }
                Ok(CommandResult::Consumed(None))
            }
            Command::LyricsScrollDown => {
                if cfg.allow_scroll.unwrap_or(true) {
                    self.scroll_by(1);
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
