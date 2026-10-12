//! Individual image operations used by the pipeline.

use image::{GrayImage, ImageBuffer, Pixel, imageops};

/// Physical radius of the unsharp mask. Expressed in millimetres so that the
/// low-resolution preview and the final scan look alike.
pub const SHARPEN_RADIUS_MM: f32 = 0.12;

/// Below this blur radius in pixels the unsharp mask has no visible effect.
const MIN_SHARPEN_SIGMA: f32 = 0.3;

/// A lookup table applying brightness and contrast, or `None` when both
/// are neutral.
pub fn tone_curve(brightness: f32, contrast: f32) -> Option<[u8; 256]> {
    if brightness == 0.0 && contrast == 0.0 {
        return None;
    }
    // Contrast spans a 1/4x ..= 4x slope around mid-gray.
    let slope = 4f32.powf(contrast.clamp(-1.0, 1.0));
    let offset = brightness.clamp(-1.0, 1.0) * 128.0;
    let mut lut = [0u8; 256];
    for (value, out) in lut.iter_mut().enumerate() {
        let v = (value as f32 - 127.5) * slope + 127.5 + offset;
        *out = v.round().clamp(0.0, 255.0) as u8;
    }
    Some(lut)
}

/// Applies a lookup table to every channel in place.
pub fn apply_lut<P>(image: &mut ImageBuffer<P, Vec<u8>>, lut: &[u8; 256])
where
    P: Pixel<Subpixel = u8>,
{
    for value in image.iter_mut() {
        *value = lut[usize::from(*value)];
    }
}

/// Unsharp mask: `out = in + amount * (in - blur(in))`.
pub fn unsharp<P>(image: &mut ImageBuffer<P, Vec<u8>>, amount: f32, dpi: f32)
where
    P: Pixel<Subpixel = u8>,
{
    let sigma = SHARPEN_RADIUS_MM * dpi / 25.4;
    if amount <= 0.0 || sigma < MIN_SHARPEN_SIGMA {
        return;
    }
    let blurred = imageops::fast_blur(image, sigma);
    for (value, blur) in image.iter_mut().zip(blurred.iter()) {
        let v = f32::from(*value);
        let sharpened = v + amount * (v - f32::from(*blur));
        *value = sharpened.round().clamp(0.0, 255.0) as u8;
    }
}

/// Otsu's threshold: the level that best separates the histogram into two
/// classes. Pixels above the returned level are white.
pub fn otsu_level(image: &GrayImage) -> u8 {
    let mut histogram = [0u64; 256];
    for &value in image.iter() {
        histogram[usize::from(value)] += 1;
    }
    let total: u64 = histogram.iter().sum();
    if total == 0 {
        return 127;
    }
    let weighted_total: f64 = histogram
        .iter()
        .enumerate()
        .map(|(i, &n)| i as f64 * n as f64)
        .sum();

    let (mut best_level, mut best_variance) = (127u8, -1.0f64);
    let (mut count_low, mut weighted_low) = (0u64, 0.0f64);
    for (level, &n) in histogram.iter().enumerate() {
        count_low += n;
        weighted_low += level as f64 * n as f64;
        let count_high = total - count_low;
        if count_low == 0 || count_high == 0 {
            continue;
        }
        let mean_low = weighted_low / count_low as f64;
        let mean_high = (weighted_total - weighted_low) / count_high as f64;
        let variance = count_low as f64 * count_high as f64 * (mean_low - mean_high).powi(2);
        if variance > best_variance {
            best_variance = variance;
            best_level = level as u8;
        }
    }
    best_level
}

/// Maps every pixel to 0 (at or below `level`) or 255 (above).
pub fn threshold(image: &mut GrayImage, level: u8) {
    for value in image.iter_mut() {
        *value = if *value > level { 255 } else { 0 };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Luma;

    #[test]
    fn neutral_tone_is_skipped() {
        assert!(tone_curve(0.0, 0.0).is_none());
    }

    #[test]
    fn tone_curve_is_monotonic_and_directional() {
        let brighter = tone_curve(0.5, 0.0).unwrap();
        assert!(brighter[100] > 100);
        let stronger = tone_curve(0.0, 0.5).unwrap();
        assert!(stronger[64] < 64 && stronger[192] > 192);
        let flatter = tone_curve(0.0, -0.5).unwrap();
        assert!(flatter[0] > 0 && flatter[255] < 255);
        for lut in [brighter, stronger, flatter] {
            assert!(lut.windows(2).all(|w| w[0] <= w[1]));
        }
    }

    #[test]
    fn otsu_separates_two_populations() {
        let image = GrayImage::from_fn(100, 10, |x, _| Luma([if x < 30 { 40 } else { 200 }]));
        let level = otsu_level(&image);
        assert!((40..200).contains(&level), "level {level}");
        let mut binary = image.clone();
        threshold(&mut binary, level);
        assert_eq!(binary.get_pixel(0, 0).0, [0]);
        assert_eq!(binary.get_pixel(99, 0).0, [255]);
    }

    #[test]
    fn otsu_handles_flat_images() {
        let flat = GrayImage::from_pixel(8, 8, Luma([90]));
        let mut binary = flat.clone();
        threshold(&mut binary, otsu_level(&flat));
        assert!(binary.iter().all(|&v| v == 0 || v == 255));
    }

    #[test]
    fn unsharp_increases_edge_contrast() {
        let mut image = GrayImage::from_fn(40, 4, |x, _| Luma([if x < 20 { 80 } else { 170 }]));
        unsharp(&mut image, 1.0, 300.0);
        assert!(
            image.get_pixel(19, 0).0[0] < 80,
            "dark side of the edge gets darker"
        );
        assert!(
            image.get_pixel(20, 0).0[0] > 170,
            "light side of the edge gets lighter"
        );
        assert_eq!(image.get_pixel(0, 0).0[0], 80, "flat areas are untouched");
    }

    #[test]
    fn unsharp_is_a_no_op_at_low_resolution() {
        let original = GrayImage::from_fn(10, 1, |x, _| Luma([(x * 20) as u8]));
        let mut image = original.clone();
        unsharp(&mut image, 1.0, 50.0);
        assert_eq!(image, original);
    }
}
