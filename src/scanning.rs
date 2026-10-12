//! Turns the user's choices into eSCL scan jobs.

use crate::escl::{
    AdfState, Capabilities, Client, ColorMode, InputSource, Intent, ScanRegion, ScanSettings,
    SourceCaps, UNITS_PER_INCH,
};
use crate::imaging::{Crop, Rotation};
use serde::{Deserialize, Serialize};

/// Standard paper sizes for the document feeder, where there is no preview
/// to draw a crop on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum PaperSize {
    #[default]
    A4,
    A5,
    Letter,
    Legal,
}

impl PaperSize {
    pub const ALL: [PaperSize; 4] = [
        PaperSize::A4,
        PaperSize::A5,
        PaperSize::Letter,
        PaperSize::Legal,
    ];

    pub fn label(self) -> &'static str {
        match self {
            PaperSize::A4 => "A4",
            PaperSize::A5 => "A5",
            PaperSize::Letter => "Letter",
            PaperSize::Legal => "Legal",
        }
    }

    /// Width and height in millimetres.
    pub fn size_mm(self) -> (f64, f64) {
        match self {
            PaperSize::A4 => (210.0, 297.0),
            PaperSize::A5 => (148.0, 210.0),
            PaperSize::Letter => (215.9, 279.4),
            PaperSize::Legal => (215.9, 355.6),
        }
    }
}

/// Which part of the input to scan.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ScanArea {
    /// The whole scannable area.
    Full,
    /// A rectangle in normalized coordinates of the (unrotated) scanner bed.
    Region(Crop),
    /// A sheet of a standard size, aligned to the top-left corner.
    Paper(PaperSize),
}

/// What the user asked to scan.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScanRequest {
    pub source: InputSource,
    pub resolution: u32,
    pub area: ScanArea,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScanSetupError {
    #[error("this scanner has no {}", .0.label().to_lowercase())]
    SourceUnavailable(InputSource),
    #[error("this scanner cannot send JPEG images")]
    NoJpeg,
    #[error("this scanner offers no 8-bit color or grayscale mode")]
    NoColorMode,
    #[error("this scanner reports no resolutions")]
    NoResolution,
}

#[derive(Debug, thiserror::Error)]
pub enum ReadinessError {
    #[error("the document feeder is empty")]
    FeederEmpty,
    #[error("the document feeder is jammed")]
    FeederJam,
    #[error("the document feeder is open")]
    FeederOpen,
    #[error(transparent)]
    Escl(#[from] crate::escl::Error),
}

/// How to scan an area chosen on a preview that is shown rotated and
/// straightened.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RegionPlan {
    /// The area of the bed to scan, normalized, in the bed's own frame.
    pub region: Crop,
    /// The crop to give the new page, in its frame after the same rotation
    /// and straightening, so that it shows exactly the chosen area.
    pub page_crop: Crop,
}

/// Plans the scan of `crop`, drawn on a preview of a `bed_width` ×
/// `bed_height` bed shown with `rotation` and `straighten` (degrees
/// clockwise).
///
/// Scanners only scan rectangles aligned with the bed, so a straightened
/// crop is scanned through its bounding box; the page then gets the same
/// straightening and a crop that cuts the chosen area out of it.
pub fn plan_region(
    crop: Crop,
    rotation: Rotation,
    straighten: f32,
    bed_width: u32,
    bed_height: u32,
) -> RegionPlan {
    if straighten.abs() < 0.01 {
        return RegionPlan {
            region: crop.to_source(rotation),
            page_crop: Crop::FULL,
        };
    }
    // Work in the rotated frame, in bed units, so angles are not distorted.
    let (w, h) = if rotation.is_sideways() {
        (f64::from(bed_height), f64::from(bed_width))
    } else {
        (f64::from(bed_width), f64::from(bed_height))
    };
    let angle = f64::from(straighten).to_radians();
    let rotate = |(x, y): (f64, f64), angle: f64, (cx, cy): (f64, f64)| {
        let (sin, cos) = angle.sin_cos();
        let (dx, dy) = (x - cx, y - cy);
        (dx * cos - dy * sin + cx, dx * sin + dy * cos + cy)
    };
    let corners = [
        (crop.left, crop.top),
        (crop.right, crop.top),
        (crop.left, crop.bottom),
        (crop.right, crop.bottom),
    ]
    .map(|(x, y)| (f64::from(x) * w, f64::from(y) * h));

    // Where the crop corners lie on the unstraightened bed.
    let bed_center = (w / 2.0, h / 2.0);
    let on_bed = corners.map(|c| rotate(c, -angle, bed_center));
    let (min_x, max_x) = bounds(on_bed.map(|p| p.0), w);
    let (min_y, max_y) = bounds(on_bed.map(|p| p.1), h);
    let oriented = Crop {
        left: (min_x / w) as f32,
        top: (min_y / h) as f32,
        right: (max_x / w) as f32,
        bottom: (max_y / h) as f32,
    };

    // The scanned page is straightened around its own center.
    let (page_w, page_h) = ((max_x - min_x).max(1.0), (max_y - min_y).max(1.0));
    let page_center = ((min_x + max_x) / 2.0, (min_y + max_y) / 2.0);
    let on_page = on_bed.map(|p| {
        let (x, y) = rotate(p, angle, page_center);
        (((x - min_x) / page_w) as f32, ((y - min_y) / page_h) as f32)
    });
    let page_min = on_page
        .iter()
        .fold((f32::MAX, f32::MAX), |m, p| (m.0.min(p.0), m.1.min(p.1)));
    let page_max = on_page
        .iter()
        .fold((f32::MIN, f32::MIN), |m, p| (m.0.max(p.0), m.1.max(p.1)));
    RegionPlan {
        region: oriented.to_source(rotation),
        page_crop: Crop::from_corners(page_min, page_max),
    }
}

/// The range covered by `values`, clamped to `0..=limit`.
fn bounds(values: [f64; 4], limit: f64) -> (f64, f64) {
    let min = values
        .iter()
        .copied()
        .fold(f64::MAX, f64::min)
        .clamp(0.0, limit);
    let max = values
        .iter()
        .copied()
        .fold(f64::MIN, f64::max)
        .clamp(0.0, limit);
    (min, max)
}

/// Settings for a fast, low-resolution color preview of the whole flatbed.
pub fn preview_settings(caps: &Capabilities) -> Result<ScanSettings, ScanSetupError> {
    let source = InputSource::Platen;
    let source_caps = source_caps(caps, source)?;
    let resolution = source_caps
        .min_resolution()
        .ok_or(ScanSetupError::NoResolution)?;
    let mut settings = base_settings(caps, source, source_caps, resolution)?;
    settings.region = Some(ScanRegion::full(
        source_caps.max_width,
        source_caps.max_height,
    ));
    if source_caps.supports_intent("Preview") {
        settings.intent = Some(Intent::Preview);
    }
    Ok(settings)
}

/// Settings for a final scan.
pub fn scan_settings(
    caps: &Capabilities,
    request: &ScanRequest,
) -> Result<ScanSettings, ScanSetupError> {
    let source_caps = source_caps(caps, request.source)?;
    let resolution = source_caps
        .nearest_resolution(request.resolution)
        .ok_or(ScanSetupError::NoResolution)?;
    let mut settings = base_settings(caps, request.source, source_caps, resolution)?;
    settings.region = Some(scan_region(source_caps, request.area));
    Ok(settings)
}

/// Checks that the source can scan right now, to fail early with a clear
/// message instead of an obscure HTTP error.
pub fn check_ready(client: &Client, source: InputSource) -> Result<(), ReadinessError> {
    if source != InputSource::Feeder {
        return Ok(());
    }
    match client.status()?.adf_state {
        Some(AdfState::Empty) => Err(ReadinessError::FeederEmpty),
        Some(AdfState::Jam) => Err(ReadinessError::FeederJam),
        Some(AdfState::DoorOpen) => Err(ReadinessError::FeederOpen),
        _ => Ok(()),
    }
}

/// The color mode to request from the scanner.
///
/// Color is always preferred, whatever the output mode: gray and black and
/// white are derived locally, exactly as on the color preview, so the
/// preview is faithful and every page can still change mode later. Scanners
/// convert to gray with their own formula, which would make the final page
/// differ from the preview. 1-bit is never requested: the threshold must
/// stay adjustable.
pub fn scanner_color_mode(source: &SourceCaps) -> Option<ColorMode> {
    [ColorMode::Rgb24, ColorMode::Grayscale8]
        .into_iter()
        .find(|m| source.supports_color_mode(*m))
}

fn source_caps(caps: &Capabilities, source: InputSource) -> Result<&SourceCaps, ScanSetupError> {
    caps.source(source)
        .ok_or(ScanSetupError::SourceUnavailable(source))
}

fn base_settings(
    caps: &Capabilities,
    source: InputSource,
    source_caps: &SourceCaps,
    resolution: u32,
) -> Result<ScanSettings, ScanSetupError> {
    // Devices that do not list formats are assumed to support JPEG.
    if !source_caps.document_formats.is_empty() && !source_caps.supports_format("image/jpeg") {
        return Err(ScanSetupError::NoJpeg);
    }
    let color_mode = scanner_color_mode(source_caps).ok_or(ScanSetupError::NoColorMode)?;
    let mut settings = ScanSettings::new(source, color_mode, resolution);
    if let Some(version) = &caps.version {
        settings.version.clone_from(version);
    }
    // The lowest compression gives the best JPEG quality, which matters for
    // clean thresholding.
    settings.compression_factor = caps.compression_factor.map(|range| range.min);
    Ok(settings)
}

fn scan_region(caps: &SourceCaps, area: ScanArea) -> ScanRegion {
    let (max_w, max_h) = (caps.max_width, caps.max_height);
    let (x, y, width, height) = match area {
        ScanArea::Full => (0, 0, max_w, max_h),
        ScanArea::Region(crop) => {
            let scale = |v: f32, max: u32| (v.clamp(0.0, 1.0) * max as f32).round() as u32;
            let x = scale(crop.left, max_w);
            let y = scale(crop.top, max_h);
            (
                x,
                y,
                scale(crop.right, max_w).saturating_sub(x),
                scale(crop.bottom, max_h).saturating_sub(y),
            )
        }
        ScanArea::Paper(paper) => {
            let (w, h) = paper.size_mm();
            (0, 0, mm_to_units(w), mm_to_units(h))
        }
    };
    // Keep the region inside the device limits.
    let x = x.min(max_w.saturating_sub(caps.min_width));
    let y = y.min(max_h.saturating_sub(caps.min_height));
    ScanRegion {
        x,
        y,
        width: width.clamp(caps.min_width, max_w - x),
        height: height.clamp(caps.min_height, max_h - y),
    }
}

fn mm_to_units(mm: f64) -> u32 {
    (mm / 25.4 * f64::from(UNITS_PER_INCH)).round() as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps() -> Capabilities {
        Capabilities::parse(include_str!("../tests/fixtures/scanner_capabilities.xml")).unwrap()
    }

    #[test]
    fn previews_whole_bed_in_color_at_lowest_resolution() {
        let settings = preview_settings(&caps()).unwrap();
        assert_eq!(settings.source, InputSource::Platen);
        assert_eq!(settings.color_mode, ColorMode::Rgb24);
        assert_eq!(settings.resolution, 75);
        assert_eq!(settings.region, Some(ScanRegion::full(2550, 3508)));
        assert_eq!(settings.intent, Some(Intent::Preview));
        assert_eq!(settings.compression_factor, Some(0));
        assert_eq!(settings.version, "2.63");
    }

    #[test]
    fn scans_regions_in_color() {
        let request = ScanRequest {
            source: InputSource::Platen,
            resolution: 300,
            area: ScanArea::Region(Crop {
                left: 0.1,
                top: 0.2,
                right: 0.5,
                bottom: 1.0,
            }),
        };
        let settings = scan_settings(&caps(), &request).unwrap();
        assert_eq!(settings.color_mode, ColorMode::Rgb24);
        assert_eq!(settings.intent, None);
        assert_eq!(
            settings.region,
            Some(ScanRegion {
                x: 255,
                y: 702,
                width: 1020,
                height: 2806
            })
        );
    }

    #[test]
    fn clamps_resolution_and_paper_to_the_feeder() {
        let request = ScanRequest {
            source: InputSource::Feeder,
            resolution: 1200,
            area: ScanArea::Paper(PaperSize::Legal),
        };
        let settings = scan_settings(&caps(), &request).unwrap();
        assert_eq!(settings.resolution, 600, "the feeder tops out at 600 dpi");
        assert_eq!(
            settings.region,
            Some(ScanRegion {
                x: 0,
                y: 0,
                width: 2550,
                height: 4200
            })
        );

        let a4 = ScanRequest {
            area: ScanArea::Paper(PaperSize::A4),
            ..request
        };
        assert_eq!(
            scan_settings(&caps(), &a4).unwrap().region,
            Some(ScanRegion {
                x: 0,
                y: 0,
                width: 2480,
                height: 3508
            })
        );
    }

    #[test]
    fn keeps_degenerate_regions_valid() {
        let source = caps().platen.unwrap();
        let corner = Crop {
            left: 1.0,
            top: 1.0,
            right: 1.0,
            bottom: 1.0,
        };
        let region = scan_region(&source, ScanArea::Region(corner));
        assert_eq!(
            region,
            ScanRegion {
                x: 2549,
                y: 3507,
                width: 1,
                height: 1
            }
        );
    }

    #[test]
    fn reports_missing_sources() {
        let mut caps = caps();
        caps.feeder = None;
        let request = ScanRequest {
            source: InputSource::Feeder,
            resolution: 300,
            area: ScanArea::Full,
        };
        assert_eq!(
            scan_settings(&caps, &request),
            Err(ScanSetupError::SourceUnavailable(InputSource::Feeder))
        );
    }

    fn assert_close(a: f32, b: f32, what: &str) {
        assert!((a - b).abs() < 1e-3, "{what}: {a} != {b}");
    }

    #[test]
    fn plans_plain_regions_without_page_crop() {
        let crop = Crop {
            left: 0.1,
            top: 0.2,
            right: 0.5,
            bottom: 0.9,
        };
        let plan = plan_region(crop, Rotation::Cw90, 0.0, 2550, 3508);
        assert_eq!(plan.region, crop.to_source(Rotation::Cw90));
        assert_eq!(plan.page_crop, Crop::FULL);
    }

    #[test]
    fn straightened_regions_round_trip() {
        let (bed_w, bed_h) = (2550.0, 3508.0);
        let crop = Crop {
            left: 0.2,
            top: 0.3,
            right: 0.7,
            bottom: 0.6,
        };
        for (rotation, straighten) in [
            (Rotation::None, 3.0f32),
            (Rotation::None, -4.5),
            (Rotation::Cw90, 2.0),
        ] {
            let plan = plan_region(crop, rotation, straighten, bed_w as u32, bed_h as u32);
            let (w, h) = if rotation.is_sideways() {
                (bed_h, bed_w)
            } else {
                (bed_w, bed_h)
            };
            let region = plan.region.from_source(rotation);
            let (page_w, page_h) = (region.width() * w, region.height() * h);

            // The page crop has the physical size of the chosen area.
            assert_close(plan.page_crop.width() * page_w, crop.width() * w, "width");
            assert_close(
                plan.page_crop.height() * page_h,
                crop.height() * h,
                "height",
            );

            // Its center maps back to the center of the chosen area.
            let angle = straighten.to_radians();
            let rotate = |(x, y): (f32, f32), a: f32, (cx, cy): (f32, f32)| {
                let (dx, dy) = (x - cx, y - cy);
                (
                    dx * a.cos() - dy * a.sin() + cx,
                    dx * a.sin() + dy * a.cos() + cy,
                )
            };
            let page_origin = (region.left * w, region.top * h);
            let page_center = (page_origin.0 + page_w / 2.0, page_origin.1 + page_h / 2.0);
            let center_on_page = (
                page_origin.0 + (plan.page_crop.left + plan.page_crop.right) / 2.0 * page_w,
                page_origin.1 + (plan.page_crop.top + plan.page_crop.bottom) / 2.0 * page_h,
            );
            let on_bed = rotate(center_on_page, -angle, page_center);
            let shown = rotate(on_bed, angle, (w / 2.0, h / 2.0));
            assert_close(shown.0 / w, (crop.left + crop.right) / 2.0, "center x");
            assert_close(shown.1 / h, (crop.top + crop.bottom) / 2.0, "center y");
        }
    }
}
