//! Typed view of the eSCL `ScannerCapabilities` document.

use super::xml::{Element, XmlError};
use std::fmt;
use std::str::FromStr;

/// eSCL geometry is expressed in 1/300 of an inch.
pub const UNITS_PER_INCH: u32 = 300;

/// Resolutions offered when a scanner only advertises a continuous range.
const STANDARD_RESOLUTIONS: [u32; 9] = [75, 100, 150, 200, 300, 400, 600, 1200, 2400];

/// Where the paper is scanned from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum InputSource {
    /// The flatbed glass.
    Platen,
    /// The automatic document feeder (ADF), simplex.
    Feeder,
}

impl InputSource {
    /// The value used in eSCL `ScanSettings`.
    pub fn as_escl(self) -> &'static str {
        match self {
            InputSource::Platen => "Platen",
            InputSource::Feeder => "Feeder",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            InputSource::Platen => "Flatbed",
            InputSource::Feeder => "Document feeder",
        }
    }
}

/// Color modes defined by eSCL. Scanners usually support a subset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ColorMode {
    BlackAndWhite1,
    Grayscale8,
    Grayscale16,
    Rgb24,
    Rgb48,
}

impl ColorMode {
    pub fn as_escl(self) -> &'static str {
        match self {
            ColorMode::BlackAndWhite1 => "BlackAndWhite1",
            ColorMode::Grayscale8 => "Grayscale8",
            ColorMode::Grayscale16 => "Grayscale16",
            ColorMode::Rgb24 => "RGB24",
            ColorMode::Rgb48 => "RGB48",
        }
    }
}

impl FromStr for ColorMode {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, ()> {
        match s {
            "BlackAndWhite1" => Ok(ColorMode::BlackAndWhite1),
            "Grayscale8" => Ok(ColorMode::Grayscale8),
            "Grayscale16" => Ok(ColorMode::Grayscale16),
            "RGB24" => Ok(ColorMode::Rgb24),
            "RGB48" => Ok(ColorMode::Rgb48),
            _ => Err(()),
        }
    }
}

impl fmt::Display for ColorMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_escl())
    }
}

/// An integer setting range such as `SharpenSupport`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ValueRange {
    pub min: i32,
    pub max: i32,
    pub normal: i32,
    pub step: i32,
}

/// Capabilities of one input source (platen or feeder).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceCaps {
    /// Smallest scannable area, in 1/300 in.
    pub min_width: u32,
    pub min_height: u32,
    /// Largest scannable area, in 1/300 in.
    pub max_width: u32,
    pub max_height: u32,
    pub max_scan_regions: u32,
    pub color_modes: Vec<ColorMode>,
    pub default_color_mode: Option<ColorMode>,
    /// MIME types, e.g. `image/jpeg`.
    pub document_formats: Vec<String>,
    /// Resolutions in dpi with equal X and Y, ascending.
    pub resolutions: Vec<u32>,
    pub default_resolution: Option<u32>,
    pub intents: Vec<String>,
    pub max_optical_resolution: Option<u32>,
}

impl SourceCaps {
    pub fn supports_color_mode(&self, mode: ColorMode) -> bool {
        self.color_modes.contains(&mode)
    }

    pub fn supports_format(&self, mime: &str) -> bool {
        self.document_formats.iter().any(|f| f == mime)
    }

    pub fn supports_intent(&self, intent: &str) -> bool {
        self.intents.iter().any(|i| i == intent)
    }

    /// The lowest supported resolution, used for previews.
    pub fn min_resolution(&self) -> Option<u32> {
        self.resolutions.first().copied()
    }

    /// The supported resolution closest to `dpi`.
    pub fn nearest_resolution(&self, dpi: u32) -> Option<u32> {
        self.resolutions
            .iter()
            .copied()
            .min_by_key(|r| r.abs_diff(dpi))
    }
}

/// The parsed `ScannerCapabilities` document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    pub version: Option<String>,
    pub make_and_model: Option<String>,
    pub manufacturer: Option<String>,
    pub uuid: Option<String>,
    pub platen: Option<SourceCaps>,
    /// Simplex feeder capabilities.
    pub feeder: Option<SourceCaps>,
    /// Whether the feeder can scan both sides. Not used by ScanClog yet.
    pub duplex: bool,
    pub feeder_capacity: Option<u32>,
    pub sharpen: Option<ValueRange>,
    pub compression_factor: Option<ValueRange>,
}

#[derive(Debug, thiserror::Error)]
pub enum CapsError {
    #[error(transparent)]
    Xml(#[from] XmlError),
    #[error("expected a ScannerCapabilities document, got <{0}>")]
    UnexpectedRoot(String),
    #[error("the scanner advertises no usable input source")]
    NoSources,
}

impl Capabilities {
    pub fn parse(xml: &str) -> Result<Self, CapsError> {
        let root = Element::parse(xml)?;
        if root.name != "ScannerCapabilities" {
            return Err(CapsError::UnexpectedRoot(root.name));
        }

        let platen = root.path(&["Platen", "PlatenInputCaps"]).map(parse_source);
        let adf = root.child("Adf");
        let feeder = adf
            .and_then(|a| a.child("AdfSimplexInputCaps"))
            .map(parse_source);
        let duplex = adf.is_some_and(|a| a.child("AdfDuplexInputCaps").is_some());
        if platen.is_none() && feeder.is_none() {
            return Err(CapsError::NoSources);
        }

        Ok(Capabilities {
            version: root.child_text("Version").map(str::to_owned),
            make_and_model: root.child_text("MakeAndModel").map(str::to_owned),
            manufacturer: root.child_text("Manufacturer").map(str::to_owned),
            uuid: root.child_text("UUID").map(str::to_owned),
            platen,
            feeder,
            duplex,
            feeder_capacity: adf.and_then(|a| a.child_parse("FeederCapacity")),
            sharpen: root.child("SharpenSupport").and_then(parse_range),
            compression_factor: root.child("CompressionFactorSupport").and_then(parse_range),
        })
    }

    pub fn source(&self, source: InputSource) -> Option<&SourceCaps> {
        match source {
            InputSource::Platen => self.platen.as_ref(),
            InputSource::Feeder => self.feeder.as_ref(),
        }
    }

    /// The available input sources, flatbed first.
    pub fn sources(&self) -> Vec<InputSource> {
        [InputSource::Platen, InputSource::Feeder]
            .into_iter()
            .filter(|s| self.source(*s).is_some())
            .collect()
    }
}

fn parse_source(caps: &Element) -> SourceCaps {
    let mut source = SourceCaps {
        min_width: caps.child_parse("MinWidth").unwrap_or(1),
        min_height: caps.child_parse("MinHeight").unwrap_or(1),
        max_width: caps.child_parse("MaxWidth").unwrap_or(0),
        max_height: caps.child_parse("MaxHeight").unwrap_or(0),
        max_scan_regions: caps.child_parse("MaxScanRegions").unwrap_or(1),
        color_modes: Vec::new(),
        default_color_mode: None,
        document_formats: Vec::new(),
        resolutions: Vec::new(),
        default_resolution: None,
        intents: caps
            .child("SupportedIntents")
            .map(|i| {
                i.children_named("Intent")
                    .map(|e| e.text.trim().to_owned())
                    .collect()
            })
            .unwrap_or_default(),
        max_optical_resolution: caps.child_parse("MaxOpticalXResolution"),
    };

    // A source may list several setting profiles; ScanClog treats their union
    // as available.
    let profiles = caps
        .child("SettingProfiles")
        .into_iter()
        .flat_map(|p| p.children_named("SettingProfile"));
    for profile in profiles {
        for mode in profile
            .child("ColorModes")
            .into_iter()
            .flat_map(|m| m.children_named("ColorMode"))
        {
            let Ok(parsed) = mode.text.trim().parse::<ColorMode>() else {
                continue;
            };
            push_unique(&mut source.color_modes, parsed);
            if is_default(mode) {
                source.default_color_mode = Some(parsed);
            }
        }

        if let Some(formats) = profile.child("DocumentFormats") {
            let entries = formats
                .children
                .iter()
                .filter(|f| f.name == "DocumentFormat" || f.name == "DocumentFormatExt");
            for format in entries {
                push_unique(&mut source.document_formats, format.text.trim().to_owned());
            }
        }

        let Some(supported) = profile.child("SupportedResolutions") else {
            continue;
        };
        let discrete = supported
            .child("DiscreteResolutions")
            .into_iter()
            .flat_map(|d| d.children_named("DiscreteResolution"));
        for res in discrete {
            let (Some(x), Some(y)) = (
                res.child_parse::<u32>("XResolution"),
                res.child_parse::<u32>("YResolution"),
            ) else {
                continue;
            };
            if x != y {
                continue;
            }
            push_unique(&mut source.resolutions, x);
            if res.child("XResolution").is_some_and(is_default) {
                source.default_resolution = Some(x);
            }
        }
        if let Some(range) = supported
            .path(&["ResolutionRange", "XResolutionRange"])
            .and_then(parse_range)
        {
            let (min, max) = (range.min.max(1) as u32, range.max.max(1) as u32);
            for dpi in STANDARD_RESOLUTIONS
                .into_iter()
                .filter(|d| (min..=max).contains(d))
            {
                push_unique(&mut source.resolutions, dpi);
            }
            source
                .default_resolution
                .get_or_insert(range.normal.max(1) as u32);
        }
    }
    source.resolutions.sort_unstable();
    source
}

fn parse_range(element: &Element) -> Option<ValueRange> {
    let min = element.child_parse("Min")?;
    let max = element.child_parse("Max")?;
    Some(ValueRange {
        min,
        max,
        normal: element.child_parse("Normal").unwrap_or(min),
        step: element.child_parse("Step").unwrap_or(1),
    })
}

fn is_default(element: &Element) -> bool {
    element.attribute("default") == Some("true")
}

fn push_unique<T: PartialEq>(items: &mut Vec<T>, item: T) {
    if !items.contains(&item) {
        items.push(item);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAPABILITIES: &str = include_str!("../../tests/fixtures/scanner_capabilities.xml");

    #[test]
    fn parses_a_real_device() {
        let caps = Capabilities::parse(CAPABILITIES).unwrap();
        assert_eq!(caps.make_and_model.as_deref(), Some("Example Inkjet MFP"));
        assert_eq!(caps.version.as_deref(), Some("2.63"));
        assert_eq!(
            caps.sources(),
            vec![InputSource::Platen, InputSource::Feeder]
        );
        assert!(!caps.duplex);
        assert_eq!(caps.feeder_capacity, Some(35));
        assert_eq!(
            caps.sharpen,
            Some(ValueRange {
                min: 0,
                max: 1,
                normal: 0,
                step: 1
            })
        );

        let platen = caps.platen.as_ref().unwrap();
        assert_eq!((platen.max_width, platen.max_height), (2550, 3508));
        assert_eq!(
            platen.color_modes,
            vec![ColorMode::Grayscale8, ColorMode::Rgb24]
        );
        assert_eq!(platen.default_color_mode, Some(ColorMode::Rgb24));
        assert_eq!(platen.resolutions, vec![75, 100, 150, 200, 300, 600, 1200]);
        assert_eq!(platen.default_resolution, Some(300));
        assert!(platen.supports_format("image/jpeg"));
        assert!(platen.supports_intent("Preview"));
        assert_eq!(platen.min_resolution(), Some(75));
        assert_eq!(platen.nearest_resolution(400), Some(300));

        let feeder = caps.feeder.as_ref().unwrap();
        assert_eq!(feeder.max_height, 4200);
        assert_eq!(feeder.resolutions.last(), Some(&600));
    }

    #[test]
    fn expands_resolution_ranges() {
        let xml = r#"<scan:ScannerCapabilities xmlns:scan="s" xmlns:pwg="p">
            <scan:Platen><scan:PlatenInputCaps>
              <scan:MaxWidth>2550</scan:MaxWidth><scan:MaxHeight>3508</scan:MaxHeight>
              <scan:SettingProfiles><scan:SettingProfile>
                <scan:ColorModes><scan:ColorMode>RGB24</scan:ColorMode><scan:ColorMode>Weird</scan:ColorMode></scan:ColorModes>
                <scan:SupportedResolutions><scan:ResolutionRange>
                  <scan:XResolutionRange><scan:Min>100</scan:Min><scan:Max>600</scan:Max><scan:Normal>300</scan:Normal><scan:Step>1</scan:Step></scan:XResolutionRange>
                </scan:ResolutionRange></scan:SupportedResolutions>
              </scan:SettingProfile></scan:SettingProfiles>
            </scan:PlatenInputCaps></scan:Platen>
          </scan:ScannerCapabilities>"#;
        let caps = Capabilities::parse(xml).unwrap();
        let platen = caps.platen.unwrap();
        assert_eq!(platen.resolutions, vec![100, 150, 200, 300, 400, 600]);
        assert_eq!(platen.default_resolution, Some(300));
        assert_eq!(platen.color_modes, vec![ColorMode::Rgb24]);
        assert!(caps.feeder.is_none());
    }

    #[test]
    fn rejects_other_documents() {
        assert!(matches!(
            Capabilities::parse("<ScannerStatus/>"),
            Err(CapsError::UnexpectedRoot(_))
        ));
        assert!(matches!(
            Capabilities::parse("<ScannerCapabilities/>"),
            Err(CapsError::NoSources)
        ));
    }
}
