pub mod raster {
    use image::{Rgba, RgbaImage};

    /// Alpha-composite `color` onto the pixel at (x, y). Out-of-bounds is a no-op.
    pub fn blend(img: &mut RgbaImage, x: i64, y: i64, color: Rgba<u8>) {
        if x < 0 || y < 0 || x >= img.width() as i64 || y >= img.height() as i64 {
            return;
        }
        let (x, y) = (x as u32, y as u32);
        let bg = img.get_pixel(x, y).0;
        let fa = color.0[3] as f32 / 255.0;
        let mix = |f: u8, b: u8| (f as f32 * fa + b as f32 * (1.0 - fa)).round() as u8;
        let out = [
            mix(color.0[0], bg[0]),
            mix(color.0[1], bg[1]),
            mix(color.0[2], bg[2]),
            (color.0[3] as f32 + bg[3] as f32 * (1.0 - fa)).min(255.0) as u8,
        ];
        img.put_pixel(x, y, Rgba(out));
    }

    pub fn filled_circle(img: &mut RgbaImage, cx: i64, cy: i64, r: i64, color: Rgba<u8>) {
        if r <= 0 {
            blend(img, cx, cy, color);
            return;
        }
        let r2 = r * r;
        for dy in -r..=r {
            for dx in -r..=r {
                if dx * dx + dy * dy <= r2 {
                    blend(img, cx + dx, cy + dy, color);
                }
            }
        }
    }

    pub fn line(img: &mut RgbaImage, a: (i64, i64), b: (i64, i64), color: Rgba<u8>) {
        let (mut x0, mut y0) = a;
        let (x1, y1) = b;
        let dx = (x1 - x0).abs();
        let sx = if x0 < x1 { 1 } else { -1 };
        let dy = -(y1 - y0).abs();
        let sy = if y0 < y1 { 1 } else { -1 };
        let mut err = dx + dy;
        loop {
            blend(img, x0, y0, color);
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

    /// Thicken a line by drawing `width` parallel copies offset along the perpendicular.
    pub fn thick_line(
        img: &mut RgbaImage,
        a: (i64, i64),
        b: (i64, i64),
        color: Rgba<u8>,
        width: i64,
    ) {
        let (dx, dy) = ((b.0 - a.0) as f64, (b.1 - a.1) as f64);
        let len = (dx * dx + dy * dy).sqrt().max(1.0);
        let (px, py) = (-dy / len, dx / len);
        let half = width.max(1) / 2;
        for k in -half..=half {
            let ox = (px * k as f64).round() as i64;
            let oy = (py * k as f64).round() as i64;
            line(img, (a.0 + ox, a.1 + oy), (b.0 + ox, b.1 + oy), color);
        }
    }

    /// An upward-bulging arc from `a` to `b`, lifting by `height` px at the midpoint,
    /// sampled into straight segments.
    pub fn arc_polyline(
        img: &mut RgbaImage,
        a: (i64, i64),
        b: (i64, i64),
        height: i64,
        color: Rgba<u8>,
    ) {
        const SEGMENTS: i64 = 24;
        let mut prev = a;
        for i in 1..=SEGMENTS {
            let t = i as f64 / SEGMENTS as f64;
            let x = a.0 as f64 + (b.0 - a.0) as f64 * t;
            let y_base = a.1 as f64 + (b.1 - a.1) as f64 * t;
            let lift = 4.0 * height as f64 * t * (1.0 - t);
            let cur = (x.round() as i64, (y_base - lift).round() as i64);
            line(img, prev, cur, color);
            prev = cur;
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn blend_opaque_sets_pixel() {
            let mut img = RgbaImage::new(3, 3);
            blend(&mut img, 1, 1, Rgba([255, 0, 0, 255]));
            assert_eq!(img.get_pixel(1, 1).0, [255, 0, 0, 255]);
        }

        #[test]
        fn blend_out_of_bounds_is_noop() {
            let mut img = RgbaImage::new(2, 2);
            blend(&mut img, -1, 0, Rgba([255, 0, 0, 255]));
            blend(&mut img, 9, 9, Rgba([255, 0, 0, 255]));
            assert_eq!(img.get_pixel(0, 0).0, [0, 0, 0, 0]);
        }

        #[test]
        fn filled_circle_sets_center_clears_corner() {
            let mut img = RgbaImage::new(11, 11);
            filled_circle(&mut img, 5, 5, 2, Rgba([0, 255, 0, 255]));
            assert_eq!(img.get_pixel(5, 5).0, [0, 255, 0, 255]);
            assert_eq!(img.get_pixel(0, 0).0, [0, 0, 0, 0]);
        }

        #[test]
        fn line_hits_endpoints() {
            let mut img = RgbaImage::new(10, 10);
            line(&mut img, (0, 0), (9, 9), Rgba([0, 0, 255, 255]));
            assert_eq!(img.get_pixel(0, 0).0[3], 255);
            assert_eq!(img.get_pixel(9, 9).0[3], 255);
        }

        #[test]
        fn thick_line_wider_than_thin() {
            let count = |w| {
                let mut img = RgbaImage::new(20, 20);
                thick_line(&mut img, (2, 10), (18, 10), Rgba([255, 0, 0, 255]), w);
                img.pixels().filter(|p| p.0[3] > 0).count()
            };
            assert!(count(5) > count(1));
        }
    }
}

use image::{Rgba, RgbaImage, imageops::FilterType};

use crate::jukebox::graph::Edge;
use crate::jukebox::{SongState, ViewMode};

const SS: u32 = 2; // supersample factor

// Full-intensity branch colours (not dimmed in graphics mode).
const PALETTE: [[u8; 4]; 5] = [
    [0, 200, 200, 255],  // cyan
    [0, 200, 0, 255],    // green
    [225, 175, 0, 255],  // amber
    [200, 0, 200, 255],  // magenta
    [90, 90, 235, 255],  // blue
];
const ACTIVE: [u8; 4] = [235, 45, 45, 255]; // red (matches jukebox_branch intent)
const CURSOR: [u8; 4] = [60, 225, 95, 255]; // green (matches jukebox_cursor intent)
const BEAT: [u8; 4] = [150, 150, 150, 255];
const BEAT_BRANCH: [u8; 4] = [205, 205, 205, 255];

#[derive(Debug, Clone, PartialEq)]
pub struct RenderKey {
    pub mode: ViewMode,
    pub graphics: bool,
    pub current_beat: usize,
    pub last_branch: Option<(usize, usize)>,
    pub total_beats: usize,
    pub region: (usize, usize),
    pub enabled: bool,
    pub show_web: bool,
    pub max: usize,
}

pub fn render_key(
    state: &SongState,
    mode: ViewMode,
    graphics: bool,
    region: (usize, usize),
    enabled: bool,
    show_web: bool,
    max: usize,
) -> RenderKey {
    RenderKey {
        mode,
        graphics,
        current_beat: state.current_beat,
        last_branch: state.last_branch.map(|e| (e.source, e.destination)),
        total_beats: state.graph.beats.len(),
        region,
        enabled,
        show_web,
        max,
    }
}

/// Non-active branch edges to draw, each with a palette index (respects the dials).
fn non_active_edges(state: &SongState, show_web: bool, max: usize) -> Vec<(usize, Edge)> {
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

/// `bg` is the opaque background colour (RGBA): a freshly blitted image fully covers the
/// previous one without a delete-then-redraw clear. Threaded from the cursive theme so the
/// panel matches the terminal background.
pub fn render(
    state: &SongState,
    mode: ViewMode,
    size_px: (u32, u32),
    show_web: bool,
    max: usize,
    bg: [u8; 4],
) -> RgbaImage {
    let (w, h) = (size_px.0.max(1), size_px.1.max(1));
    let (bw, bh) = (w * SS, h * SS);
    let mut img = RgbaImage::from_pixel(bw, bh, Rgba(bg));

    if !state.graph.beats.is_empty() {
        match mode {
            ViewMode::Radial => draw_radial(&mut img, state, bw, bh, show_web, max),
            ViewMode::Linear | ViewMode::Split => draw_linear(&mut img, state, bw, bh, show_web, max),
        }
    }

    image::imageops::resize(&img, w, h, FilterType::Lanczos3)
}

fn draw_radial(img: &mut RgbaImage, state: &SongState, w: u32, h: u32, show_web: bool, max: usize) {
    let total = state.graph.beats.len();
    let cx = w as f64 / 2.0;
    let cy = h as f64 / 2.0;
    let radius = (cx.min(cy) - (6 * SS) as f64).max(2.0);
    let pos = |i: usize| -> (i64, i64) {
        let ang = std::f64::consts::TAU * i as f64 / total as f64 - std::f64::consts::FRAC_PI_2;
        ((cx + radius * ang.cos()).round() as i64, (cy + radius * ang.sin()).round() as i64)
    };

    for i in 0..total {
        let (x, y) = pos(i);
        let (color, r) = if state.graph.beats[i].neighbours.is_empty() {
            (Rgba(BEAT), SS as i64)
        } else {
            (Rgba(BEAT_BRANCH), (2 * SS) as i64)
        };
        raster::filled_circle(img, x, y, r, color);
    }
    for (i, e) in non_active_edges(state, show_web, max) {
        raster::line(img, pos(e.source), pos(e.destination), Rgba(PALETTE[i % PALETTE.len()]));
    }
    if let Some(e) = state.last_branch {
        raster::thick_line(img, pos(e.source), pos(e.destination), Rgba(ACTIVE), (2 * SS) as i64);
    }
    let (ccx, ccy) = pos(state.current_beat);
    raster::filled_circle(img, ccx, ccy, (3 * SS) as i64, Rgba(CURSOR));
}

fn draw_linear(img: &mut RgbaImage, state: &SongState, w: u32, h: u32, show_web: bool, max: usize) {
    let total = state.graph.beats.len();
    let margin = (4 * SS) as i64;
    let track_y = (h as f64 * 0.62) as i64;
    let x_of = |i: usize| -> i64 {
        if total <= 1 {
            margin
        } else {
            margin + (i as i64 * (w as i64 - 2 * margin)) / (total as i64 - 1)
        }
    };
    let arc_h = |span: i64| -> i64 {
        ((span * track_y) / (w as i64).max(1)).clamp(SS as i64, (track_y - 2).max(SS as i64))
    };

    raster::line(img, (margin, track_y), (w as i64 - margin, track_y), Rgba(BEAT));
    for i in 0..total {
        raster::filled_circle(img, x_of(i), track_y, SS as i64, Rgba(BEAT));
    }
    for (i, e) in non_active_edges(state, show_web, max) {
        let (lo, hi) =
            (x_of(e.source).min(x_of(e.destination)), x_of(e.source).max(x_of(e.destination)));
        raster::arc_polyline(img, (lo, track_y), (hi, track_y), arc_h(hi - lo), Rgba(PALETTE[i % PALETTE.len()]));
    }
    if let Some(e) = state.last_branch {
        let (lo, hi) =
            (x_of(e.source).min(x_of(e.destination)), x_of(e.source).max(x_of(e.destination)));
        let height = arc_h(hi - lo);
        raster::arc_polyline(img, (lo, track_y), (hi, track_y), height, Rgba(ACTIVE));
        raster::arc_polyline(img, (lo, track_y - 1), (hi, track_y - 1), height, Rgba(ACTIVE));
    }
    raster::filled_circle(img, x_of(state.current_beat), track_y, (2 * SS) as i64, Rgba(CURSOR));
}

#[cfg(test)]
mod render_tests {
    use super::*;
    use crate::jukebox::graph::{Beat, Edge, SongGraph};
    use std::sync::Arc;

    fn state_with_active_branch() -> SongState {
        let beats: Vec<Beat> = (0..8)
            .map(|i| Beat {
                index: i,
                start_ms: i as f64 * 1000.0,
                duration_ms: 1000.0,
                neighbours: if i == 4 {
                    vec![Edge { source: 4, destination: 1, distance: 5.0 }]
                } else {
                    vec![]
                },
            })
            .collect();
        SongState {
            track_title: "t".into(),
            graph: Arc::new(SongGraph { beats, last_branch_point: 4, longest_reach: 0.0 }),
            current_beat: 4,
            beats_played: 10,
            jumps: 1,
            branch_chance: 0.2,
            listen_time_ms: 1000,
            last_branch: Some(Edge { source: 4, destination: 1, distance: 5.0 }),
            bouncing: false,
            no_analysis: false,
        }
    }

    #[test]
    fn render_produces_requested_dimensions() {
        let img = render(&state_with_active_branch(), ViewMode::Radial, (120, 60), true, 0, [0, 0, 0, 255]);
        assert_eq!(img.dimensions(), (120, 60));
    }

    #[test]
    fn render_is_non_empty() {
        let img = render(&state_with_active_branch(), ViewMode::Radial, (120, 60), true, 0, [0, 0, 0, 255]);
        assert!(img.pixels().any(|p| p.0[3] > 0));
    }

    #[test]
    fn render_is_deterministic() {
        let s = state_with_active_branch();
        let a = render(&s, ViewMode::Linear, (120, 40), true, 0, [0, 0, 0, 255]);
        let b = render(&s, ViewMode::Linear, (120, 40), true, 0, [0, 0, 0, 255]);
        assert_eq!(a.into_raw(), b.into_raw());
    }

    #[test]
    fn render_key_changes_with_current_beat() {
        let mut s = state_with_active_branch();
        let k1 = render_key(&s, ViewMode::Radial, true, (40, 20), true, true, 0);
        s.current_beat = 2;
        let k2 = render_key(&s, ViewMode::Radial, true, (40, 20), true, true, 0);
        assert_ne!(k1, k2);
    }
}

