//! The non-destructive render pipeline: original scan + [`Adjustments`] → output.

use super::adjust::{Adjustments, Crop, OutputMode, Rotation, Threshold};
use super::deskew;
use super::ops;
use image::imageops::{self, FilterType};
use image::{DynamicImage, GrayImage, ImageBuffer, ImageReader, Pixel, RgbImage};
use std::io::Cursor;

/// Straightening below this angle (degrees) is treated as none.
const MIN_STRAIGHTEN: f32 = 0.01;

/// Pixel data in one of the forms ScanClog works with.
#[derive(Debug, Clone, PartialEq)]
pub enum Raster {
    Rgb(RgbImage),
    Gray(GrayImage),
    /// Thresholded gray image whose pixels are only 0 or 255.
    Bilevel(GrayImage),
}

impl Raster {
    pub fn dimensions(&self) -> (u32, u32) {
        match self {
            Raster::Rgb(image) => image.dimensions(),
            Raster::Gray(image) | Raster::Bilevel(image) => image.dimensions(),
        }
    }

    pub fn to_rgb(&self) -> RgbImage {
        match self {
            Raster::Rgb(image) => image.clone(),
            Raster::Gray(image) | Raster::Bilevel(image) => {
                DynamicImage::ImageLuma8(image.clone()).into_rgb8()
            }
        }
    }
}

/// A raster together with its physical resolution.
#[derive(Debug, Clone, PartialEq)]
pub struct Bitmap {
    pub raster: Raster,
    /// Pixels per inch. Fractional for downscaled previews.
    pub dpi: f32,
}

impl Bitmap {
    /// Physical size in millimetres.
    pub fn size_mm(&self) -> (f32, f32) {
        let (w, h) = self.raster.dimensions();
        (w as f32 * 25.4 / self.dpi, h as f32 * 25.4 / self.dpi)
    }
}

#[derive(Debug, thiserror::Error)]
#[error("cannot decode the scanned image: {0}")]
pub struct DecodeError(#[from] image::ImageError);

/// Decodes a scanner JPEG (or any supported format) into an RGB or gray bitmap.
pub fn decode(data: &[u8], dpi: f32) -> Result<Bitmap, DecodeError> {
    let image = ImageReader::new(Cursor::new(data))
        .with_guessed_format()
        .map_err(image::ImageError::IoError)?
        .decode()?;
    let raster = match image {
        DynamicImage::ImageLuma8(gray) => Raster::Gray(gray),
        image if !image.color().has_color() => Raster::Gray(image.into_luma8()),
        image => Raster::Rgb(image.into_rgb8()),
    };
    Ok(Bitmap { raster, dpi })
}

/// Where the output is going, which decides its resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// Full resolution, for saving.
    Full,
    /// Downscaled to fit in a box of this many pixels, for display.
    /// Images are never enlarged.
    Fit { width: u32, height: u32 },
}

/// Renders `source` with `adjustments`.
///
/// Order: crop → rotate → (downscale) → convert → tone → sharpen → threshold.
/// Cropping happens first, in source coordinates, so the remaining steps
/// touch as few pixels as possible.
pub fn render(source: &Bitmap, adjustments: &Adjustments, target: Target) -> Bitmap {
    render_inner(source, adjustments, adjustments.crop, None, target).0
}

/// A rendered view of the whole image for editing, with the crop shown as an
/// overlay rather than applied.
#[derive(Debug, Clone, PartialEq)]
pub struct View {
    pub bitmap: Bitmap,
    /// The black-and-white threshold used, if any.
    pub threshold: Option<u8>,
}

/// Renders the whole (rotated) image, ignoring the crop, but computes an
/// automatic threshold from the cropped area only, so that the view matches
/// what [`render`] will produce inside the crop.
pub fn render_view(source: &Bitmap, adjustments: &Adjustments, target: Target) -> View {
    let (bitmap, threshold) = render_inner(
        source,
        adjustments,
        Crop::FULL,
        Some(adjustments.crop),
        target,
    );
    View { bitmap, threshold }
}

/// `crop` is applied to the image; `level_area` (in the output frame)
/// restricts the region used for the automatic threshold.
fn render_inner(
    source: &Bitmap,
    adjustments: &Adjustments,
    crop: Crop,
    level_area: Option<Crop>,
    target: Target,
) -> (Bitmap, Option<u8>) {
    let geometry = Geometry {
        crop,
        rotation: adjustments.rotation,
        straighten: adjustments.straighten,
    };
    let mut dpi = source.dpi;
    let raster = match &source.raster {
        Raster::Rgb(image) => Raster::Rgb(transform(image, geometry, target, &mut dpi)),
        Raster::Gray(image) | Raster::Bilevel(image) => {
            Raster::Gray(transform(image, geometry, target, &mut dpi))
        }
    };

    let mut level = None;
    let raster = match (adjustments.mode, raster) {
        (OutputMode::Color, Raster::Rgb(mut image)) => {
            adjust(&mut image, adjustments, dpi);
            Raster::Rgb(image)
        }
        (mode, raster) => {
            let mut gray = match raster {
                Raster::Rgb(image) => DynamicImage::ImageRgb8(image).into_luma8(),
                Raster::Gray(image) | Raster::Bilevel(image) => image,
            };
            adjust(&mut gray, adjustments, dpi);
            if mode == OutputMode::BlackAndWhite {
                let used = match level_area.filter(|area| !area.is_full()) {
                    Some(area) => {
                        let (x, y, w, h) = area.to_pixels(gray.width(), gray.height());
                        let region = imageops::crop_imm(&gray, x, y, w, h).to_image();
                        threshold_level(&region, adjustments.threshold)
                    }
                    None => threshold_level(&gray, adjustments.threshold),
                };
                ops::threshold(&mut gray, used);
                level = Some(used);
                Raster::Bilevel(gray)
            } else {
                Raster::Gray(gray)
            }
        }
    };
    (Bitmap { raster, dpi }, level)
}

/// The threshold actually used for `image`: the manual value, or the
/// Otsu level of the (already toned and sharpened) gray image.
pub fn threshold_level(image: &GrayImage, threshold: Threshold) -> u8 {
    match threshold {
        Threshold::Manual(level) => level,
        Threshold::Auto => ops::otsu_level(image),
    }
}

/// The geometric part of the adjustments.
#[derive(Debug, Clone, Copy)]
struct Geometry {
    crop: Crop,
    rotation: Rotation,
    straighten: f32,
}

/// Crops, rotates, straightens and downscales.
fn transform<P>(
    image: &ImageBuffer<P, Vec<u8>>,
    geometry: Geometry,
    target: Target,
    dpi: &mut f32,
) -> ImageBuffer<P, Vec<u8>>
where
    P: Pixel<Subpixel = u8> + 'static,
{
    let Geometry {
        crop,
        rotation,
        straighten,
    } = geometry;
    let rotate = |image: &ImageBuffer<P, Vec<u8>>| match rotation {
        Rotation::None => image.clone(),
        Rotation::Cw90 => imageops::rotate90(image),
        Rotation::Cw180 => imageops::rotate180(image),
        Rotation::Cw270 => imageops::rotate270(image),
    };
    let oriented = if straighten.abs() < MIN_STRAIGHTEN {
        // Fast path: crop first, in source coordinates, so the remaining
        // steps touch as few pixels as possible.
        let (x, y, w, h) = crop
            .to_source(rotation)
            .to_pixels(image.width(), image.height());
        rotate(&imageops::crop_imm(image, x, y, w, h).to_image())
    } else {
        // The crop was drawn on the straightened image, so straighten the
        // whole page first.
        let straightened = deskew::rotate(&rotate(image), straighten);
        let (x, y, w, h) = crop.to_pixels(straightened.width(), straightened.height());
        imageops::crop_imm(&straightened, x, y, w, h).to_image()
    };

    let Target::Fit {
        width: max_w,
        height: max_h,
    } = target
    else {
        return oriented;
    };
    let (w, h) = oriented.dimensions();
    let scale = (max_w as f32 / w as f32).min(max_h as f32 / h as f32);
    if scale >= 1.0 {
        return oriented;
    }
    let new_w = ((w as f32 * scale).round() as u32).max(1);
    let new_h = ((h as f32 * scale).round() as u32).max(1);
    *dpi *= new_w as f32 / w as f32;
    imageops::resize(&oriented, new_w, new_h, FilterType::Triangle)
}

fn adjust<P>(image: &mut ImageBuffer<P, Vec<u8>>, adjustments: &Adjustments, dpi: f32)
where
    P: Pixel<Subpixel = u8>,
{
    if let Some(lut) = ops::tone_curve(adjustments.brightness, adjustments.contrast) {
        ops::apply_lut(image, &lut);
    }
    ops::unsharp(image, adjustments.sharpen, dpi);
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Luma, Rgb};

    /// A 40 × 20 mm test card at 100 dpi: dark text-like stripes on a light,
    /// slightly tinted background, with a red marker in the top-left corner.
    fn card() -> Bitmap {
        let image = RgbImage::from_fn(157, 79, |x, y| match (x, y) {
            (0..10, 0..10) => Rgb([220, 20, 20]),
            _ if (x / 8) % 2 == 0 && y > 20 => Rgb([30, 30, 40]),
            _ => Rgb([235, 230, 210]),
        });
        Bitmap {
            raster: Raster::Rgb(image),
            dpi: 100.0,
        }
    }

    #[test]
    fn default_adjustments_keep_the_image() {
        let source = card();
        assert_eq!(
            render(&source, &Adjustments::default(), Target::Full),
            source
        );
    }

    #[test]
    fn crops_in_the_rotated_frame() {
        let source = card();
        let adjustments = Adjustments {
            rotation: Rotation::Cw90,
            // Top-right corner of the rotated view = top-left of the source.
            crop: Crop {
                left: 0.8,
                top: 0.0,
                right: 1.0,
                bottom: 0.1,
            },
            ..Adjustments::default()
        };
        let out = render(&source, &adjustments, Target::Full);
        let Raster::Rgb(image) = &out.raster else {
            panic!("expected color")
        };
        assert_eq!(image.dimensions(), (16, 16));
        assert_eq!(image.get_pixel(15, 0), &Rgb([220, 20, 20]));
    }

    #[test]
    fn fits_target_and_tracks_dpi() {
        let out = render(
            &card(),
            &Adjustments::default(),
            Target::Fit {
                width: 50,
                height: 50,
            },
        );
        assert_eq!(out.raster.dimensions(), (50, 25));
        assert!((out.dpi - 100.0 * 50.0 / 157.0).abs() < 0.01);
        let (w, _) = out.size_mm();
        assert!((w - 39.88).abs() < 0.1, "physical size is preserved: {w}");
        // Small images are not enlarged.
        let same = render(
            &card(),
            &Adjustments::default(),
            Target::Fit {
                width: 1000,
                height: 1000,
            },
        );
        assert_eq!(same.raster.dimensions(), (157, 79));
    }

    #[test]
    fn produces_gray_and_bilevel_output() {
        let gray = Adjustments {
            mode: OutputMode::Gray,
            ..Adjustments::default()
        };
        assert!(matches!(
            render(&card(), &gray, Target::Full).raster,
            Raster::Gray(_)
        ));

        let bw = Adjustments {
            mode: OutputMode::BlackAndWhite,
            ..Adjustments::default()
        };
        let Raster::Bilevel(image) = render(&card(), &bw, Target::Full).raster else {
            panic!("expected bilevel")
        };
        assert!(image.iter().all(|&v| v == 0 || v == 255));
        assert_eq!(image.get_pixel(0, 30), &Luma([0]), "stripes are black");
        assert_eq!(image.get_pixel(12, 30), &Luma([255]), "background is white");
    }

    #[test]
    fn manual_threshold_is_respected() {
        let all_white = Adjustments {
            mode: OutputMode::BlackAndWhite,
            threshold: Threshold::Manual(0),
            ..Adjustments::default()
        };
        let Raster::Bilevel(image) = render(&card(), &all_white, Target::Full).raster else {
            panic!()
        };
        assert!(image.iter().all(|&v| v == 255));
    }

    #[test]
    fn gray_sources_cannot_become_color() {
        let source = Bitmap {
            raster: Raster::Gray(GrayImage::from_pixel(4, 4, Luma([100]))),
            dpi: 300.0,
        };
        let out = render(&source, &Adjustments::default(), Target::Full);
        assert_eq!(
            out.raster,
            Raster::Gray(GrayImage::from_pixel(4, 4, Luma([100])))
        );
    }

    #[test]
    fn decodes_gray_and_color_jpegs() {
        let mut jpeg = Vec::new();
        let gray = GrayImage::from_pixel(8, 8, Luma([128]));
        DynamicImage::ImageLuma8(gray)
            .write_to(&mut Cursor::new(&mut jpeg), image::ImageFormat::Jpeg)
            .unwrap();
        assert!(matches!(
            decode(&jpeg, 300.0).unwrap().raster,
            Raster::Gray(_)
        ));

        let mut jpeg = Vec::new();
        let Raster::Rgb(color) = card().raster else {
            unreachable!()
        };
        DynamicImage::ImageRgb8(color)
            .write_to(&mut Cursor::new(&mut jpeg), image::ImageFormat::Jpeg)
            .unwrap();
        let decoded = decode(&jpeg, 100.0).unwrap();
        assert!(matches!(decoded.raster, Raster::Rgb(_)));
        assert_eq!(decoded.raster.dimensions(), (157, 79));

        assert!(decode(b"not an image", 300.0).is_err());
    }

    #[test]
    fn view_ignores_crop_but_thresholds_like_the_output() {
        // Left half dark gray, right half white; the crop covers a mid-gray
        // band and part of the dark half.
        let image = GrayImage::from_fn(100, 10, |x, _| {
            Luma([match x {
                0..40 => 20,
                40..60 => 120,
                _ => 255,
            }])
        });
        let source = Bitmap {
            raster: Raster::Gray(image),
            dpi: 100.0,
        };
        let adjustments = Adjustments {
            mode: OutputMode::BlackAndWhite,
            crop: Crop {
                left: 0.3,
                top: 0.0,
                right: 0.6,
                bottom: 1.0,
            },
            ..Adjustments::default()
        };
        let view = render_view(&source, &adjustments, Target::Full);
        assert_eq!(view.bitmap.raster.dimensions(), (100, 10));
        let output = render(&source, &adjustments, Target::Full);
        let (Raster::Bilevel(view_image), Raster::Bilevel(out_image)) =
            (&view.bitmap.raster, &output.raster)
        else {
            panic!("expected bilevel images");
        };
        assert!(view.threshold.is_some());
        for x in 0..30 {
            assert_eq!(
                view_image.get_pixel(30 + x, 0),
                out_image.get_pixel(x, 0),
                "column {x}"
            );
        }
    }

    #[test]
    fn straightening_keeps_size_and_applies_crop_afterwards() {
        let adjustments = Adjustments {
            straighten: 2.0,
            crop: Crop {
                left: 0.25,
                top: 0.25,
                right: 0.75,
                bottom: 0.75,
            },
            ..Adjustments::default()
        };
        let out = render(&card(), &adjustments, Target::Full);
        assert_eq!(out.raster.dimensions(), (79, 39));
        let view = render_view(&card(), &adjustments, Target::Full);
        assert_eq!(view.bitmap.raster.dimensions(), (157, 79));
        let Raster::Rgb(image) = &view.bitmap.raster else {
            panic!("expected color")
        };
        assert_eq!(
            image.get_pixel(156, 0),
            &Rgb([255, 255, 255]),
            "corners are filled with white"
        );
    }
}
