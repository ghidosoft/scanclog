//! User settings remembered between sessions, stored as TOML in the XDG
//! config directory (usually `~/.config/scanclog/config.toml`).

use crate::escl::InputSource;
use crate::imaging::{Adjustments, Crop};
use crate::scanning::PaperSize;
use crate::timing::ScanTiming;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const FILE_NAME: &str = "config.toml";

/// The scanner used last, so it can be preselected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedScanner {
    pub name: String,
    pub uuid: Option<String>,
    /// The endpoint URL, e.g. `http://192.0.2.10:80/eSCL`.
    pub endpoint: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub scanner: Option<SavedScanner>,
    /// Addresses added by hand, for networks where mDNS does not work.
    pub manual_scanners: Vec<String>,
    pub source: InputSource,
    pub resolution: u32,
    pub feeder_paper: PaperSize,
    /// Adjustments applied to new scans.
    pub template: Adjustments,
    pub last_directory: Option<PathBuf>,
    /// Quality for JPEG data that has to be re-encoded (1-100).
    pub jpeg_quality: u8,
    /// Learned scan speeds, for progress estimates.
    pub scan_timing: ScanTiming,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            scanner: None,
            manual_scanners: Vec::new(),
            source: InputSource::Platen,
            resolution: 300,
            feeder_paper: PaperSize::default(),
            template: Adjustments::default(),
            last_directory: None,
            jpeg_quality: 90,
            scan_timing: ScanTiming::default(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot determine the configuration directory")]
    NoDirectory,
    #[error("cannot access {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid configuration in {path}: {source}")]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[error("cannot serialize the configuration: {0}")]
    Serialize(#[from] toml::ser::Error),
}

impl Config {
    /// The default location of the configuration file.
    pub fn default_path() -> Option<PathBuf> {
        directories::ProjectDirs::from("", "", "scanclog")
            .map(|dirs| dirs.config_dir().join(FILE_NAME))
    }

    /// Loads the configuration, falling back to defaults when the file does
    /// not exist yet.
    pub fn load_from(path: &Path) -> Result<Self, ConfigError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
            Err(source) => {
                return Err(ConfigError::Io {
                    path: path.to_owned(),
                    source,
                });
            }
        };
        let mut config: Config = toml::from_str(&text).map_err(|source| ConfigError::Parse {
            path: path.to_owned(),
            source,
        })?;
        config.sanitize();
        Ok(config)
    }

    pub fn save_to(&self, path: &Path) -> Result<(), ConfigError> {
        let text = toml::to_string_pretty(self)?;
        let io_error = |source| ConfigError::Io {
            path: path.to_owned(),
            source,
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(io_error)?;
        }
        crate::export::write_atomically(path, text.as_bytes()).map_err(|e| match e {
            crate::export::ExportError::Io(source) => io_error(source),
            other => io_error(std::io::Error::other(other.to_string())),
        })
    }

    /// Loads from the default location, logging problems instead of failing:
    /// a broken config file should never prevent scanning.
    pub fn load() -> Self {
        let Some(path) = Self::default_path() else {
            log::warn!("{}", ConfigError::NoDirectory);
            return Config::default();
        };
        Self::load_from(&path).unwrap_or_else(|err| {
            log::warn!("{err}; using default settings");
            Config::default()
        })
    }

    pub fn save(&self) -> Result<(), ConfigError> {
        self.save_to(&Self::default_path().ok_or(ConfigError::NoDirectory)?)
    }

    /// Clamps values that may have been edited by hand.
    fn sanitize(&mut self) {
        self.jpeg_quality = self.jpeg_quality.clamp(1, 100);
        let t = &mut self.template;
        t.brightness = t.brightness.clamp(-1.0, 1.0);
        t.contrast = t.contrast.clamp(-1.0, 1.0);
        t.sharpen = t.sharpen.clamp(0.0, 2.0);
        // A crop only makes sense on the preview it was drawn on, which is
        // gone after a restart.
        t.crop = Crop::FULL;
        // Straightening is per page; new scans start level.
        t.straighten = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::imaging::{OutputMode, Threshold};

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!("scanclog-config-{}-{name}", std::process::id()))
            .join(FILE_NAME)
    }

    #[test]
    fn round_trips_and_resets_crop() {
        let path = temp_path("roundtrip");
        let mut config = Config {
            scanner: Some(SavedScanner {
                name: "Scanner".into(),
                uuid: Some("1234".into()),
                endpoint: "http://192.0.2.10:80/eSCL".into(),
            }),
            manual_scanners: vec!["192.0.2.20".into()],
            source: InputSource::Feeder,
            resolution: 600,
            feeder_paper: PaperSize::Legal,
            template: Adjustments {
                mode: OutputMode::BlackAndWhite,
                threshold: Threshold::Manual(140),
                crop: Crop {
                    left: 0.5,
                    ..Crop::FULL
                },
                ..Adjustments::default()
            },
            last_directory: Some("/tmp".into()),
            jpeg_quality: 85,
            scan_timing: ScanTiming::default(),
        };
        config.save_to(&path).unwrap();
        let loaded = Config::load_from(&path).unwrap();
        config.template.crop = Crop::FULL;
        assert_eq!(loaded, config);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn missing_file_gives_defaults() {
        assert_eq!(
            Config::load_from(&temp_path("missing")).unwrap(),
            Config::default()
        );
    }

    #[test]
    fn partial_files_are_completed_and_sanitized() {
        let config: Config = toml::from_str("resolution = 150\njpeg_quality = 0\n").unwrap();
        assert_eq!(config.resolution, 150);
        assert_eq!(config.source, InputSource::Platen);
        let mut config = config;
        config.sanitize();
        assert_eq!(config.jpeg_quality, 1);
        assert!(toml::from_str::<Config>("resolution = \"high\"").is_err());
    }
}
