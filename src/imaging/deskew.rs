//! Straightening slightly tilted pages.

use super::ops::otsu_level;
use image::{GrayImage, ImageBuffer, Pixel, imageops};

/// The largest tilt that detection looks for, in degrees.
pub const MAX_SKEW: f32 = 5.0;

/// Detection works on a copy downscaled to about this width.
const ANALYSIS_WIDTH: u32 = 1000;
/// Below this share of dark pixels there is too little content to judge.
const MIN_INK_SHARE: f64 = 0.002;

/// Estimates the rotation, in degrees clockwise, that makes the lines of
/// text (or other horizontal structures) in `image` horizontal.
///
/// Returns `None` for images with too little content. Uses a projection
/// profile search: when lines are level, the horizontal histogram of dark
/// pixels has the sharpest peaks.
pub fn detect_skew(image: &GrayImage) -> Option<f32> {
    let small = if image.width() > ANALYSIS_WIDTH {
        let height =
            (image.height() as f32 * ANALYSIS_WIDTH as f32 / image.width() as f32).round() as u32;
        imageops::resize(
            image,
            ANALYSIS_WIDTH,
            height.max(1),
            imageops::FilterType::Triangle,
        )
    } else {
        image.clone()
    };
    let level = otsu_level(&small);
    let (width, height) = small.dimensions();
    let (cx, cy) = (width as f32 / 2.0, height as f32 / 2.0);
    let ink: Vec<(f32, f32)> = small
        .enumerate_pixels()
        .filter(|(_, _, p)| p.0[0] <= level)
        .map(|(x, y, _)| (x as f32 - cx, y as f32 - cy))
        .collect();
    if (ink.len() as f64) < MIN_INK_SHARE * f64::from(width) * f64::from(height) {
        return None;
    }

    // Coarse search, then refine around the best angle.
    let coarse = best_angle(&ink, height, -MAX_SKEW, MAX_SKEW, 0.5);
    let fine = best_angle(&ink, height, coarse - 0.5, coarse + 0.5, 0.05);
    // Lines tilted by `fine` are corrected by rotating the other way.
    Some(-fine)
}

/// The tilt in `[from, to]` whose projection profile is sharpest.
fn best_angle(ink: &[(f32, f32)], height: u32, from: f32, to: f32, step: f32) -> f32 {
    let margin = height as usize; // room for sheared rows above and below
    let mut bins = vec![0u32; height as usize + 2 * margin];
    let mut best = (0.0, f64::MIN);
    let steps = ((to - from) / step).round() as i32;
    for i in 0..=steps {
        let angle = from + i as f32 * step;
        let slope = angle.to_radians().tan();
        bins.iter_mut().for_each(|b| *b = 0);
        for &(x, y) in ink {
            // Undo a clockwise tilt of `angle` by shearing rows back.
            let row = (y - x * slope).round() as isize + (height / 2) as isize + margin as isize;
            if let Some(bin) = usize::try_from(row).ok().and_then(|r| bins.get_mut(r)) {
                *bin += 1;
            }
        }
        let score: f64 = bins.iter().map(|&n| f64::from(n).powi(2)).sum();
        if score > best.1 {
            best = (angle, score);
        }
    }
    best.0
}

/// Rotates by a small angle (degrees, clockwise) around the center, keeping
/// the size and filling the uncovered corners with white.
pub fn rotate<P>(image: &ImageBuffer<P, Vec<u8>>, degrees: f32) -> ImageBuffer<P, Vec<u8>>
where
    P: Pixel<Subpixel = u8>,
{
    let (width, height) = image.dimensions();
    let (sin, cos) = degrees.to_radians().sin_cos();
    let (cx, cy) = ((width as f32 - 1.0) / 2.0, (height as f32 - 1.0) / 2.0);
    let channels = usize::from(P::CHANNEL_COUNT);
    let source = image.as_raw();
    let stride = width as usize * channels;
    let sample = |x: i64, y: i64, c: usize| -> f32 {
        if x < 0 || y < 0 || x >= i64::from(width) || y >= i64::from(height) {
            255.0
        } else {
            f32::from(source[y as usize * stride + x as usize * channels + c])
        }
    };

    let mut out = ImageBuffer::<P, Vec<u8>>::new(width, height);
    for (v, row) in out.chunks_exact_mut(stride).enumerate() {
        let dy = v as f32 - cy;
        for (u, pixel) in row.chunks_exact_mut(channels).enumerate() {
            let dx = u as f32 - cx;
            // Inverse mapping: where does this output pixel come from?
            let sx = dx * cos + dy * sin + cx;
            let sy = -dx * sin + dy * cos + cy;
            let (x0, y0) = (sx.floor(), sy.floor());
            let (fx, fy) = (sx - x0, sy - y0);
            let (x0, y0) = (x0 as i64, y0 as i64);
            for (c, value) in pixel.iter_mut().enumerate() {
                let top = sample(x0, y0, c) * (1.0 - fx) + sample(x0 + 1, y0, c) * fx;
                let bottom = sample(x0, y0 + 1, c) * (1.0 - fx) + sample(x0 + 1, y0 + 1, c) * fx;
                *value = (top * (1.0 - fy) + bottom * fy).round().clamp(0.0, 255.0) as u8;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Luma;

    /// A page of "text": short dark horizontal strokes on white.
    fn text_page() -> GrayImage {
        GrayImage::from_fn(900, 1200, |x, y| {
            let in_line = (y % 40) < 6 && (100..800).contains(&x) && (100..1100).contains(&y);
            let in_word = (x / 30) % 4 != 3;
            Luma([if in_line && in_word { 20 } else { 245 }])
        })
    }

    #[test]
    fn detects_and_corrects_tilt() {
        for tilt in [-3.0f32, -1.2, 0.7, 2.5] {
            let tilted = rotate(&text_page(), tilt);
            let correction = detect_skew(&tilted).expect("enough content");
            assert!(
                (correction + tilt).abs() < 0.15,
                "tilt {tilt}: correction {correction}"
            );
        }
    }

    #[test]
    fn level_pages_need_no_correction() {
        let correction = detect_skew(&text_page()).unwrap();
        assert!(correction.abs() < 0.1, "{correction}");
    }

    #[test]
    fn blank_pages_are_not_judged() {
        assert_eq!(
            detect_skew(&GrayImage::from_pixel(500, 700, Luma([250]))),
            None
        );
    }

    #[test]
    fn rotation_keeps_size_and_fills_with_white() {
        let image = GrayImage::from_pixel(100, 60, Luma([0]));
        let rotated = rotate(&image, 4.0);
        assert_eq!(rotated.dimensions(), (100, 60));
        assert_eq!(rotated.get_pixel(0, 0).0, [255], "corner uncovered");
        assert_eq!(rotated.get_pixel(50, 30).0, [0], "center unchanged");
        assert_eq!(rotate(&image, 0.0), image);
    }
}
