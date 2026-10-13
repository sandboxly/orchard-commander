//! The HTTP client's edge cases against a tiny in-process server: servers
//! that ignore Range, drop keep-alive connections, send chunked bodies or 100
//! Continue, stall (timeouts), and S3 answers moto doesn't give (rejected
//! signatures, errors inside "200 OK", buckets in another region).

mod support;

use std::io::{Read, Seek, SeekFrom};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use oc_remote_http::http::{Body, ClientOptions, Headers, HttpClient, Payload, RangeReader};
use oc_remote_http::{Error, S3FileSystem};
use oc_vfs::FileSystem;
use support::{FakeServer, Reply, response};

fn client(port: u16, timeout: Duration) -> HttpClient {
    HttpClient::new("127.0.0.1", port, ClientOptions { timeout, ..Default::default() })
}

fn data() -> Vec<u8> {
    (0..100_000u32).map(|i| (i % 253) as u8).collect()
}

fn range_reader(client: HttpClient, size: u64) -> RangeReader {
    let get = Box::new(move |headers: &Headers| client.request_stream("GET", "/file", headers, Body::None));
    RangeReader::new(get, Box::new(move || Ok(size)))
}

#[test]
fn ranges_ignored_by_the_server_are_read_up_to_the_position() {
    let server = FakeServer::start(|_| Reply::Send(response("200 OK", &[], &data())));
    let mut r = range_reader(client(server.port, Duration::from_secs(5)), data().len() as u64);
    r.seek(SeekFrom::Start(70_000)).unwrap();
    let mut buf = vec![0; 100];
    r.read_exact(&mut buf).unwrap();
    assert_eq!(buf, data()[70_000..70_100]);
    r.seek(SeekFrom::End(-3)).unwrap(); // a short hop forward: read on
    let mut tail = Vec::new();
    r.read_to_end(&mut tail).unwrap();
    assert_eq!(tail, data()[data().len() - 3..]);
    r.seek(SeekFrom::Start(5)).unwrap(); // backwards: a new request
    r.read_exact(&mut buf).unwrap();
    assert_eq!(buf, data()[5..105]);
    let ranges: Vec<Option<String>> = server.requests().iter().map(|q| q.header("Range").map(str::to_string)).collect();
    assert_eq!(ranges, [Some("bytes=70000-".to_string()), Some("bytes=5-".to_string())]);
}

#[test]
fn ranged_reads_use_206_and_416() {
    let server = FakeServer::start(|q| {
        let all = data();
        let start: usize = q.header("Range").and_then(|r| r.strip_prefix("bytes=")?.strip_suffix('-')?.parse().ok()).unwrap_or(0);
        if start >= all.len() {
            return Reply::Send(response("416 Range Not Satisfiable", &[], b""));
        }
        let range = format!("bytes {start}-{}/{}", all.len() - 1, all.len());
        Reply::Send(response("206 Partial Content", &[("Content-Range", &range)], &all[start..]))
    });
    let mut r = range_reader(client(server.port, Duration::from_secs(5)), 0); // size from Content-Range
    r.seek(SeekFrom::Start(10)).unwrap();
    let mut buf = [0; 4];
    r.read_exact(&mut buf).unwrap();
    assert_eq!(buf, data()[10..14]);
    r.seek(SeekFrom::End(-1)).unwrap();
    assert_eq!(r.read(&mut buf).unwrap(), 1);
    r.seek(SeekFrom::Start(200_000)).unwrap();
    assert_eq!(r.read(&mut buf).unwrap(), 0);
}

#[test]
fn dropped_keep_alive_connections_are_retried() {
    // The server closes every connection after one answer, without saying so.
    let server = FakeServer::start(|q| Reply::SendAndClose(response("200 OK", &[], q.path.as_bytes())));
    let c = client(server.port, Duration::from_secs(5));
    for path in ["/one", "/two", "/three"] {
        let (r, body) = c.request("GET", path, &Headers::new(), Body::None).unwrap();
        assert_eq!((r.status, body.as_slice()), (200, path.as_bytes()));
    }
    let connections: std::collections::HashSet<usize> = server.requests().iter().map(|q| q.connection).collect();
    assert_eq!(connections.len(), 3);
}

#[test]
fn a_pool_of_dropped_connections_costs_one_retry() {
    let server = FakeServer::start(|q| Reply::SendAndClose(response("200 OK", &[], q.path.as_bytes())));
    let c = client(server.port, Duration::from_secs(5));
    // Four connections at once, all back in the pool afterwards — and all closed by the server.
    let streams: Vec<Payload> = (0..4).map(|_| c.request_stream("GET", "/s", &Headers::new(), Body::None).unwrap().1).collect();
    for s in streams {
        assert_eq!(s.into_bytes().unwrap(), b"/s");
    }
    std::thread::sleep(Duration::from_millis(50));
    let (_, body) = c.request("GET", "/after", &Headers::new(), Body::None).unwrap();
    assert_eq!(body, b"/after");
}

#[test]
fn keep_alive_connections_are_reused() {
    let server = FakeServer::start(|_| Reply::Send(response("200 OK", &[], b"ok")));
    let c = client(server.port, Duration::from_secs(5));
    for _ in 0..5 {
        c.request("GET", "/", &Headers::new(), Body::None).unwrap();
    }
    assert!(server.requests().iter().all(|q| q.connection == server.requests()[0].connection));
    assert_eq!(server.requests()[4].index, 4);
    let first = &server.requests()[0];
    assert_eq!(first.header("User-Agent"), Some("OrchardCommander/2.0"));
    assert_eq!(first.header("Host"), Some(format!("127.0.0.1:{}", server.port).as_str()));
}

#[test]
fn chunked_bodies_continue_and_close_delimited_answers() {
    let server = FakeServer::start(|q| match q.path.as_str() {
        "/chunked" => Reply::Send(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5;ext=1\r\nhello\r\n6\r\n world\r\n0\r\nTrailer: x\r\n\r\n".to_vec(),
        ),
        "/continue" => {
            let mut raw = b"HTTP/1.1 100 Continue\r\n\r\n".to_vec();
            raw.extend(response("201 Created", &[], b"made"));
            Reply::Send(raw)
        }
        _ => Reply::SendAndClose(b"HTTP/1.0 200 OK\r\n\r\nuntil the end".to_vec()),
    });
    let c = client(server.port, Duration::from_secs(5));
    let (_, body) = c.request("GET", "/chunked", &Headers::new(), Body::None).unwrap();
    assert_eq!(body, b"hello world");
    let (r, body) = c.request("PUT", "/continue", &Headers::new(), Body::Bytes(b"abc")).unwrap();
    assert_eq!((r.status, body.as_slice()), (201, b"made".as_slice()));
    assert_eq!(server.requests()[1].body, b"abc");
    let (_, body) = c.request("GET", "/close", &Headers::new(), Body::None).unwrap();
    assert_eq!(body, b"until the end");
    // The chunked answer left the connection reusable.
    assert_eq!(server.requests()[1].connection, server.requests()[0].connection);
}

#[test]
fn streamed_uploads_send_their_length() {
    let server = FakeServer::start(|q| Reply::Send(response("201 Created", &[], q.body.len().to_string().as_bytes())));
    let c = client(server.port, Duration::from_secs(5));
    let payload: Vec<u8> = (0..700_000u32).map(|i| i as u8).collect();
    let mut cursor = std::io::Cursor::new(payload.clone());
    let (_, body) = c.request("PUT", "/up", &Headers::new(), Body::Stream(&mut cursor, payload.len() as u64)).unwrap();
    assert_eq!(body, b"700000");
    let q = &server.requests()[0];
    assert_eq!(q.header("Content-Length"), Some("700000"));
    assert_eq!(q.body, payload);
}

#[test]
fn stalled_servers_time_out() {
    let server = FakeServer::start(|_| Reply::Stall(Duration::from_secs(5)));
    let c = client(server.port, Duration::from_millis(300));
    let started = Instant::now();
    let err = c.request("GET", "/", &Headers::new(), Body::None).unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(3), "{:?}", started.elapsed());
    assert!(matches!(&err, Error::Connection(m) if m.ends_with("timed out")), "{err}");
    // Nothing listening: refused at once.
    let port = { std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port() };
    let err = client(port, Duration::from_secs(5)).request("GET", "/", &Headers::new(), Body::None).unwrap_err();
    assert!(matches!(&err, Error::Connection(m) if m.starts_with(&format!("Cannot connect to 127.0.0.1:{port}: "))), "{err}");
}

#[test]
fn closed_clients_refuse_requests() {
    let server = FakeServer::start(|_| Reply::Send(response("200 OK", &[], b"ok")));
    let c = client(server.port, Duration::from_secs(5));
    let (_, payload) = c.request_stream("GET", "/", &Headers::new(), Body::None).unwrap();
    assert!(matches!(payload, Payload::Stream(_)));
    c.close();
    let err = c.request("GET", "/", &Headers::new(), Body::None).unwrap_err();
    assert!(matches!(&err, Error::Connection(m) if m == "Connection to 127.0.0.1 was closed"), "{err}");
}

// -- S3 answers moto doesn't give --------------------------------------------------

fn s3_fs(port: u16, region: Option<&str>) -> S3FileSystem {
    S3FileSystem::new("127.0.0.1", port, false, region, Some("AK"), Some("SK"), None, Duration::from_secs(5))
}

#[test]
fn rejected_keys_ask_for_the_secret_again() {
    let server = FakeServer::start(|_| {
        Reply::Send(response("403 Forbidden", &[], b"<Error><Code>SignatureDoesNotMatch</Code><Message>Check your key</Message></Error>"))
    });
    let err = s3_fs(server.port, None).check_credentials().unwrap_err();
    assert!(matches!(&err, Error::AuthenticationRequired(m) if m.contains("Check your key")), "{err}");
    // Merely not allowed to list the buckets: fine.
    let server = FakeServer::start(|_| Reply::Send(response("403 Forbidden", &[], b"<Error><Code>AccessDenied</Code></Error>")));
    s3_fs(server.port, None).check_credentials().unwrap();
}

#[test]
fn errors_inside_200_answers_are_errors() {
    let server = FakeServer::start(|q| match (q.method.as_str(), q.path.as_str()) {
        ("HEAD", "/b/src.txt") => Reply::Send(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\n".to_vec()),
        ("HEAD", _) => Reply::Send(response("404 Not Found", &[], b"")),
        ("GET", _) => Reply::Send(response("200 OK", &[], b"<ListBucketResult><IsTruncated>false</IsTruncated></ListBucketResult>")),
        ("PUT", "/b/dst.txt") if q.header("x-amz-copy-source") == Some("b/src.txt") => Reply::Send(response(
            "200 OK",
            &[],
            b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Error><Code>InternalError</Code><Message>We encountered an internal error.</Message></Error>",
        )),
        _ => Reply::Send(response("500 Internal Server Error", &[], b"")),
    });
    let fs = s3_fs(server.port, None);
    let err = fs.rename("/b/src.txt", "/b/dst.txt").unwrap_err();
    let text = err.to_string();
    assert!(text.contains("Copying to s3+http://AK@127.0.0.1:") && text.contains("We encountered an internal error"), "{text}");
    assert!(!server.requests().iter().any(|q| q.method == "DELETE"), "the source is kept");
}

#[test]
fn buckets_in_another_region_are_asked_again() {
    let hits = Arc::new(AtomicUsize::new(0));
    let hits2 = hits.clone();
    let server = FakeServer::start(move |q| {
        hits2.fetch_add(1, Ordering::SeqCst);
        let auth = q.header("Authorization").unwrap_or("");
        if !auth.contains("/eu-west-1/s3/aws4_request") {
            return Reply::Send(response(
                "400 Bad Request",
                &[],
                b"<Error><Code>AuthorizationHeaderMalformed</Code><Region>eu-west-1</Region></Error>",
            ));
        }
        Reply::Send(response("200 OK", &[("Content-Length", "5")], b"hello"))
    });
    let fs = s3_fs(server.port, None);
    let mut data = Vec::new();
    fs.open_read("/b/k.txt").unwrap().read_to_end(&mut data).unwrap();
    assert_eq!(data, b"hello");
    assert_eq!(fs.bucket_regions().get("b").map(String::as_str), Some("eu-west-1"));
    assert_eq!(hits.load(Ordering::SeqCst), 2);
}
