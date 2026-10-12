//! Blocking HTTP client for one eSCL service.
//!
//! All calls block the calling thread; the UI runs them on worker threads.

use super::caps::{Capabilities, CapsError};
use super::endpoint::Endpoint;
use super::settings::ScanSettings;
use super::status::{Status, StatusError};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;
use ureq::Agent;
use ureq::http::{Response, StatusCode};
use ureq::tls::TlsConfig;

/// Timeout for small requests (capabilities, status, job creation).
const SHORT_TIMEOUT: Duration = Duration::from_secs(15);
/// Time allowed for the scanner to start sending a document. High
/// resolutions on slow devices can take minutes.
const DOCUMENT_TIMEOUT: Duration = Duration::from_secs(600);
/// How long to keep retrying while the scanner reports itself busy.
const BUSY_RETRIES: u32 = 30;
const BUSY_RETRY_DELAY: Duration = Duration::from_secs(1);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("cannot reach the scanner: {0}")]
    Transport(#[from] ureq::Error),
    #[error("the scanner is busy")]
    Busy,
    #[error("the scanner rejected the request ({status}): {url}")]
    Http { status: u16, url: String },
    #[error("the scanner rejected the scan settings (409 Conflict)")]
    Conflict,
    #[error("the scanner did not return a job location")]
    MissingLocation,
    #[error("the scan was cancelled")]
    Cancelled,
    #[error(transparent)]
    Capabilities(#[from] CapsError),
    #[error(transparent)]
    Status(#[from] StatusError),
}

/// A connection to one eSCL endpoint. Cheap to clone and usable from any thread.
#[derive(Clone)]
pub struct Client {
    agent: Agent,
    endpoint: Endpoint,
}

impl Client {
    pub fn new(endpoint: Endpoint) -> Self {
        let mut config = Agent::config_builder()
            .http_status_as_error(false)
            .timeout_connect(Some(Duration::from_secs(5)))
            .timeout_global(Some(SHORT_TIMEOUT))
            .user_agent(concat!("ScanClog/", env!("CARGO_PKG_VERSION")));
        if endpoint.tls {
            // Network scanners almost always use self-signed certificates.
            config = config.tls_config(TlsConfig::builder().disable_verification(true).build());
        }
        Client {
            agent: config.build().into(),
            endpoint,
        }
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    fn url(&self, resource: &str) -> String {
        format!("{}/{resource}", self.endpoint.base_url())
    }

    pub fn capabilities(&self) -> Result<Capabilities, Error> {
        let body = self.get_text(&self.url("ScannerCapabilities"))?;
        Ok(Capabilities::parse(&body)?)
    }

    pub fn status(&self) -> Result<Status, Error> {
        let body = self.get_text(&self.url("ScannerStatus"))?;
        Ok(Status::parse(&body)?)
    }

    /// Submits a scan job. Documents are then fetched with [`Job::next_document`].
    pub fn create_job(&self, settings: &ScanSettings) -> Result<Job, Error> {
        let url = self.url("ScanJobs");
        let mut attempt = 0;
        let response = loop {
            let response = self
                .agent
                .post(&url)
                .header("Content-Type", "text/xml")
                .send(settings.to_xml())?;
            match response.status() {
                StatusCode::SERVICE_UNAVAILABLE if attempt < BUSY_RETRIES => {
                    attempt += 1;
                    thread::sleep(BUSY_RETRY_DELAY);
                }
                StatusCode::SERVICE_UNAVAILABLE => return Err(Error::Busy),
                StatusCode::CONFLICT => return Err(Error::Conflict),
                status if status.is_success() => break response,
                status => {
                    return Err(Error::Http {
                        status: status.as_u16(),
                        url,
                    });
                }
            }
        };
        let location = response
            .headers()
            .get("Location")
            .and_then(|v| v.to_str().ok())
            .ok_or(Error::MissingLocation)?;
        Ok(Job {
            client: self.clone(),
            url: self.resolve_location(location),
            cancelled: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Job locations are often absolute URLs built from the device's own host
    /// name, which may not resolve here. Keep only the path and reuse the
    /// address we already know works.
    fn resolve_location(&self, location: &str) -> String {
        let location = location.trim();
        let path = match location.split_once("://") {
            Some((_, rest)) => rest.find('/').map_or("/", |i| &rest[i..]),
            None => location,
        };
        let path = path.trim_end_matches('/');
        if path.starts_with('/') {
            format!("{}{path}", self.endpoint.origin())
        } else {
            self.url(path)
        }
    }

    fn get_text(&self, url: &str) -> Result<String, Error> {
        let mut response = self.agent.get(url).call()?;
        check_status(&response, url)?;
        Ok(response.body_mut().read_to_string()?)
    }
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

/// One scanned page as returned by the device.
#[derive(Debug, Clone)]
pub struct Document {
    pub data: Vec<u8>,
    /// MIME type reported by the scanner, e.g. `image/jpeg`.
    pub content_type: Option<String>,
}

/// A running scan job. Clones share cancellation state, so one thread can
/// cancel while another waits for a document.
#[derive(Debug, Clone)]
pub struct Job {
    client: Client,
    url: String,
    cancelled: Arc<AtomicBool>,
}

impl Job {
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Waits for the next page. Returns `Ok(None)` once the job has no more
    /// pages (the scanner answers 404).
    pub fn next_document(&self) -> Result<Option<Document>, Error> {
        match self.fetch_next_document() {
            // Cancelling makes the scanner drop the transfer, which shows up
            // here as an I/O or HTTP error.
            Err(_) | Ok(Some(_)) if self.is_cancelled() => Err(Error::Cancelled),
            result => result,
        }
    }

    fn fetch_next_document(&self) -> Result<Option<Document>, Error> {
        let url = format!("{}/NextDocument", self.url);
        let mut attempt = 0;
        loop {
            if self.is_cancelled() {
                return Err(Error::Cancelled);
            }
            let mut response = self
                .client
                .agent
                .get(&url)
                .config()
                .timeout_global(None)
                .timeout_recv_response(Some(DOCUMENT_TIMEOUT))
                .timeout_recv_body(Some(DOCUMENT_TIMEOUT))
                .build()
                .call()?;
            match response.status() {
                StatusCode::NOT_FOUND | StatusCode::GONE => return Ok(None),
                // Some devices answer 503 while the scan head warms up.
                StatusCode::SERVICE_UNAVAILABLE if attempt < BUSY_RETRIES => {
                    attempt += 1;
                    thread::sleep(BUSY_RETRY_DELAY);
                }
                StatusCode::SERVICE_UNAVAILABLE => return Err(Error::Busy),
                status if status.is_success() => {
                    let content_type = response.body().mime_type().map(str::to_owned);
                    let data = response
                        .body_mut()
                        .with_config()
                        .limit(u64::MAX)
                        .read_to_vec()?;
                    return Ok(Some(Document { data, content_type }));
                }
                status => {
                    return Err(Error::Http {
                        status: status.as_u16(),
                        url,
                    });
                }
            }
        }
    }

    /// Asks the scanner to abort the job. A thread blocked in
    /// [`next_document`](Self::next_document) returns [`Error::Cancelled`]
    /// once the scanner closes the transfer.
    pub fn cancel(&self) -> Result<(), Error> {
        self.cancelled.store(true, Ordering::SeqCst);
        let response = self.client.agent.delete(&self.url).call()?;
        // The job may already be finished, which is not an error here.
        match response.status() {
            StatusCode::NOT_FOUND | StatusCode::GONE => Ok(()),
            _ => check_status(&response, &self.url),
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

fn check_status<B>(response: &Response<B>, url: &str) -> Result<(), Error> {
    match response.status() {
        status if status.is_success() => Ok(()),
        StatusCode::SERVICE_UNAVAILABLE => Err(Error::Busy),
        status => Err(Error::Http {
            status: status.as_u16(),
            url: url.to_owned(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_job_locations_against_known_origin() {
        let client = Client::new(Endpoint::new("192.0.2.10", 80, false, "eSCL"));
        let expected = "http://192.0.2.10:80/eSCL/ScanJobs/42";
        assert_eq!(
            client.resolve_location("http://scanner.local./eSCL/ScanJobs/42"),
            expected
        );
        assert_eq!(client.resolve_location("/eSCL/ScanJobs/42/"), expected);
        assert_eq!(client.resolve_location("ScanJobs/42"), expected);
    }
}
