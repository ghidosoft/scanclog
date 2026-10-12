//! Multi-page PDF output with one image per page.
//!
//! Color and gray pages are embedded as JPEG (`DCTDecode`), black-and-white
//! pages as CCITT Group 4 (`CCITTFaxDecode`), which is what office scanners
//! produce: an A4 page of text at 300 dpi is typically 30–60 KB.

use super::ExportError;
use crate::imaging::{Bitmap, Raster};
use image::codecs::jpeg::{JpegDecoder, JpegEncoder};
use image::{ExtendedColorType, ImageDecoder};
use pdf_writer::{Content, Filter, Finish, Name, Pdf, Rect, Ref, TextStr};
use std::io::Cursor;

/// The image placed on one page.
#[derive(Debug, Clone, Copy)]
pub enum PdfImage<'a> {
    /// A rendered page, encoded according to its raster type.
    Rendered(&'a Bitmap),
    /// A JPEG embedded byte for byte, avoiding a second lossy encoding when
    /// a page has no adjustments.
    Jpeg { data: &'a [u8], dpi: f32 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PdfOptions {
    /// Quality used when color or gray pages must be re-encoded.
    pub jpeg_quality: u8,
    pub title: Option<String>,
}

impl Default for PdfOptions {
    fn default() -> Self {
        PdfOptions {
            jpeg_quality: 90,
            title: None,
        }
    }
}

/// An encoded image stream and how to declare it.
struct EncodedImage {
    data: Vec<u8>,
    width: u32,
    height: u32,
    dpi: f32,
    kind: ImageKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImageKind {
    JpegRgb,
    JpegGray,
    Ccitt,
}

/// Builds a PDF one page at a time, so callers can render, add and drop each
/// page without holding every full-resolution bitmap in memory.
pub struct PdfBuilder {
    pdf: Pdf,
    options: PdfOptions,
    next_id: i32,
    catalog_id: Ref,
    page_tree_id: Ref,
    info_id: Ref,
    page_ids: Vec<Ref>,
}

impl PdfBuilder {
    pub fn new(options: PdfOptions) -> Self {
        PdfBuilder {
            pdf: Pdf::new(),
            options,
            next_id: 4,
            catalog_id: Ref::new(1),
            page_tree_id: Ref::new(2),
            info_id: Ref::new(3),
            page_ids: Vec::new(),
        }
    }

    fn allocate(&mut self) -> Ref {
        let id = Ref::new(self.next_id);
        self.next_id += 1;
        id
    }

    pub fn page_count(&self) -> usize {
        self.page_ids.len()
    }

    /// Encodes `image` and appends it as a new page.
    pub fn add_page(&mut self, image: PdfImage<'_>) -> Result<(), ExportError> {
        let image = encode(image, self.options.jpeg_quality)?;
        let (page_id, image_id, content_id) = (self.allocate(), self.allocate(), self.allocate());
        self.page_ids.push(page_id);
        write_image(&mut self.pdf, image_id, &image);

        // PDF user space is in points (1/72 in).
        let width_pt = image.width as f32 * 72.0 / image.dpi;
        let height_pt = image.height as f32 * 72.0 / image.dpi;
        let image_name = Name(b"Im0");
        let mut page = self.pdf.page(page_id);
        page.parent(self.page_tree_id)
            .media_box(Rect::new(0.0, 0.0, width_pt, height_pt))
            .contents(content_id);
        page.resources().x_objects().pair(image_name, image_id);
        page.finish();

        let mut content = Content::new();
        content
            .save_state()
            .transform([width_pt, 0.0, 0.0, height_pt, 0.0, 0.0])
            .x_object(image_name)
            .restore_state();
        self.pdf.stream(content_id, &content.finish());
        Ok(())
    }

    /// Completes the document and returns its bytes.
    pub fn finish(mut self) -> Result<Vec<u8>, ExportError> {
        if self.page_ids.is_empty() {
            return Err(ExportError::NoPages);
        }
        self.pdf.catalog(self.catalog_id).pages(self.page_tree_id);
        self.pdf
            .pages(self.page_tree_id)
            .count(self.page_ids.len() as i32)
            .kids(self.page_ids.iter().copied());
        let mut info = self.pdf.document_info(self.info_id);
        info.producer(TextStr(concat!("ScanClog ", env!("CARGO_PKG_VERSION"))));
        if let Some(title) = &self.options.title {
            info.title(TextStr(title));
        }
        info.finish();
        Ok(self.pdf.finish())
    }
}

/// Writes all pages into a single PDF document.
pub fn write_pdf<'a>(
    pages: impl IntoIterator<Item = PdfImage<'a>>,
    options: &PdfOptions,
) -> Result<Vec<u8>, ExportError> {
    let mut builder = PdfBuilder::new(options.clone());
    for page in pages {
        builder.add_page(page)?;
    }
    builder.finish()
}

fn write_image(pdf: &mut Pdf, id: Ref, image: &EncodedImage) {
    let mut xobject = pdf.image_xobject(id, &image.data);
    xobject
        .width(image.width as i32)
        .height(image.height as i32);
    match image.kind {
        ImageKind::JpegRgb => {
            xobject.bits_per_component(8).filter(Filter::DctDecode);
            xobject.color_space().device_rgb();
        }
        ImageKind::JpegGray => {
            xobject.bits_per_component(8).filter(Filter::DctDecode);
            xobject.color_space().device_gray();
        }
        ImageKind::Ccitt => {
            xobject.bits_per_component(1).filter(Filter::CcittFaxDecode);
            xobject.color_space().device_gray();
            xobject
                .decode_parms()
                .k(-1)
                .columns(image.width as i32)
                .rows(image.height as i32)
                .black_is_1(false);
        }
    }
}

fn encode(page: PdfImage<'_>, jpeg_quality: u8) -> Result<EncodedImage, ExportError> {
    match page {
        PdfImage::Jpeg { data, dpi } => {
            let decoder = JpegDecoder::new(Cursor::new(data))?;
            let (width, height) = decoder.dimensions();
            let kind = match decoder.color_type().channel_count() {
                1 => ImageKind::JpegGray,
                3 => ImageKind::JpegRgb,
                n => return Err(ExportError::UnsupportedJpeg(n)),
            };
            Ok(EncodedImage {
                data: data.to_vec(),
                width,
                height,
                dpi,
                kind,
            })
        }
        PdfImage::Rendered(bitmap) => {
            let (width, height) = bitmap.raster.dimensions();
            let (data, kind) = match &bitmap.raster {
                Raster::Rgb(image) => {
                    let data = encode_jpeg(
                        image,
                        (width, height),
                        ExtendedColorType::Rgb8,
                        jpeg_quality,
                    )?;
                    (data, ImageKind::JpegRgb)
                }
                Raster::Gray(image) => {
                    let data =
                        encode_jpeg(image, (width, height), ExtendedColorType::L8, jpeg_quality)?;
                    (data, ImageKind::JpegGray)
                }
                Raster::Bilevel(image) => (super::ccitt::encode_g4(image), ImageKind::Ccitt),
            };
            Ok(EncodedImage {
                data,
                width,
                height,
                dpi: bitmap.dpi,
                kind,
            })
        }
    }
}

fn encode_jpeg(
    pixels: &[u8],
    (width, height): (u32, u32),
    color: ExtendedColorType,
    quality: u8,
) -> Result<Vec<u8>, ExportError> {
    let mut out = Vec::new();
    JpegEncoder::new_with_quality(&mut out, quality).encode(pixels, width, height, color)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{GrayImage, Luma, Rgb, RgbImage};

    fn contains(haystack: &[u8], needle: &str) -> bool {
        haystack
            .windows(needle.len())
            .any(|w| w == needle.as_bytes())
    }

    #[test]
    fn writes_one_page_per_image() {
        let color = Bitmap {
            raster: Raster::Rgb(RgbImage::from_pixel(300, 150, Rgb([200, 10, 10]))),
            dpi: 300.0,
        };
        let bilevel = Bitmap {
            raster: Raster::Bilevel(GrayImage::from_fn(600, 300, |x, _| {
                Luma([if x < 300 { 0 } else { 255 }])
            })),
            dpi: 600.0,
        };
        let mut jpeg = Vec::new();
        JpegEncoder::new(&mut jpeg)
            .encode(&[128; 8 * 4], 8, 4, ExtendedColorType::L8)
            .unwrap();

        let options = PdfOptions {
            title: Some("Test".into()),
            ..PdfOptions::default()
        };
        let pages = [
            PdfImage::Rendered(&color),
            PdfImage::Rendered(&bilevel),
            PdfImage::Jpeg {
                data: &jpeg,
                dpi: 72.0,
            },
        ];
        let pdf = write_pdf(pages, &options).unwrap();

        assert!(pdf.starts_with(b"%PDF-"));
        assert!(contains(&pdf, "/Count 3"));
        assert!(contains(&pdf, "/CCITTFaxDecode"));
        assert!(contains(&pdf, "/K -1"));
        assert!(contains(&pdf, "/DeviceRGB"));
        // 300 px at 300 dpi and 600 px at 600 dpi are both one inch wide.
        assert!(contains(&pdf, "/MediaBox [0 0 72 36]"));
        // The passthrough JPEG is embedded unchanged.
        assert!(pdf.windows(jpeg.len()).any(|w| w == jpeg));
    }

    #[test]
    fn rejects_empty_documents() {
        assert!(matches!(
            write_pdf([], &PdfOptions::default()),
            Err(ExportError::NoPages)
        ));
    }
}
