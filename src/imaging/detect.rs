//! Finding the document on the scanner glass.

use super::adjust::Crop;
use super::pipeline::{Bitmap, Raster};
use image::{RgbImage, imageops};

/// Detection works at about this resolution (pixels per millimetre).
const ANALYSIS_PX_PER_MM: f32 = 3.0;
/// The outermost strip of the bed often shows the glass edge; it is not
/// used to decide where the document is.
const EDGE_IGNORE_MM: f32 = 1.5;
/// Border band used to estimate the color of the lid.
const BACKGROUND_BAND_MM: f32 = 4.0;
/// Minimum per-channel difference from the lid for a pixel to count as
/// document.
const DIFFERENCE: u8 = 32;
/// Content closer than this is merged into one blob.
const MERGE_MM: f32 = 1.0;
/// Blobs smaller than this in both directions are dust or noise.
const MIN_BLOB_MM: f32 = 4.0;
/// Space added around the detected area.
const MARGIN_MM: f32 = 1.5;
/// Edges this close to the border of the bed snap to it.
const SNAP_MM: f32 = 3.0;

/// Finds the area covered by a document lying on the scanner glass, by
/// comparing the image with the color of the lid around it.
///
/// Returns `None` when nothing stands out from the background, or when the
/// document seems to cover the whole area anyway.
pub fn detect_document(bitmap: &Bitmap) -> Option<Crop> {
    let (width, height) = bitmap.raster.dimensions();
    let px_per_mm = bitmap.dpi / 25.4;
    let scale = (ANALYSIS_PX_PER_MM / px_per_mm).min(1.0);
    let small_w = ((width as f32 * scale).round() as u32).max(1);
    let small_h = ((height as f32 * scale).round() as u32).max(1);
    let rgb = match &bitmap.raster {
        Raster::Rgb(image) => {
            imageops::resize(image, small_w, small_h, imageops::FilterType::Triangle)
        }
        other => imageops::resize(
            &other.to_rgb(),
            small_w,
            small_h,
            imageops::FilterType::Triangle,
        ),
    };
    let px_per_mm = px_per_mm * scale;

    let background = border_median(&rgb, (BACKGROUND_BAND_MM * px_per_mm).ceil() as u32);
    let ignore = (EDGE_IGNORE_MM * px_per_mm).ceil() as u32;
    let (w, h) = rgb.dimensions();
    if w <= 2 * ignore + 2 || h <= 2 * ignore + 2 {
        return None;
    }
    let mask = Mask::from_fn(w, h, |x, y| {
        let inside = x >= ignore && y >= ignore && x < w - ignore && y < h - ignore;
        inside
            && rgb
                .get_pixel(x, y)
                .0
                .iter()
                .zip(background)
                .any(|(&v, b)| v.abs_diff(b) > DIFFERENCE)
    });

    // Merge letters, lines and page edges into blobs, then keep only blobs
    // large enough not to be dust.
    let radius = (MERGE_MM * px_per_mm).round().max(1.0) as u32;
    let min_extent = (MIN_BLOB_MM * px_per_mm).round() as u32;
    let (left, top, right, bottom) = mask.dilate(radius).large_blobs_bounds(min_extent)?;
    // Undo the growth caused by the dilation.
    let (left, top) = ((left + radius) as f32, (top + radius) as f32);
    let (right, bottom) = (
        (right + 1).saturating_sub(radius) as f32,
        (bottom + 1).saturating_sub(radius) as f32,
    );
    if right <= left || bottom <= top {
        return None;
    }

    let margin = MARGIN_MM * px_per_mm;
    let snap = SNAP_MM * px_per_mm;
    let bound = |start: f32, end: f32, size: u32| {
        let size = size as f32;
        let start = if start - margin <= snap {
            0.0
        } else {
            start - margin
        };
        let end = if end + margin >= size - snap {
            size
        } else {
            end + margin
        };
        (start / size, end / size)
    };
    let (left, right) = bound(left, right, w);
    let (top, bottom) = bound(top, bottom, h);
    let crop = Crop::from_corners((left, top), (right, bottom));
    // A crop that covers practically everything is no crop at all.
    (crop.width() * crop.height() < 0.97).then_some(crop)
}

/// A binary image.
struct Mask {
    width: u32,
    height: u32,
    bits: Vec<bool>,
}

impl Mask {
    fn from_fn(width: u32, height: u32, f: impl Fn(u32, u32) -> bool) -> Self {
        let bits = (0..height)
            .flat_map(|y| (0..width).map(move |x| (x, y)))
            .map(|(x, y)| f(x, y))
            .collect();
        Mask {
            width,
            height,
            bits,
        }
    }

    fn get(&self, x: u32, y: u32) -> bool {
        self.bits[(y * self.width + x) as usize]
    }

    /// Grows every set pixel into a square of side `2 * radius + 1`.
    fn dilate(&self, radius: u32) -> Mask {
        let (w, h, r) = (self.width, self.height, radius);
        let horizontal = Mask::from_fn(w, h, |x, y| {
            (x.saturating_sub(r)..=(x + r).min(w - 1)).any(|i| self.get(i, y))
        });
        Mask::from_fn(w, h, |x, y| {
            (y.saturating_sub(r)..=(y + r).min(h - 1)).any(|j| horizontal.get(x, j))
        })
    }

    /// The bounding box `(left, top, right, bottom)` (inclusive) of all
    /// connected blobs spanning at least `min_extent` pixels in some
    /// direction.
    fn large_blobs_bounds(&self, min_extent: u32) -> Option<(u32, u32, u32, u32)> {
        let mut seen = vec![false; self.bits.len()];
        let mut stack = Vec::new();
        let mut result: Option<(u32, u32, u32, u32)> = None;
        for start in 0..self.bits.len() {
            if !self.bits[start] || seen[start] {
                continue;
            }
            seen[start] = true;
            stack.push(start);
            let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0, 0);
            while let Some(index) = stack.pop() {
                let (x, y) = (index as u32 % self.width, index as u32 / self.width);
                (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x), y1.max(y));
                let neighbours = [
                    (x > 0).then(|| index - 1),
                    (x + 1 < self.width).then(|| index + 1),
                    (y > 0).then(|| index - self.width as usize),
                    (y + 1 < self.height).then(|| index + self.width as usize),
                ];
                for next in neighbours.into_iter().flatten() {
                    if self.bits[next] && !seen[next] {
                        seen[next] = true;
                        stack.push(next);
                    }
                }
            }
            if x1 - x0 + 1 >= min_extent || y1 - y0 + 1 >= min_extent {
                result = Some(match result {
                    Some((a, b, c, d)) => (a.min(x0), b.min(y0), c.max(x1), d.max(y1)),
                    None => (x0, y0, x1, y1),
                });
            }
        }
        result
    }
}

/// The per-channel median of a band along the image border.
fn border_median(image: &RgbImage, band: u32) -> [u8; 3] {
    let (w, h) = image.dimensions();
    let mut channels: [Vec<u8>; 3] = Default::default();
    for (x, y, pixel) in image.enumerate_pixels() {
        if x < band || y < band || x >= w.saturating_sub(band) || y >= h.saturating_sub(band) {
            for (values, &v) in channels.iter_mut().zip(&pixel.0) {
                values.push(v);
            }
        }
    }
    channels.map(|mut values| {
        if values.is_empty() {
            return 255;
        }
        let middle = values.len() / 2;
        *values.select_nth_unstable(middle).1
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgb;

    const LID: Rgb<u8> = Rgb([228, 232, 227]);

    /// A 200 × 280 mm bed at 75 dpi with a light, slightly noisy lid and a
    /// darker strip along the glass edge.
    fn bed() -> RgbImage {
        RgbImage::from_fn(590, 827, |x, y| {
            if x == 0 || y == 0 {
                Rgb([120, 140, 140])
            } else {
                let noise = ((x * 7 + y * 13) % 9) as u8;
                Rgb([LID.0[0] - noise, LID.0[1] - noise, LID.0[2] - noise])
            }
        })
    }

    fn detect(image: RgbImage) -> Option<Crop> {
        detect_document(&Bitmap {
            raster: Raster::Rgb(image),
            dpi: 75.0,
        })
    }

    fn fill(image: &mut RgbImage, (x0, y0, x1, y1): (u32, u32, u32, u32), color: Rgb<u8>) {
        for y in y0..y1 {
            for x in x0..x1 {
                image.put_pixel(x, y, color);
            }
        }
    }

    #[test]
    fn finds_a_colored_document() {
        let mut image = bed();
        fill(&mut image, (100, 150, 400, 600), Rgb([60, 110, 170]));
        let crop = detect(image).expect("document found");
        let px = |v: f32, size: u32| v * size as f32;
        // Within the 1.5 mm (≈ 4.4 px) margin plus a pixel of blur.
        assert!((px(crop.left, 590) - 95.6).abs() < 2.5, "{crop:?}");
        assert!((px(crop.right, 590) - 404.4).abs() < 2.5, "{crop:?}");
        assert!((px(crop.top, 827) - 145.6).abs() < 2.5, "{crop:?}");
        assert!((px(crop.bottom, 827) - 604.4).abs() < 2.5, "{crop:?}");
    }

    #[test]
    fn finds_white_paper_by_its_outline_and_ignores_dust() {
        let mut image = bed();
        // White sheet in the corner, with a faint shadow along its edges.
        fill(&mut image, (0, 0, 300, 420), Rgb([250, 250, 250]));
        fill(&mut image, (300, 0, 302, 422), Rgb([170, 172, 170]));
        fill(&mut image, (0, 420, 302, 422), Rgb([170, 172, 170]));
        // Some text.
        fill(&mut image, (40, 60, 260, 66), Rgb([20, 20, 20]));
        // Dust on the glass.
        fill(&mut image, (500, 700, 503, 703), Rgb([40, 40, 40]));
        let crop = detect(image).expect("document found");
        assert_eq!(
            (crop.left, crop.top),
            (0.0, 0.0),
            "snapped to the corner: {crop:?}"
        );
        assert!((crop.right * 590.0 - 306.4).abs() < 3.0, "{crop:?}");
        assert!((crop.bottom * 827.0 - 426.4).abs() < 3.0, "{crop:?}");
    }

    #[test]
    fn empty_glass_gives_nothing() {
        assert_eq!(detect(bed()), None);
    }

    #[test]
    fn full_coverage_gives_nothing() {
        let mut image = bed();
        fill(&mut image, (2, 2, 588, 825), Rgb([80, 80, 80]));
        assert_eq!(detect(image), None);
    }
}
