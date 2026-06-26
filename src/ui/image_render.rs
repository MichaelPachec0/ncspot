//! Terminal-image helpers shared by the album-cover and jukebox-graphics renderers.
use std::io::Write;

use cursive::Vec2;
use ioctl_rs::{TIOCGWINSZ, ioctl};

/// Per-cell pixel size, from the terminal's reported pixel/character geometry.
/// Falls back to a typical 8x16 when the terminal does not report pixels.
pub fn font_size() -> Vec2 {
    let (rows, cols, xpixels, ypixels) = unsafe {
        let mut query: (u16, u16, u16, u16) = (0, 0, 0, 0);
        ioctl(1, TIOCGWINSZ, &mut query);
        query
    };
    if cols == 0 || rows == 0 || xpixels == 0 || ypixels == 0 {
        Vec2::new(8, 16)
    } else {
        Vec2::new(
            std::cmp::max(1, xpixels / cols) as usize,
            std::cmp::max(1, ypixels / rows) as usize,
        )
    }
}

pub fn is_iterm_terminal() -> bool {
    std::env::var("TERM_PROGRAM").is_ok_and(|term| term.contains("iTerm"))
        || std::env::var("LC_TERMINAL").is_ok_and(|term| term.contains("iTerm"))
}

fn is_apple_terminal() -> bool {
    std::env::var("TERM_PROGRAM").is_ok_and(|term| term == "Apple_Terminal")
}

pub fn can_use_kitty_graphics() -> bool {
    !is_apple_terminal()
}

/// True when at least one supported image protocol is likely available.
pub fn terminal_supports_graphics() -> bool {
    can_use_kitty_graphics() || is_iterm_terminal()
}

// Currently only the cover renderer fits an externally-sized image to cells; the jukebox
// renderer sizes its own image. Kept here as a shared helper for both.
#[cfg_attr(not(feature = "cover"), allow(dead_code))]
pub fn fit_image_to_cells(
    available_size: Vec2,
    font_size: Vec2,
    image_width: u32,
    image_height: u32,
) -> Vec2 {
    if available_size.x == 0 || available_size.y == 0 || font_size.x == 0 || font_size.y == 0 {
        return Vec2::new(0, 0);
    }

    let image_aspect = image_width as f32 / image_height as f32;
    let cell_aspect = font_size.x as f32 / font_size.y as f32;
    let width_for_full_height =
        (available_size.y as f32 * image_aspect / cell_aspect).floor() as usize;

    if width_for_full_height <= available_size.x {
        Vec2::new(std::cmp::max(1, width_for_full_height), available_size.y)
    } else {
        let height_for_full_width =
            (available_size.x as f32 * cell_aspect / image_aspect).floor() as usize;
        Vec2::new(available_size.x, std::cmp::max(1, height_for_full_width))
    }
}

pub fn clear_terminal_area(offset: Vec2, size: Vec2) {
    let mut stdout = std::io::stdout();
    // Remove stateful Kitty graphics where available, then overwrite the cells used by
    // other protocols/fallbacks.
    if can_use_kitty_graphics() {
        let _ = stdout.write_all(b"\x1b_Ga=d,d=A\x1b\\");
    }
    for y in offset.y..offset.y + size.y {
        let _ = write!(stdout, "\x1b[{};{}H{}", y + 1, offset.x + 1, " ".repeat(size.x));
    }
    let _ = stdout.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_image_keeps_full_height_when_it_fits() {
        let got = fit_image_to_cells(Vec2::new(40, 10), Vec2::new(8, 8), 100, 100);
        assert_eq!(got, Vec2::new(10, 10));
    }

    #[test]
    fn fit_image_clamps_to_width_when_too_wide() {
        let got = fit_image_to_cells(Vec2::new(5, 100), Vec2::new(8, 16), 1000, 100);
        assert_eq!(got.x, 5);
        assert!(got.y >= 1);
    }

    #[test]
    fn fit_image_zero_input_is_zero() {
        assert_eq!(
            fit_image_to_cells(Vec2::new(0, 10), Vec2::new(8, 16), 10, 10),
            Vec2::new(0, 0)
        );
    }
}
