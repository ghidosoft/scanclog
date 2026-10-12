//! CCITT Group 4 (T.6) encoding of black-and-white images.

use fax::encoder::Encoder;
use fax::{Color, VecWriter};
use image::GrayImage;

/// Encodes a bilevel image (pixels 0 = black, anything else = white) as a
/// raw G4 stream, suitable for PDF `CCITTFaxDecode` with `K = -1` and
/// `BlackIs1 = false`.
pub fn encode_g4(image: &GrayImage) -> Vec<u8> {
    let width = image.width();
    let mut encoder = Encoder::new(VecWriter::new());
    for row in image.rows() {
        let pels = row.map(|pixel| {
            if pixel.0[0] == 0 {
                Color::Black
            } else {
                Color::White
            }
        });
        // VecWriter writes into memory and cannot fail.
        let Ok(()) = encoder.encode_line(pels, width);
    }
    let Ok(writer) = encoder.finish();
    writer.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use fax::decoder;
    use image::Luma;

    fn decode(data: &[u8], width: u32, height: u32) -> GrayImage {
        let mut image = GrayImage::new(width, height);
        let mut y = 0;
        decoder::decode_g4(data.iter().copied(), width, Some(height), |transitions| {
            for (x, color) in decoder::pels(transitions, width).enumerate() {
                let value = if color == Color::Black { 0 } else { 255 };
                image.put_pixel(x as u32, y, Luma([value]));
            }
            y += 1;
        });
        assert_eq!(y, height, "all rows decoded");
        image
    }

    #[test]
    fn round_trips_through_the_decoder() {
        let image = GrayImage::from_fn(123, 45, |x, y| {
            let black = (x / 7 + y / 5) % 3 == 0 || x == y;
            Luma([if black { 0 } else { 255 }])
        });
        let encoded = encode_g4(&image);
        assert!(
            encoded.len() < (123 * 45 / 8),
            "compresses: {} bytes",
            encoded.len()
        );
        assert_eq!(decode(&encoded, 123, 45), image);
    }

    #[test]
    fn compresses_blank_pages_to_almost_nothing() {
        let blank = GrayImage::from_pixel(2480, 3508, Luma([255]));
        assert!(encode_g4(&blank).len() < 1024);
    }
}
