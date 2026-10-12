//! Single-image output as PNG or JPEG, with the resolution recorded in the file.

use super::ExportError;
use crate::imaging::{Bitmap, Raster};
use image::ExtendedColorType;
use image::codecs::jpeg::{JpegEncoder, PixelDensity};
use png::{BitDepth, ColorType, PixelDimensions, Unit};

/// Encodes a bitmap as PNG. Black-and-white pages are written with one bit
/// per pixel.
pub fn encode_png(bitmap: &Bitmap) -> Result<Vec<u8>, ExportError> {
    let (width, height) = bitmap.raster.dimensions();
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, width, height);
    let packed;
    let (color, depth, data): (ColorType, BitDepth, &[u8]) = match &bitmap.raster {
        Raster::Rgb(image) => (ColorType::Rgb, BitDepth::Eight, image),
        Raster::Gray(image) => (ColorType::Grayscale, BitDepth::Eight, image),
        Raster::Bilevel(image) => {
            packed = pack_bits(image);
            (ColorType::Grayscale, BitDepth::One, &packed)
        }
    };
    encoder.set_color(color);
    encoder.set_depth(depth);
    encoder.set_compression(png::Compression::High);
    let pixels_per_metre = (bitmap.dpi / 0.0254).round() as u32;
    encoder.set_pixel_dims(Some(PixelDimensions {
        xppu: pixels_per_metre,
        yppu: pixels_per_metre,
        unit: Unit::Meter,
    }));
    let mut writer = encoder.write_header()?;
    writer.write_image_data(data)?;
    writer.finish()?;
    Ok(out)
}

/// Encodes a bitmap as JPEG. Black-and-white pages become gray JPEGs.
pub fn encode_jpeg(bitmap: &Bitmap, quality: u8) -> Result<Vec<u8>, ExportError> {
    let (width, height) = bitmap.raster.dimensions();
    let mut out = Vec::new();
    let mut encoder = JpegEncoder::new_with_quality(&mut out, quality);
    encoder.set_pixel_density(PixelDensity::dpi(
        bitmap.dpi.round().clamp(1.0, f32::from(u16::MAX)) as u16,
    ));
    match &bitmap.raster {
        Raster::Rgb(image) => encoder.encode(image, width, height, ExtendedColorType::Rgb8)?,
        Raster::Gray(image) | Raster::Bilevel(image) => {
            encoder.encode(image, width, height, ExtendedColorType::L8)?
        }
    }
    Ok(out)
}

/// Packs a bilevel image into rows of bits, most significant bit first,
/// 1 = white (PNG grayscale convention).
fn pack_bits(image: &image::GrayImage) -> Vec<u8> {
    let row_bytes = (image.width() as usize).div_ceil(8);
    let mut packed = vec![0u8; row_bytes * image.height() as usize];
    for (row, out) in image.rows().zip(packed.chunks_exact_mut(row_bytes)) {
        for (x, pixel) in row.enumerate() {
            if pixel.0[0] != 0 {
                out[x / 8] |= 0x80 >> (x % 8);
            }
        }
    }
    packed
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{GrayImage, Luma, RgbImage};

    fn bilevel() -> Bitmap {
        let image =
            GrayImage::from_fn(13, 3, |x, y| Luma([if (x + y) % 2 == 0 { 0 } else { 255 }]));
        Bitmap {
            raster: Raster::Bilevel(image),
            dpi: 300.0,
        }
    }

    #[test]
    fn packs_bits_msb_first() {
        let image =
            GrayImage::from_fn(10, 1, |x, _| Luma([if x == 0 || x == 9 { 255 } else { 0 }]));
        assert_eq!(pack_bits(&image), vec![0b1000_0000, 0b0100_0000]);
    }

    #[test]
    fn png_round_trips_with_resolution() {
        let source = bilevel();
        let data = encode_png(&source).unwrap();
        let decoded = image::load_from_memory(&data).unwrap().into_luma8();
        let Raster::Bilevel(expected) = &source.raster else {
            unreachable!()
        };
        assert_eq!(&decoded, expected);

        let reader = png::Decoder::new(std::io::Cursor::new(&data))
            .read_info()
            .unwrap();
        let info = reader.info();
        assert_eq!(info.bit_depth, BitDepth::One);
        assert_eq!(
            info.pixel_dims.unwrap().xppu,
            11811,
            "300 dpi in pixels per metre"
        );
    }

    #[test]
    fn jpeg_keeps_size_and_color() {
        let bitmap = Bitmap {
            raster: Raster::Rgb(RgbImage::from_pixel(20, 10, image::Rgb([10, 200, 30]))),
            dpi: 150.0,
        };
        let decoded = image::load_from_memory(&encode_jpeg(&bitmap, 90).unwrap()).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (20, 10));
        assert!(decoded.color().has_color());
        let gray = image::load_from_memory(&encode_jpeg(&bilevel(), 90).unwrap()).unwrap();
        assert!(!gray.color().has_color());
    }
}
