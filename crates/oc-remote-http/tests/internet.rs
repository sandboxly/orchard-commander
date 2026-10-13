//! Against the real internet (ignored by default; run with `--ignored`):
//! the operating system's trust store, and Amazon's per-bucket regions with
//! public buckets (anonymous, so no credentials needed).

use std::io::Read;
use std::time::Duration;

use oc_remote_http::s3::{self, S3Options};
use oc_remote_http::tls::check_certificate;
use oc_vfs::{FileSystem, ListOptions};

#[test]
#[ignore = "needs the internet"]
fn the_system_trusts_public_servers() {
    let trusted = std::env::temp_dir().join("oc-remote-http-unused.json");
    let pin = check_certificate("s3.amazonaws.com", 443, &trusted, &|_| panic!("asked"), Duration::from_secs(20)).unwrap();
    assert_eq!(pin, None);
}

#[test]
#[ignore = "needs the internet"]
fn public_amazon_buckets_in_their_regions() {
    let options = S3Options { trusted_certificates: std::env::temp_dir().join("oc-remote-http-unused.json"), ..Default::default() };
    let remote = s3::connect(s3::AWS_HOST, &options, &|_| panic!("asked")).unwrap();
    // Copernicus' elevation data lives in eu-central-1; found out by asking us-east-1.
    let entries = remote.list_dir("/copernicus-dem-30m", ListOptions::default()).unwrap();
    assert!(!entries.is_empty());
    assert_eq!(remote.bucket_regions().get("copernicus-dem-30m").map(String::as_str), Some("eu-central-1"));
    let file = entries.iter().find(|e| !e.is_dir).map(|e| format!("/copernicus-dem-30m/{}", e.name));
    if let Some(file) = file {
        let mut head = vec![0; 100];
        let n = remote.open_read(&file).unwrap().read(&mut head).unwrap();
        assert!(n > 0);
    }
    remote.close();
}
