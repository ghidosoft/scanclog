//! A small eSCL (AirScan / Mopria scan) client.
//!
//! eSCL is an HTTP + XML protocol: the scanner advertises itself over mDNS,
//! describes what it can do in `ScannerCapabilities`, accepts jobs as
//! `ScanSettings` documents and returns each page from `NextDocument`.

mod caps;
mod client;
mod discovery;
mod endpoint;
mod settings;
mod status;
mod xml;

pub use caps::{
    Capabilities, CapsError, ColorMode, InputSource, SourceCaps, UNITS_PER_INCH, ValueRange,
};
pub use client::{Client, Document, Error, Job};
pub use discovery::{DiscoveryError, ScannerInfo, discover};
pub use endpoint::{DEFAULT_ROOT, Endpoint, EndpointParseError};
pub use settings::{DEFAULT_ESCL_VERSION, Intent, ScanRegion, ScanSettings};
pub use status::{AdfState, ScannerState, Status, StatusError};
