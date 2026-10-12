//! Image processing: decoding scans and rendering them with per-page adjustments.

mod adjust;
mod deskew;
mod detect;
mod ops;
mod pipeline;

pub use adjust::{Adjustments, Crop, OutputMode, Rotation, Threshold};
pub use deskew::{MAX_SKEW, detect_skew};
pub use detect::detect_document;
pub use ops::SHARPEN_RADIUS_MM;
pub use pipeline::{
    Bitmap, DecodeError, Raster, Target, View, decode, render, render_view, threshold_level,
};
