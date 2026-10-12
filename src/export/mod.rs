//! Writing pages to PDF, PNG and JPEG files.

mod ccitt;
mod pdf;
mod raster;

pub use pdf::{PdfBuilder, PdfImage, PdfOptions, write_pdf};
pub use raster::{encode_jpeg, encode_png};

use std::io::Write;
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    #[error("there are no pages to save")]
    NoPages,
    #[error("unsupported JPEG with {0} color channels")]
    UnsupportedJpeg(u8),
    #[error(transparent)]
    Decode(#[from] crate::imaging::DecodeError),
    #[error("cannot encode the image: {0}")]
    Image(#[from] image::ImageError),
    #[error("cannot encode the PNG file: {0}")]
    Png(#[from] png::EncodingError),
    #[error("cannot write the file: {0}")]
    Io(#[from] std::io::Error),
}

/// Single-file output formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileFormat {
    Pdf,
    Png,
    Jpeg,
}

impl FileFormat {
    pub const ALL: [FileFormat; 3] = [FileFormat::Pdf, FileFormat::Png, FileFormat::Jpeg];

    pub fn extension(self) -> &'static str {
        match self {
            FileFormat::Pdf => "pdf",
            FileFormat::Png => "png",
            FileFormat::Jpeg => "jpg",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            FileFormat::Pdf => "PDF document",
            FileFormat::Png => "PNG image",
            FileFormat::Jpeg => "JPEG image",
        }
    }

    pub fn mime_type(self) -> &'static str {
        match self {
            FileFormat::Pdf => "application/pdf",
            FileFormat::Png => "image/png",
            FileFormat::Jpeg => "image/jpeg",
        }
    }

    /// Guesses the format from a file name's extension.
    pub fn from_path(path: &Path) -> Option<Self> {
        let extension = path.extension()?.to_str()?.to_ascii_lowercase();
        match extension.as_str() {
            "pdf" => Some(FileFormat::Pdf),
            "png" => Some(FileFormat::Png),
            "jpg" | "jpeg" => Some(FileFormat::Jpeg),
            _ => None,
        }
    }
}

/// Writes `data` to `path` through a temporary file in the same directory,
/// so an interrupted save never leaves a truncated file behind.
pub fn write_atomically(path: &Path, data: &[u8]) -> Result<(), ExportError> {
    let directory = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_default();
    let temporary = directory.join(format!(".{file_name}.{}.tmp", std::process::id()));
    let result = (|| {
        let mut file = std::fs::File::create(&temporary)?;
        file.write_all(data)?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)
    })();
    if result.is_err() {
        // Best effort: the temporary file may not even exist.
        let _ = std::fs::remove_file(&temporary);
    }
    Ok(result?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_formats_from_paths() {
        assert_eq!(
            FileFormat::from_path(Path::new("a/b.PDF")),
            Some(FileFormat::Pdf)
        );
        assert_eq!(
            FileFormat::from_path(Path::new("x.jpeg")),
            Some(FileFormat::Jpeg)
        );
        assert_eq!(FileFormat::from_path(Path::new("x.tiff")), None);
        assert_eq!(FileFormat::from_path(Path::new("noext")), None);
    }

    #[test]
    fn writes_atomically() {
        let dir = std::env::temp_dir().join(format!("scanclog-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("out.bin");
        write_atomically(&path, b"first").unwrap();
        write_atomically(&path, b"second").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"second");
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            1,
            "no temporary files left"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
