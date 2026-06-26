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
