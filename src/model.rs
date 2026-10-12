//! Pages and the document they form.
//!
//! A page is the scanner's original image plus the [`Adjustments`] chosen
//! for it. Rendering always starts again from the original.

use crate::export::{ExportError, PdfBuilder, PdfImage, PdfOptions};
use crate::imaging::{self, Adjustments, Bitmap, DecodeError, OutputMode, Rotation, Target};
use image::ImageDecoder;
use image::codecs::jpeg::JpegDecoder;
use std::io::Cursor;
use std::sync::Arc;

/// An image as delivered by the scanner, kept in its compressed form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScannedImage {
    pub jpeg: Arc<[u8]>,
    pub dpi: u32,
    pub width: u32,
    pub height: u32,
    pub is_color: bool,
}

impl ScannedImage {
    /// Wraps a scanner JPEG, reading only its header.
    pub fn from_jpeg(jpeg: impl Into<Arc<[u8]>>, dpi: u32) -> Result<Self, DecodeError> {
        let jpeg = jpeg.into();
        let decoder = JpegDecoder::new(Cursor::new(&*jpeg))?;
        let (width, height) = decoder.dimensions();
        let is_color = decoder.color_type().has_color();
        Ok(ScannedImage {
            jpeg,
            dpi,
            width,
            height,
            is_color,
        })
    }

    /// Decodes the full-resolution image.
    pub fn decode(&self) -> Result<Bitmap, DecodeError> {
        imaging::decode(&self.jpeg, self.dpi as f32)
    }

    /// The output modes this image can produce: gray scans cannot become color.
    pub fn allowed_modes(&self) -> &'static [OutputMode] {
        if self.is_color {
            &OutputMode::ALL
        } else {
            &OutputMode::ALL[1..]
        }
    }

    /// Physical size in millimetres.
    pub fn size_mm(&self) -> (f32, f32) {
        let to_mm = |px: u32| px as f32 * 25.4 / self.dpi as f32;
        (to_mm(self.width), to_mm(self.height))
    }
}

/// Identifies a page independently of its position in the document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PageId(u64);

#[derive(Debug, Clone, PartialEq)]
pub struct Page {
    pub id: PageId,
    pub image: Arc<ScannedImage>,
    pub adjustments: Adjustments,
}

impl Page {
    /// Whether the output is exactly the scanner's image, so it can be
    /// saved without decoding and re-encoding.
    pub fn is_unmodified(&self) -> bool {
        let a = &self.adjustments;
        let mode_matches = match a.mode {
            OutputMode::Color => self.image.is_color,
            OutputMode::Gray => !self.image.is_color,
            OutputMode::BlackAndWhite => false,
        };
        mode_matches
            && a.rotation == Rotation::None
            && a.crop.is_full()
            && a.brightness == 0.0
            && a.contrast == 0.0
            && a.sharpen == 0.0
    }

    /// Renders the page at full resolution.
    pub fn render(&self) -> Result<Bitmap, DecodeError> {
        Ok(imaging::render(
            &self.image.decode()?,
            &self.adjustments,
            Target::Full,
        ))
    }

    /// Adds this page to a PDF, reusing the scanner's JPEG when possible.
    pub fn add_to_pdf(&self, pdf: &mut PdfBuilder) -> Result<(), ExportError> {
        if self.is_unmodified() {
            let dpi = self.image.dpi as f32;
            pdf.add_page(PdfImage::Jpeg {
                data: &self.image.jpeg,
                dpi,
            })
        } else {
            let bitmap = self.render()?;
            pdf.add_page(PdfImage::Rendered(&bitmap))
        }
    }
}

/// The ordered list of scanned pages.
#[derive(Debug, Clone, Default)]
pub struct Document {
    pages: Vec<Page>,
    next_id: u64,
    /// Incremented on every change.
    revision: u64,
    saved_revision: u64,
}

impl Document {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn pages(&self) -> &[Page] {
        &self.pages
    }

    pub fn len(&self) -> usize {
        self.pages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pages.is_empty()
    }

    /// Whether there are changes that have not been saved as a whole document.
    pub fn is_modified(&self) -> bool {
        self.revision != self.saved_revision && !self.pages.is_empty()
    }

    /// A counter that changes whenever the document does.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Records that the document was saved as it was at `revision`. Changes
    /// made after that revision (e.g. during a background save) keep the
    /// document modified.
    pub fn mark_saved(&mut self, revision: u64) {
        self.saved_revision = revision;
    }

    fn touch(&mut self) {
        self.revision += 1;
    }

    /// Appends a page. The mode is downgraded if the image cannot provide it.
    pub fn push(&mut self, image: ScannedImage, mut adjustments: Adjustments) -> PageId {
        if !image.allowed_modes().contains(&adjustments.mode) {
            adjustments.mode = OutputMode::Gray;
        }
        let id = PageId(self.next_id);
        self.next_id += 1;
        self.pages.push(Page {
            id,
            image: Arc::new(image),
            adjustments,
        });
        self.touch();
        id
    }

    pub fn get(&self, id: PageId) -> Option<&Page> {
        self.pages.iter().find(|p| p.id == id)
    }

    pub fn index_of(&self, id: PageId) -> Option<usize> {
        self.pages.iter().position(|p| p.id == id)
    }

    /// Replaces a page's adjustments. Returns false if nothing changed.
    pub fn set_adjustments(&mut self, id: PageId, adjustments: Adjustments) -> bool {
        let Some(page) = self.pages.iter_mut().find(|p| p.id == id) else {
            return false;
        };
        if page.adjustments == adjustments {
            return false;
        }
        page.adjustments = adjustments;
        self.touch();
        true
    }

    /// Removes a page and returns it.
    pub fn remove(&mut self, id: PageId) -> Option<Page> {
        let index = self.index_of(id)?;
        self.touch();
        Some(self.pages.remove(index))
    }

    /// Moves a page to `index` (clamped to the end). Returns false if the
    /// page does not exist or did not move.
    pub fn move_to(&mut self, id: PageId, index: usize) -> bool {
        let Some(from) = self.index_of(id) else {
            return false;
        };
        let to = index.min(self.pages.len() - 1);
        if from == to {
            return false;
        }
        let page = self.pages.remove(from);
        self.pages.insert(to, page);
        self.touch();
        true
    }

    pub fn clear(&mut self) {
        self.pages.clear();
        self.touch();
        self.saved_revision = self.revision;
    }

    /// Renders every page into one PDF. `progress` is called after each page
    /// with the number of pages done.
    pub fn to_pdf(
        &self,
        options: PdfOptions,
        mut progress: impl FnMut(usize),
    ) -> Result<Vec<u8>, ExportError> {
        let mut pdf = PdfBuilder::new(options);
        for (index, page) in self.pages.iter().enumerate() {
            page.add_to_pdf(&mut pdf)?;
            progress(index + 1);
        }
        pdf.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::imaging::Crop;
    use image::ExtendedColorType;
    use image::codecs::jpeg::JpegEncoder;

    fn jpeg(color: bool) -> ScannedImage {
        let mut data = Vec::new();
        let (pixels, kind) = if color {
            (vec![120u8; 30 * 20 * 3], ExtendedColorType::Rgb8)
        } else {
            (vec![120u8; 30 * 20], ExtendedColorType::L8)
        };
        JpegEncoder::new(&mut data)
            .encode(&pixels, 30, 20, kind)
            .unwrap();
        ScannedImage::from_jpeg(data, 150).unwrap()
    }

    #[test]
    fn reads_jpeg_headers() {
        let image = jpeg(true);
        assert_eq!((image.width, image.height, image.is_color), (30, 20, true));
        assert_eq!(image.allowed_modes().len(), 3);
        assert_eq!(
            jpeg(false).allowed_modes(),
            &[OutputMode::Gray, OutputMode::BlackAndWhite]
        );
        let (w, h) = image.size_mm();
        assert!((w - 5.08).abs() < 0.01 && (h - 3.387).abs() < 0.01);
        assert!(ScannedImage::from_jpeg(vec![1, 2, 3], 300).is_err());
    }

    #[test]
    fn downgrades_impossible_modes() {
        let mut doc = Document::new();
        let id = doc.push(jpeg(false), Adjustments::default());
        assert_eq!(doc.get(id).unwrap().adjustments.mode, OutputMode::Gray);
    }

    #[test]
    fn detects_unmodified_pages() {
        let mut doc = Document::new();
        let color = doc.push(jpeg(true), Adjustments::default());
        assert!(doc.get(color).unwrap().is_unmodified());
        let cropped = Adjustments {
            crop: Crop {
                left: 0.1,
                ..Crop::FULL
            },
            ..Adjustments::default()
        };
        doc.set_adjustments(color, cropped);
        assert!(!doc.get(color).unwrap().is_unmodified());

        let gray = doc.push(
            jpeg(true),
            Adjustments {
                mode: OutputMode::Gray,
                ..Adjustments::default()
            },
        );
        assert!(
            !doc.get(gray).unwrap().is_unmodified(),
            "color scan saved as gray needs conversion"
        );
    }

    #[test]
    fn reorders_and_removes_pages() {
        let mut doc = Document::new();
        let ids: Vec<_> = (0..3)
            .map(|_| doc.push(jpeg(true), Adjustments::default()))
            .collect();
        let saved = doc.revision();
        doc.mark_saved(saved);
        assert!(!doc.is_modified());

        assert!(doc.move_to(ids[0], 99));
        assert_eq!(
            doc.pages().iter().map(|p| p.id).collect::<Vec<_>>(),
            [ids[1], ids[2], ids[0]]
        );
        assert!(!doc.move_to(ids[0], 2), "already there");
        assert!(doc.is_modified());
        doc.mark_saved(saved);
        assert!(doc.is_modified(), "changes after the saved revision remain");

        assert_eq!(doc.remove(ids[2]).unwrap().id, ids[2]);
        assert_eq!(doc.index_of(ids[0]), Some(1));
        assert!(doc.remove(ids[2]).is_none());
        assert!(!doc.set_adjustments(ids[2], Adjustments::default()));

        doc.clear();
        assert!(doc.is_empty() && !doc.is_modified());
    }

    #[test]
    fn exports_pdf_with_progress() {
        let mut doc = Document::new();
        doc.push(jpeg(true), Adjustments::default());
        doc.push(
            jpeg(true),
            Adjustments {
                mode: OutputMode::BlackAndWhite,
                ..Adjustments::default()
            },
        );
        let mut done = Vec::new();
        let pdf = doc.to_pdf(PdfOptions::default(), |n| done.push(n)).unwrap();
        assert!(pdf.starts_with(b"%PDF-"));
        assert_eq!(done, [1, 2]);
    }
}
