//! Per-page processing parameters.
//!
//! Every value here is a description, not an operation: the pipeline applies
//! them to the original scan each time, so changing a setting never loses
//! information.

use serde::{Deserialize, Serialize};

/// The kind of image a page produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum OutputMode {
    #[default]
    Color,
    Gray,
    /// One bit per pixel, produced by thresholding the gray image.
    BlackAndWhite,
}

impl OutputMode {
    pub const ALL: [OutputMode; 3] = [
        OutputMode::Color,
        OutputMode::Gray,
        OutputMode::BlackAndWhite,
    ];

    pub fn label(self) -> &'static str {
        match self {
            OutputMode::Color => "Color",
            OutputMode::Gray => "Grayscale",
            OutputMode::BlackAndWhite => "Black & white",
        }
    }

    /// Whether the scanner has to deliver color data for this mode.
    pub fn needs_color(self) -> bool {
        self == OutputMode::Color
    }
}

/// Clockwise rotation in quarter turns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Rotation {
    #[default]
    None,
    Cw90,
    Cw180,
    Cw270,
}

impl Rotation {
    pub fn clockwise(self) -> Self {
        match self {
            Rotation::None => Rotation::Cw90,
            Rotation::Cw90 => Rotation::Cw180,
            Rotation::Cw180 => Rotation::Cw270,
            Rotation::Cw270 => Rotation::None,
        }
    }

    pub fn counter_clockwise(self) -> Self {
        self.clockwise().clockwise().clockwise()
    }

    /// Whether width and height are swapped.
    pub fn is_sideways(self) -> bool {
        matches!(self, Rotation::Cw90 | Rotation::Cw270)
    }
}

/// How black and white is separated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Threshold {
    /// Gray levels above this value become white.
    Manual(u8),
    /// The level is chosen from the image histogram (Otsu's method).
    #[default]
    Auto,
}

/// A crop rectangle in normalized coordinates (0..=1) of the *rotated*
/// image, i.e. the frame the user sees.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Crop {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

impl Crop {
    pub const FULL: Crop = Crop {
        left: 0.0,
        top: 0.0,
        right: 1.0,
        bottom: 1.0,
    };

    /// Builds a crop from two corners in any order, clamped to the image.
    pub fn from_corners(a: (f32, f32), b: (f32, f32)) -> Self {
        let clamp = |v: f32| v.clamp(0.0, 1.0);
        Crop {
            left: clamp(a.0.min(b.0)),
            top: clamp(a.1.min(b.1)),
            right: clamp(a.0.max(b.0)),
            bottom: clamp(a.1.max(b.1)),
        }
    }

    pub fn width(&self) -> f32 {
        self.right - self.left
    }

    pub fn height(&self) -> f32 {
        self.bottom - self.top
    }

    pub fn is_full(&self) -> bool {
        *self == Crop::FULL
    }

    /// The same area expressed in the unrotated source frame.
    pub fn to_source(self, rotation: Rotation) -> Crop {
        let Crop {
            left: l,
            top: t,
            right: r,
            bottom: b,
        } = self;
        match rotation {
            Rotation::None => self,
            Rotation::Cw90 => Crop {
                left: t,
                top: 1.0 - r,
                right: b,
                bottom: 1.0 - l,
            },
            Rotation::Cw180 => Crop {
                left: 1.0 - r,
                top: 1.0 - b,
                right: 1.0 - l,
                bottom: 1.0 - t,
            },
            Rotation::Cw270 => Crop {
                left: 1.0 - b,
                top: l,
                right: 1.0 - t,
                bottom: r,
            },
        }
    }

    /// The same area expressed in the frame after applying `rotation`.
    pub fn from_source(self, rotation: Rotation) -> Crop {
        // Rotating by the inverse maps the frame back.
        let inverse = match rotation {
            Rotation::Cw90 => Rotation::Cw270,
            Rotation::Cw270 => Rotation::Cw90,
            other => other,
        };
        self.to_source(inverse)
    }

    /// Pixel bounds `(x, y, width, height)` in an image of the given size,
    /// at least one pixel in each direction.
    pub fn to_pixels(self, width: u32, height: u32) -> (u32, u32, u32, u32) {
        let to_px =
            |v: f32, size: u32| ((v.clamp(0.0, 1.0) * size as f32).round() as u32).min(size);
        let x0 = to_px(self.left, width).min(width.saturating_sub(1));
        let y0 = to_px(self.top, height).min(height.saturating_sub(1));
        let x1 = to_px(self.right, width).max(x0 + 1);
        let y1 = to_px(self.bottom, height).max(y0 + 1);
        (x0, y0, x1 - x0, y1 - y0)
    }
}

impl Default for Crop {
    fn default() -> Self {
        Crop::FULL
    }
}

/// All processing parameters of one page.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Adjustments {
    pub mode: OutputMode,
    pub rotation: Rotation,
    /// Fine rotation in degrees, clockwise, applied after `rotation` to
    /// straighten a tilted page.
    pub straighten: f32,
    /// Crop in the frame after rotation and straightening.
    pub crop: Crop,
    /// -1.0 (darker) ..= 1.0 (lighter).
    pub brightness: f32,
    /// -1.0 (flatter) ..= 1.0 (stronger).
    pub contrast: f32,
    /// Unsharp-mask strength, 0.0 (off) ..= 2.0.
    pub sharpen: f32,
    /// Only used in [`OutputMode::BlackAndWhite`].
    pub threshold: Threshold,
}

impl Default for Adjustments {
    fn default() -> Self {
        Adjustments {
            mode: OutputMode::default(),
            rotation: Rotation::default(),
            straighten: 0.0,
            crop: Crop::FULL,
            brightness: 0.0,
            contrast: 0.0,
            sharpen: 0.0,
            threshold: Threshold::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: Crop = Crop {
        left: 0.1,
        top: 0.2,
        right: 0.5,
        bottom: 0.9,
    };

    fn assert_close(a: Crop, b: Crop) {
        let close = |x: f32, y: f32| (x - y).abs() < 1e-6;
        assert!(
            close(a.left, b.left)
                && close(a.top, b.top)
                && close(a.right, b.right)
                && close(a.bottom, b.bottom),
            "{a:?} != {b:?}"
        );
    }

    #[test]
    fn crop_round_trips_through_rotations() {
        for rotation in [
            Rotation::None,
            Rotation::Cw90,
            Rotation::Cw180,
            Rotation::Cw270,
        ] {
            assert_close(SAMPLE.to_source(rotation).from_source(rotation), SAMPLE);
        }
    }

    #[test]
    fn crop_maps_quarter_turns() {
        // The top-left corner of a 90° clockwise view is the source's bottom-left.
        let corner = Crop {
            left: 0.0,
            top: 0.0,
            right: 0.25,
            bottom: 0.5,
        };
        assert_close(
            corner.to_source(Rotation::Cw90),
            Crop {
                left: 0.0,
                top: 0.75,
                right: 0.5,
                bottom: 1.0,
            },
        );
        assert_close(
            corner.to_source(Rotation::Cw180),
            Crop {
                left: 0.75,
                top: 0.5,
                right: 1.0,
                bottom: 1.0,
            },
        );
        assert_close(
            corner.to_source(Rotation::Cw270),
            Crop {
                left: 0.5,
                top: 0.0,
                right: 1.0,
                bottom: 0.25,
            },
        );
    }

    #[test]
    fn crop_converts_to_pixels() {
        assert_eq!(Crop::FULL.to_pixels(100, 50), (0, 0, 100, 50));
        assert_eq!(SAMPLE.to_pixels(100, 100), (10, 20, 40, 70));
        // Degenerate crops still produce one pixel.
        let empty = Crop {
            left: 1.0,
            top: 1.0,
            right: 1.0,
            bottom: 1.0,
        };
        assert_eq!(empty.to_pixels(10, 10), (9, 9, 1, 1));
    }

    #[test]
    fn crop_from_corners_normalizes() {
        let crop = Crop::from_corners((0.8, 1.4), (0.2, -0.1));
        assert_eq!(
            crop,
            Crop {
                left: 0.2,
                top: 0.0,
                right: 0.8,
                bottom: 1.0
            }
        );
    }

    #[test]
    fn rotation_cycles() {
        assert_eq!(Rotation::None.counter_clockwise(), Rotation::Cw270);
        assert_eq!(Rotation::Cw270.clockwise(), Rotation::None);
        assert!(Rotation::Cw90.is_sideways());
    }
}
