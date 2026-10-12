//! The eSCL `ScanSettings` request document.

use super::caps::{ColorMode, InputSource};
use quick_xml::escape::escape;
use std::fmt::Write;

/// The eSCL protocol version sent with requests when the scanner's is unknown.
pub const DEFAULT_ESCL_VERSION: &str = "2.63";

/// A rectangle on the scanner bed, in 1/300 in, relative to the top-left corner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanRegion {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl ScanRegion {
    pub fn full(width: u32, height: u32) -> Self {
        ScanRegion {
            x: 0,
            y: 0,
            width,
            height,
        }
    }
}

/// Hints the scanner uses to tune its own image processing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intent {
    Document,
    TextAndGraphic,
    Photo,
    Preview,
}

impl Intent {
    pub fn as_escl(self) -> &'static str {
        match self {
            Intent::Document => "Document",
            Intent::TextAndGraphic => "TextAndGraphic",
            Intent::Photo => "Photo",
            Intent::Preview => "Preview",
        }
    }
}

/// Parameters of a scan job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanSettings {
    pub version: String,
    pub source: InputSource,
    pub color_mode: ColorMode,
    /// Resolution in dpi, applied to both axes.
    pub resolution: u32,
    pub region: Option<ScanRegion>,
    /// MIME type of the returned documents.
    pub document_format: String,
    pub intent: Option<Intent>,
    pub compression_factor: Option<i32>,
    pub sharpen: Option<i32>,
}

impl ScanSettings {
    pub fn new(source: InputSource, color_mode: ColorMode, resolution: u32) -> Self {
        ScanSettings {
            version: DEFAULT_ESCL_VERSION.to_owned(),
            source,
            color_mode,
            resolution,
            region: None,
            document_format: "image/jpeg".to_owned(),
            intent: None,
            compression_factor: None,
            sharpen: None,
        }
    }

    pub fn to_xml(&self) -> String {
        let mut xml = String::from(concat!(
            r#"<?xml version="1.0" encoding="UTF-8"?>"#,
            "\n",
            r#"<scan:ScanSettings xmlns:scan="http://schemas.hp.com/imaging/escl/2011/05/03" "#,
            r#"xmlns:pwg="http://www.pwg.org/schemas/2010/12/sm">"#,
            "\n"
        ));
        push_element(&mut xml, "pwg:Version", &self.version);
        if let Some(intent) = self.intent {
            push_element(&mut xml, "scan:Intent", intent.as_escl());
        }
        if let Some(r) = self.region {
            // Region fields are numbers, so no escaping is needed. Writing
            // to a String cannot fail.
            let _ = write!(
                xml,
                concat!(
                    "  <pwg:ScanRegions><pwg:ScanRegion>",
                    "<pwg:ContentRegionUnits>escl:ThreeHundredthsOfInches</pwg:ContentRegionUnits>",
                    "<pwg:XOffset>{}</pwg:XOffset><pwg:YOffset>{}</pwg:YOffset>",
                    "<pwg:Width>{}</pwg:Width><pwg:Height>{}</pwg:Height>",
                    "</pwg:ScanRegion></pwg:ScanRegions>\n"
                ),
                r.x, r.y, r.width, r.height
            );
        }
        push_element(&mut xml, "pwg:InputSource", self.source.as_escl());
        push_element(&mut xml, "scan:ColorMode", self.color_mode.as_escl());
        push_element(&mut xml, "scan:XResolution", &self.resolution.to_string());
        push_element(&mut xml, "scan:YResolution", &self.resolution.to_string());
        // Both spellings are sent: older devices read only pwg:DocumentFormat.
        push_element(&mut xml, "pwg:DocumentFormat", &self.document_format);
        push_element(&mut xml, "scan:DocumentFormatExt", &self.document_format);
        if let Some(value) = self.compression_factor {
            push_element(&mut xml, "scan:CompressionFactor", &value.to_string());
        }
        if let Some(value) = self.sharpen {
            push_element(&mut xml, "scan:Sharpen", &value.to_string());
        }
        xml.push_str("</scan:ScanSettings>\n");
        xml
    }
}

fn push_element(xml: &mut String, tag: &str, value: &str) {
    // Writing to a String cannot fail.
    let _ = writeln!(xml, "  <{tag}>{}</{tag}>", escape(value));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::escl::xml::Element;

    #[test]
    fn serializes_all_fields() {
        let mut settings = ScanSettings::new(InputSource::Platen, ColorMode::Grayscale8, 300);
        settings.region = Some(ScanRegion {
            x: 10,
            y: 20,
            width: 300,
            height: 400,
        });
        settings.intent = Some(Intent::Document);
        settings.compression_factor = Some(0);
        settings.sharpen = Some(1);

        let root = Element::parse(&settings.to_xml()).unwrap();
        assert_eq!(root.name, "ScanSettings");
        assert_eq!(root.child_text("Version"), Some(DEFAULT_ESCL_VERSION));
        assert_eq!(root.child_text("Intent"), Some("Document"));
        assert_eq!(root.child_text("InputSource"), Some("Platen"));
        assert_eq!(root.child_text("ColorMode"), Some("Grayscale8"));
        assert_eq!(root.child_text("XResolution"), Some("300"));
        assert_eq!(root.child_text("YResolution"), Some("300"));
        assert_eq!(root.child_text("DocumentFormatExt"), Some("image/jpeg"));
        assert_eq!(root.child_text("CompressionFactor"), Some("0"));
        assert_eq!(root.child_text("Sharpen"), Some("1"));
        let region = root.path(&["ScanRegions", "ScanRegion"]).unwrap();
        assert_eq!(
            region.child_text("ContentRegionUnits"),
            Some("escl:ThreeHundredthsOfInches")
        );
        assert_eq!(region.child_parse::<u32>("XOffset"), Some(10));
        assert_eq!(region.child_parse::<u32>("YOffset"), Some(20));
        assert_eq!(region.child_parse::<u32>("Width"), Some(300));
        assert_eq!(region.child_parse::<u32>("Height"), Some(400));
    }

    #[test]
    fn omits_optional_fields() {
        let xml = ScanSettings::new(InputSource::Feeder, ColorMode::Rgb24, 150).to_xml();
        let root = Element::parse(&xml).unwrap();
        assert_eq!(root.child_text("InputSource"), Some("Feeder"));
        for absent in ["Intent", "ScanRegions", "CompressionFactor", "Sharpen"] {
            assert!(root.child(absent).is_none(), "{absent} should be omitted");
        }
    }
}
