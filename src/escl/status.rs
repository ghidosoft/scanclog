//! Typed view of the eSCL `ScannerStatus` document.

use super::xml::{Element, XmlError};

/// The overall device state (`pwg:State`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScannerState {
    Idle,
    Processing,
    Testing,
    Stopped,
    Down,
    Other(String),
}

/// The document feeder state (`scan:AdfState`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdfState {
    Loaded,
    Empty,
    Jam,
    DoorOpen,
    Processing,
    Other(String),
}

impl AdfState {
    fn parse(value: &str) -> Self {
        match value {
            "ScannerAdfLoaded" => AdfState::Loaded,
            "ScannerAdfEmpty" => AdfState::Empty,
            "ScannerAdfJam" => AdfState::Jam,
            "ScannerAdfDoorOpen" => AdfState::DoorOpen,
            "ScannerAdfProcessing" => AdfState::Processing,
            other => AdfState::Other(other.to_owned()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub state: ScannerState,
    /// Absent when the scanner has no feeder or does not report it.
    pub adf_state: Option<AdfState>,
}

#[derive(Debug, thiserror::Error)]
pub enum StatusError {
    #[error(transparent)]
    Xml(#[from] XmlError),
    #[error("expected a ScannerStatus document, got <{0}>")]
    UnexpectedRoot(String),
}

impl Status {
    pub fn parse(xml: &str) -> Result<Self, StatusError> {
        let root = Element::parse(xml)?;
        if root.name != "ScannerStatus" {
            return Err(StatusError::UnexpectedRoot(root.name));
        }
        let state = match root.child_text("State").unwrap_or_default() {
            "Idle" => ScannerState::Idle,
            "Processing" => ScannerState::Processing,
            "Testing" => ScannerState::Testing,
            "Stopped" => ScannerState::Stopped,
            "Down" => ScannerState::Down,
            other => ScannerState::Other(other.to_owned()),
        };
        let adf_state = root.child_text("AdfState").map(AdfState::parse);
        Ok(Status { state, adf_state })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_real_device() {
        let status =
            Status::parse(include_str!("../../tests/fixtures/scanner_status.xml")).unwrap();
        assert_eq!(status.state, ScannerState::Idle);
        assert_eq!(status.adf_state, Some(AdfState::Empty));
    }

    #[test]
    fn tolerates_missing_and_unknown_fields() {
        let status =
            Status::parse("<ScannerStatus><State>Warming</State></ScannerStatus>").unwrap();
        assert_eq!(status.state, ScannerState::Other("Warming".into()));
        assert_eq!(status.adf_state, None);
    }
}
