//! Exercises the eSCL client against a scripted in-process HTTP server.

use scanclog::escl::{Client, ColorMode, Endpoint, Error, InputSource, ScanSettings};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;

/// A canned response: status code, extra headers, body.
type Reply = (u16, Vec<(&'static str, String)>, Vec<u8>);

/// Serves scripted replies in order and records the requests it received.
struct FakeScanner {
    endpoint: Endpoint,
    requests: Arc<Mutex<Vec<String>>>,
}

impl FakeScanner {
    fn start(replies: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&requests);
        thread::spawn(move || {
            for reply in replies {
                let (stream, _) = listener.accept().unwrap();
                let request = serve(stream, reply);
                log.lock().unwrap().push(request);
            }
        });
        FakeScanner {
            endpoint: Endpoint::new("127.0.0.1", port, false, "eSCL"),
            requests,
        }
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

/// Reads one request and answers it; returns "METHOD /path".
fn serve(stream: TcpStream, (status, headers, body): Reply) -> String {
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    reader.read_line(&mut request_line).unwrap();
    let mut content_length = 0;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        if line.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            content_length = value.trim().parse().unwrap();
        }
    }
    let mut request_body = vec![0; content_length];
    reader.read_exact(&mut request_body).unwrap();

    let mut response = format!(
        "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (name, value) in headers {
        response.push_str(&format!("{name}: {value}\r\n"));
    }
    response.push_str("\r\n");
    let mut stream = reader.into_inner();
    stream.write_all(response.as_bytes()).unwrap();
    stream.write_all(&body).unwrap();

    let mut parts = request_line.split_whitespace();
    format!("{} {}", parts.next().unwrap(), parts.next().unwrap())
}

fn settings() -> ScanSettings {
    ScanSettings::new(InputSource::Feeder, ColorMode::Rgb24, 300)
}

#[test]
fn runs_a_multi_page_job() {
    let jpeg = |n: u8| vec![0xFF, 0xD8, n, 0xFF, 0xD9];
    let image = |n| {
        (
            200,
            vec![("Content-Type", "image/jpeg".to_owned())],
            jpeg(n),
        )
    };
    let scanner = FakeScanner::start(vec![
        // An absolute Location with a host name we cannot resolve.
        (
            201,
            vec![(
                "Location",
                "http://scanner.invalid./eSCL/ScanJobs/7".to_owned(),
            )],
            vec![],
        ),
        (503, vec![], vec![]),
        image(1),
        image(2),
        (404, vec![], vec![]),
    ]);
    let client = Client::new(scanner.endpoint.clone());

    let job = client.create_job(&settings()).unwrap();
    assert_eq!(
        job.url(),
        format!("{}/ScanJobs/7", scanner.endpoint.base_url())
    );
    let first = job.next_document().unwrap().unwrap();
    assert_eq!(first.data, jpeg(1));
    assert_eq!(first.content_type.as_deref(), Some("image/jpeg"));
    assert_eq!(job.next_document().unwrap().unwrap().data, jpeg(2));
    assert!(job.next_document().unwrap().is_none());

    assert_eq!(
        scanner.requests(),
        [
            "POST /eSCL/ScanJobs",
            "GET /eSCL/ScanJobs/7/NextDocument",
            "GET /eSCL/ScanJobs/7/NextDocument",
            "GET /eSCL/ScanJobs/7/NextDocument",
            "GET /eSCL/ScanJobs/7/NextDocument",
        ]
    );
}

#[test]
fn reports_rejected_jobs() {
    let scanner = FakeScanner::start(vec![
        (409, vec![], vec![]),
        (400, vec![], vec![]),
        (201, vec![], vec![]),
    ]);
    let client = Client::new(scanner.endpoint.clone());
    assert!(matches!(
        client.create_job(&settings()),
        Err(Error::Conflict)
    ));
    assert!(matches!(
        client.create_job(&settings()),
        Err(Error::Http { status: 400, .. })
    ));
    assert!(matches!(
        client.create_job(&settings()),
        Err(Error::MissingLocation)
    ));
}

#[test]
fn cancels_a_job() {
    let scanner = FakeScanner::start(vec![
        (
            201,
            vec![("Location", "/eSCL/ScanJobs/9".to_owned())],
            vec![],
        ),
        (200, vec![], vec![]),
    ]);
    let client = Client::new(scanner.endpoint.clone());
    let job = client.create_job(&settings()).unwrap();
    let waiting = job.clone();
    job.cancel().unwrap();
    assert!(matches!(waiting.next_document(), Err(Error::Cancelled)));
    assert_eq!(
        scanner.requests(),
        ["POST /eSCL/ScanJobs", "DELETE /eSCL/ScanJobs/9"]
    );
}

#[test]
fn reads_capabilities_and_status() {
    let caps = include_str!("fixtures/scanner_capabilities.xml");
    let status = include_str!("fixtures/scanner_status.xml");
    let scanner = FakeScanner::start(vec![
        (200, vec![], caps.as_bytes().to_vec()),
        (200, vec![], status.as_bytes().to_vec()),
    ]);
    let client = Client::new(scanner.endpoint.clone());
    assert_eq!(client.capabilities().unwrap().feeder_capacity, Some(35));
    assert!(client.status().unwrap().adf_state.is_some());
    assert_eq!(
        scanner.requests(),
        ["GET /eSCL/ScannerCapabilities", "GET /eSCL/ScannerStatus"]
    );
}
