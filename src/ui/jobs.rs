//! Work that runs off the GTK main thread and reports back through channels.

use anyhow::Context;
use scanclog::escl::{self, Capabilities, Client, Job, ScanSettings, ScannerInfo};
use scanclog::imaging::Bitmap;
use scanclog::model::ScannedImage;
use scanclog::scanning;
use std::thread;
use std::time::Duration;

/// How long mDNS discovery listens for answers.
const DISCOVERY_TIME: Duration = Duration::from_secs(4);

/// Progress of a scan job.
pub enum JobEvent {
    /// The scanner accepted the job; it can now be cancelled.
    Started(Job),
    /// One page arrived. `decoded` is set when the job was asked to decode.
    Page {
        image: ScannedImage,
        decoded: Option<Bitmap>,
    },
    /// The job ended, with the number of pages received or an error.
    Finished(Result<usize, JobError>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobError {
    Cancelled,
    Failed(String),
}

/// Runs a scan job on a worker thread.
pub fn run_scan(
    client: Client,
    settings: ScanSettings,
    decode: bool,
) -> async_channel::Receiver<JobEvent> {
    let (sender, receiver) = async_channel::unbounded();
    thread::spawn(move || {
        let result = scan(&client, &settings, decode, &sender);
        let result = result.map_err(|err| {
            if matches!(
                err.downcast_ref::<escl::Error>(),
                Some(escl::Error::Cancelled)
            ) {
                JobError::Cancelled
            } else {
                JobError::Failed(format!("{err:#}"))
            }
        });
        // The receiver is gone only if the window was closed.
        let _ = sender.send_blocking(JobEvent::Finished(result));
    });
    receiver
}

fn scan(
    client: &Client,
    settings: &ScanSettings,
    decode: bool,
    sender: &async_channel::Sender<JobEvent>,
) -> anyhow::Result<usize> {
    scanning::check_ready(client, settings.source)?;
    let job = client
        .create_job(settings)
        .context("the scanner refused the job")?;
    let _ = sender.send_blocking(JobEvent::Started(job.clone()));
    let mut pages = 0;
    while let Some(document) = job.next_document()? {
        let image = ScannedImage::from_jpeg(document.data, settings.resolution)
            .context("the scanner sent an image that cannot be read")?;
        let decoded = if decode { Some(image.decode()?) } else { None };
        pages += 1;
        if sender
            .send_blocking(JobEvent::Page { image, decoded })
            .is_err()
        {
            // Nobody is listening any more: stop the scanner too.
            let _ = job.cancel();
            break;
        }
    }
    Ok(pages)
}

/// Asks the scanner to cancel, without blocking the caller.
pub fn cancel(job: Job) {
    thread::spawn(move || {
        if let Err(err) = job.cancel() {
            log::warn!("cancelling the scan job failed: {err}");
        }
    });
}

/// Asks the scanner to cancel and waits up to `timeout` for the request to
/// complete. Used when the application is about to exit.
pub fn cancel_and_wait(job: Job, timeout: Duration) {
    let (sender, receiver) = std::sync::mpsc::channel();
    thread::spawn(move || {
        if let Err(err) = job.cancel() {
            log::warn!("cancelling the scan job failed: {err}");
        }
        let _ = sender.send(());
    });
    if receiver.recv_timeout(timeout).is_err() {
        log::warn!("the scanner did not confirm the cancellation in time");
    }
}

/// Browses for scanners; each message is the full list found so far.
pub fn discover() -> async_channel::Receiver<Vec<ScannerInfo>> {
    let (sender, receiver) = async_channel::unbounded();
    thread::spawn(move || {
        let result = escl::discover(DISCOVERY_TIME, |scanners| {
            let _ = sender.send_blocking(scanners.to_vec());
        });
        if let Err(err) = result {
            log::warn!("{err}");
        }
    });
    receiver
}

/// Connects to the first working endpoint of a scanner.
pub async fn connect(info: ScannerInfo) -> Result<(Client, Capabilities), String> {
    let result = gtk::gio::spawn_blocking(move || {
        let mut last_error = None;
        for endpoint in info.endpoints {
            let client = Client::new(endpoint);
            match client.capabilities() {
                Ok(caps) => return Ok((client, caps)),
                Err(err) => {
                    log::info!("{}: {err}", client.endpoint());
                    last_error = Some(err);
                }
            }
        }
        Err(last_error.map_or_else(
            || "the scanner has no address".to_owned(),
            |e| e.to_string(),
        ))
    })
    .await;
    result.unwrap_or_else(|_| Err("the connection attempt crashed".to_owned()))
}

/// Runs `work` on a worker thread and awaits its result.
pub async fn background<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
    match gtk::gio::spawn_blocking(work).await {
        Ok(value) => value,
        Err(panic) => std::panic::resume_unwind(panic),
    }
}
