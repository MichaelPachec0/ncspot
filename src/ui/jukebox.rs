use std::sync::{Arc, RwLock};

use cursive::Cursive;
use cursive::event::{Event, EventResult, Key};
use cursive::theme::{ColorStyle, ColorType, PaletteColor};
use cursive::{Printer, Vec2, View};

use crate::command::Command;
use crate::commands::CommandResult;
use crate::config::Config;
use crate::jukebox::graph::Edge;
use crate::jukebox::{Jukebox, SongState, ViewMode};
use crate::queue::Queue;
use crate::traits::ViewExt;

pub struct JukeboxView {
    jukebox: Arc<Jukebox>,
    #[allow(dead_code)]
    queue: Arc<Queue>,
    cfg: Arc<Config>,
    selected_beat: RwLock<Option<usize>>,
}

impl JukeboxView {
    pub fn new(jukebox: Arc<Jukebox>, queue: Arc<Queue>, cfg: Arc<Config>) -> Self {
        Self { jukebox, queue, cfg, selected_beat: RwLock::new(None) }
    }

    /// Whether to draw the full branch web for `layout`, and the cap on branches drawn
    /// (0 = unlimited). Read live from `[jukebox]` config each draw.
    fn branch_render(&self, layout: &str) -> (bool, usize) {
        let jb = self.cfg.values().jukebox.clone().unwrap_or_default();
        let show_all = jb.show_all_branches.unwrap_or(true);
        let layouts = jb.branch_layouts.unwrap_or_else(|| {
            vec!["linear".to_string(), "radial".to_string(), "split".to_string()]
        });
        let show_web = show_all && layouts.iter().any(|l| l == layout);
        (show_web, jb.max_branches_drawn.unwrap_or(0))
    }

    /// The non-active branch edges to draw (respecting the layout toggle and cap), each
    /// paired with a palette index. Empty when the web is disabled for this layout.
    fn web_edges(state: &SongState, show_web: bool, max: usize) -> Vec<(usize, Edge)> {
        if !show_web {
            return Vec::new();
        }
        let mut out: Vec<(usize, Edge)> = Vec::new();
        for b in &state.graph.beats {
            for e in &b.neighbours {
                if max != 0 && out.len() >= max {
                    return out;
                }
                let i = out.len();
                out.push((i, *e));
            }
        }
        out
    }

    fn x_of(beat: usize, total: usize, width: usize) -> usize {
        if total <= 1 {
            0
        } else {
            beat * width.saturating_sub(1) / (total - 1)
        }
    }

    fn move_selection(&self, delta: i64) {
        let Some(state) = self.jukebox.state() else { return };
        let total = state.graph.beats.len();
        if total == 0 {
            return;
        }
        let mut sel = self.selected_beat.write().unwrap();
        let cur = sel.unwrap_or(state.current_beat);
        *sel = Some((cur as i64 + delta).clamp(0, total as i64 - 1) as usize);
    }

    fn seek_to_selection(&self) {
        if let Some(sel) = *self.selected_beat.read().unwrap() {
            self.jukebox.request_seek_to_beat(sel);
        }
    }

    fn stats_line(state: &SongState) -> String {
        let mins = state.listen_time_ms / 60000;
        let secs = (state.listen_time_ms / 1000) % 60;
        format!(
            "beat {}/{}   branch {:.0}%   jumps {}   listened {}:{:02}{}",
            state.current_beat + 1,
            state.graph.beats.len(),
            state.branch_chance * 100.0,
            state.jumps,
            mins,
            secs,
            if state.bouncing { "   [bounce]" } else { "" },
        )
    }

    fn draw_status(printer: &Printer, msg: &str) {
        let y = printer.size.y / 2;
        let x = printer.size.x.saturating_sub(msg.len()) / 2;
        printer.print((x, y), msg);
    }

    /// Distinct, intentionally dim colours for non-active branches (the active branch is
    /// drawn brightly in `jukebox_branch`, i.e. red). Cycled by edge index to give the
    /// "web" look without competing with the active branch. Truecolor terminals (kitty,
    /// etc.) render these exactly; 256-colour terminals map to the nearest shade.
    fn edge_colors() -> Vec<ColorStyle> {
        use cursive::theme::Color;
        [
            Color::Rgb(0, 80, 80),    // dim cyan
            Color::Rgb(0, 80, 0),     // dim green
            Color::Rgb(90, 70, 0),    // dim amber
            Color::Rgb(80, 0, 80),    // dim magenta
            Color::Rgb(40, 40, 100),  // dim blue
        ]
        .into_iter()
        .map(|c| ColorStyle::new(ColorType::Color(c), ColorType::Palette(PaletteColor::Background)))
        .collect()
    }

    /// Row at which a branch's bracket apex sits: longer branches arc higher, so the
    /// stacked brackets spread out instead of overlapping on one row.
    fn apex_row(track_row: usize, span: usize, width: usize) -> usize {
        if track_row < 2 {
            return 0;
        }
        let max_lift = track_row - 1;
        let lift = 1 + span * max_lift.saturating_sub(1) / width.max(1);
        track_row.saturating_sub(lift).max(1)
    }

    /// A single branch bracket (╭──╮) at `apex`, in `color`.
    fn draw_bracket(printer: &Printer, color: ColorStyle, lo: usize, hi: usize, apex: usize) {
        printer.with_color(color, |p| {
            if lo == hi {
                p.print((lo, apex), "│");
                return;
            }
            p.print((lo, apex), "╭");
            p.print((hi, apex), "╮");
            for x in (lo + 1)..hi {
                p.print((x, apex), "─");
            }
        });
    }

    /// Draw every branch as a colour-cycled bracket over a horizontal track, with the
    /// active branch overlaid in `branch` (red) and a ▼ at its destination.
    fn draw_branches_linear(
        printer: &Printer,
        state: &SongState,
        branch: ColorStyle,
        total: usize,
        width: usize,
        track_row: usize,
        edges: &[(usize, Edge)],
    ) {
        let palette = Self::edge_colors();
        for &(i, e) in edges {
            let x1 = Self::x_of(e.source, total, width);
            let x2 = Self::x_of(e.destination, total, width);
            let (lo, hi) = (x1.min(x2), x1.max(x2));
            let apex = Self::apex_row(track_row, hi - lo, width);
            Self::draw_bracket(printer, palette[i % palette.len()], lo, hi, apex);
        }
        if let Some(edge) = state.last_branch {
            let x1 = Self::x_of(edge.source, total, width);
            let x2 = Self::x_of(edge.destination, total, width);
            let (lo, hi) = (x1.min(x2), x1.max(x2));
            let apex = Self::apex_row(track_row, hi - lo, width);
            Self::draw_bracket(printer, branch, lo, hi, apex);
            if track_row >= 1 {
                printer.with_color(branch, |p| p.print((x2, track_row - 1), "▼"));
            }
        }
    }

    fn draw_line(printer: &Printer, color: ColorStyle, a: (usize, usize), b: (usize, usize)) {
        let (mut x0, mut y0) = (a.0 as i64, a.1 as i64);
        let (x1, y1) = (b.0 as i64, b.1 as i64);
        let dx = (x1 - x0).abs();
        let sx = if x0 < x1 { 1 } else { -1 };
        let dy = -(y1 - y0).abs();
        let sy = if y0 < y1 { 1 } else { -1 };
        let mut err = dx + dy;
        loop {
            printer.with_color(color, |p| p.print((x0 as usize, y0 as usize), "·"));
            if x0 == x1 && y0 == y1 {
                break;
            }
            let e2 = 2 * err;
            if e2 >= dy {
                err += dy;
                x0 += sx;
            }
            if e2 <= dx {
                err += dx;
                y0 += sy;
            }
        }
    }

    fn draw_linear(&self, printer: &Printer, state: &SongState) {
        let w = printer.size.x;
        let h = printer.size.y;
        if w == 0 || h < 4 {
            return;
        }
        let total = state.graph.beats.len();
        if total == 0 {
            Self::draw_status(printer, "No analysis available for this track.");
            return;
        }

        let branch = ColorStyle::new(
            ColorType::Color(*printer.theme.palette.custom("jukebox_branch").unwrap()),
            ColorType::Palette(PaletteColor::Background),
        );
        let cursor = ColorStyle::new(
            ColorType::Color(*printer.theme.palette.custom("jukebox_cursor").unwrap()),
            ColorType::Palette(PaletteColor::Background),
        );
        let normal = ColorStyle::primary();

        let status = if self.jukebox.is_enabled() { "▶" } else { "⏸" };
        printer.print((0, 0), &format!("Eternal Jukebox {status}  {}", state.track_title));

        let track_row = h / 2;

        for i in 0..total {
            let x = Self::x_of(i, total, w);
            printer.with_color(normal, |p| p.print((x, track_row), "─"));
        }

        let (show_web, max) = self.branch_render("linear");
        let edges = Self::web_edges(state, show_web, max);
        Self::draw_branches_linear(printer, state, branch, total, w, track_row, &edges);

        let cx = Self::x_of(state.current_beat, total, w);
        printer.with_color(cursor, |p| {
            p.print((cx, track_row), "▮");
            p.print((cx, track_row + 1), "▲");
        });
        if let Some(sel) = *self.selected_beat.read().unwrap() {
            let sx = Self::x_of(sel, total, w);
            printer.with_color(cursor, |p| p.print((sx, track_row + 2), "┃"));
        }

        let stats = Self::stats_line(state);
        printer.print((0, h.saturating_sub(1)), &stats);
    }

    fn draw_radial(&self, printer: &Printer, state: &SongState) {
        let w = printer.size.x;
        let h = printer.size.y;
        if w < 8 || h < 8 {
            self.draw_linear(printer, state);
            return;
        }
        let total = state.graph.beats.len();
        if total == 0 {
            Self::draw_status(printer, "No analysis available for this track.");
            return;
        }
        let cx = w as f64 / 2.0;
        let cy = h as f64 / 2.0;
        // Terminal cells are ~twice as tall as wide, so a round circle needs the column
        // radius to be ~2x the row radius. Pick the largest round radius that fits both.
        const CELL_ASPECT: f64 = 2.0;
        let r_rows = (h as f64 / 2.0 - 1.0).max(1.0);
        let r_cols = (w as f64 / 2.0 - 1.0).max(1.0);
        let ry = r_rows.min(r_cols / CELL_ASPECT);
        let rx = ry * CELL_ASPECT;
        let pos = |i: usize| -> (usize, usize) {
            let ang = std::f64::consts::TAU * i as f64 / total as f64 - std::f64::consts::FRAC_PI_2;
            let x = (cx + rx * ang.cos()).round().clamp(0.0, (w - 1) as f64) as usize;
            let y = (cy + ry * ang.sin()).round().clamp(0.0, (h - 1) as f64) as usize;
            (x, y)
        };

        let branch = ColorStyle::new(
            ColorType::Color(*printer.theme.palette.custom("jukebox_branch").unwrap()),
            ColorType::Palette(PaletteColor::Background),
        );
        let cursor = ColorStyle::new(
            ColorType::Color(*printer.theme.palette.custom("jukebox_cursor").unwrap()),
            ColorType::Palette(PaletteColor::Background),
        );
        let normal = ColorStyle::primary();

        printer.print((0, 0), &format!("Eternal Jukebox  {}", state.track_title));
        for i in 0..total {
            let (x, y) = pos(i);
            let ch = if state.graph.beats[i].neighbours.is_empty() { "·" } else { "◦" };
            printer.with_color(normal, |p| p.print((x, y), ch));
        }
        // All branches as colour-cycled chords; active branch overlaid in red.
        let (show_web, max) = self.branch_render("radial");
        let palette = Self::edge_colors();
        for (i, e) in Self::web_edges(state, show_web, max) {
            Self::draw_line(printer, palette[i % palette.len()], pos(e.source), pos(e.destination));
        }
        if let Some(edge) = state.last_branch {
            Self::draw_line(printer, branch, pos(edge.source), pos(edge.destination));
        }
        let (ccx, ccy) = pos(state.current_beat);
        printer.with_color(cursor, |p| p.print((ccx, ccy), "●"));
        if let Some(sel) = *self.selected_beat.read().unwrap() {
            let (sx, sy) = pos(sel);
            printer.with_color(cursor, |p| p.print((sx, sy), "◉"));
        }
        let stats = Self::stats_line(state);
        printer.print((0, h.saturating_sub(1)), &stats);
    }

    fn draw_split(&self, printer: &Printer, state: &SongState) {
        let w = printer.size.x;
        let h = printer.size.y;
        if w < 30 || h < 8 {
            self.draw_linear(printer, state);
            return;
        }
        let total = state.graph.beats.len();
        if total == 0 {
            Self::draw_status(printer, "No analysis available for this track.");
            return;
        }
        let panel_w = 22usize;
        let left_w = w.saturating_sub(panel_w + 1);

        let branch = ColorStyle::new(
            ColorType::Color(*printer.theme.palette.custom("jukebox_branch").unwrap()),
            ColorType::Palette(PaletteColor::Background),
        );
        let cursor = ColorStyle::new(
            ColorType::Color(*printer.theme.palette.custom("jukebox_cursor").unwrap()),
            ColorType::Palette(PaletteColor::Background),
        );
        let normal = ColorStyle::primary();

        for y in 0..h {
            printer.print((left_w, y), "│");
        }
        let x_of = |i: usize| -> usize {
            if total <= 1 {
                0
            } else {
                i * left_w.saturating_sub(1) / (total - 1)
            }
        };
        let track_row = h / 2;
        for i in 0..total {
            printer.with_color(normal, |p| p.print((x_of(i), track_row), "─"));
        }
        // All branches as colour-cycled brackets; active branch overlaid in red.
        let (show_web, max) = self.branch_render("split");
        let palette = Self::edge_colors();
        for (i, e) in Self::web_edges(state, show_web, max) {
            let (lo, hi) = (x_of(e.source).min(x_of(e.destination)), x_of(e.source).max(x_of(e.destination)));
            let apex = Self::apex_row(track_row, hi - lo, left_w);
            Self::draw_bracket(printer, palette[i % palette.len()], lo, hi, apex);
        }
        if let Some(edge) = state.last_branch {
            let (lo, hi) = (x_of(edge.source).min(x_of(edge.destination)), x_of(edge.source).max(x_of(edge.destination)));
            let apex = Self::apex_row(track_row, hi - lo, left_w);
            Self::draw_bracket(printer, branch, lo, hi, apex);
        }
        let cx = x_of(state.current_beat);
        printer.with_color(cursor, |p| {
            p.print((cx, track_row), "▮");
            p.print((cx, track_row + 1), "▲");
        });

        let px = left_w + 2;
        let title: String = state.track_title.chars().take(panel_w - 2).collect();
        printer.print((px, 1), "Now Playing");
        printer.print((px, 2), &title);
        printer.print((px, 4), &format!("Beat   {}/{}", state.current_beat + 1, total));
        printer.print((px, 9), &format!("Played {}", state.beats_played));
        printer.print((px, 5), &format!("Branch {:.0}%", state.branch_chance * 100.0));
        printer.print((px, 6), &format!("Jumps  {}", state.jumps));
        let mins = state.listen_time_ms / 60000;
        let secs = (state.listen_time_ms / 1000) % 60;
        printer.print((px, 7), &format!("Listen {mins}:{secs:02}"));
        if state.bouncing {
            printer.print((px, 8), "[bounce]");
        }
    }
}

impl View for JukeboxView {
    fn draw(&self, printer: &Printer<'_, '_>) {
        let Some(state) = self.jukebox.state() else {
            Self::draw_status(printer, "Jukebox is off. Run :jukeboxtoggle to start.");
            return;
        };
        if state.no_analysis {
            Self::draw_status(printer, "No analysis available for this track.");
            return;
        }
        match self.jukebox.view_mode() {
            ViewMode::Linear => self.draw_linear(printer, &state),
            ViewMode::Radial => self.draw_radial(printer, &state),
            ViewMode::Split => self.draw_split(printer, &state),
        }
    }

    fn required_size(&mut self, constraint: Vec2) -> Vec2 {
        constraint
    }

    fn layout(&mut self, _: Vec2) {}

    fn on_event(&mut self, event: Event) -> EventResult {
        match event {
            Event::Char('l') | Event::Key(Key::Right) => {
                self.move_selection(1);
                EventResult::Consumed(None)
            }
            Event::Char('h') | Event::Key(Key::Left) => {
                self.move_selection(-1);
                EventResult::Consumed(None)
            }
            Event::Key(Key::Enter) => {
                self.seek_to_selection();
                EventResult::Consumed(None)
            }
            Event::Char('v') => {
                self.jukebox.cycle_view_mode();
                EventResult::Consumed(None)
            }
            Event::Char('g') => {
                self.jukebox.toggle_graphics();
                EventResult::Consumed(None)
            }
            Event::Char('b') => {
                self.jukebox.toggle_bounce();
                EventResult::Consumed(None)
            }
            Event::Char('i') => {
                self.jukebox.toggle();
                EventResult::Consumed(None)
            }
            _ => EventResult::Ignored,
        }
    }
}

impl ViewExt for JukeboxView {
    fn title(&self) -> String {
        "Jukebox".to_string()
    }

    fn on_command(&mut self, s: &mut Cursive, cmd: &Command) -> Result<CommandResult, String> {
        match cmd {
            Command::JukeboxViewCycle => {
                self.jukebox.cycle_view_mode();
                Ok(CommandResult::Consumed(None))
            }
            Command::JukeboxBounce => {
                self.jukebox.toggle_bounce();
                Ok(CommandResult::Consumed(None))
            }
            Command::JukeboxSeekToBeat => {
                self.seek_to_selection();
                Ok(CommandResult::Consumed(None))
            }
            Command::JukeboxToggle => {
                self.jukebox.toggle();
                Ok(CommandResult::Consumed(None))
            }
            Command::JukeboxSettings => {
                crate::ui::jukebox_settings::open_settings_modal(s, self.jukebox.clone());
                Ok(CommandResult::Consumed(None))
            }
            Command::JukeboxGraphics => {
                self.jukebox.toggle_graphics();
                Ok(CommandResult::Consumed(None))
            }
            _ => Ok(CommandResult::Ignored),
        }
    }
}
