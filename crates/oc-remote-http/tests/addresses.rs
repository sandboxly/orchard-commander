//! Addresses and keychain account strings byte-for-byte as the Python version
//! computes them. `data/python_addresses.json` was produced by running 1.x's
//! `fsproviders.get(scheme).format_url / account / describe / validate /
//! parse_url` on these params and URLs.

use oc_provider::{FileSystemProvider, Params};
use oc_remote_http::{PlainS3Provider, PlainWebDavProvider, S3Provider, WebDavProvider};
use serde_json::Value;

fn provider(scheme: &str) -> Box<dyn FileSystemProvider> {
    match scheme {
        "webdavs" => Box::new(WebDavProvider),
        "webdav" => Box::new(PlainWebDavProvider),
        "s3" => Box::new(S3Provider),
        "s3+http" => Box::new(PlainS3Provider),
        other => panic!("unknown scheme {other}"),
    }
}

fn vectors() -> Value {
    let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/python_addresses.json")).unwrap();
    serde_json::from_str(&text).unwrap()
}

#[test]
fn urls_accounts_and_descriptions_match_python() {
    let v = vectors();
    let cases = v["format"].as_array().unwrap();
    assert!(cases.len() >= 10);
    for case in cases {
        let p = provider(case["scheme"].as_str().unwrap());
        let params: Params = case["params"].as_object().unwrap().clone();
        assert_eq!(p.format_url(&params), case["url"].as_str().unwrap(), "format_url {params:?}");
        assert_eq!(p.account(&params), case["account"].as_str().unwrap(), "account {params:?}");
        assert_eq!(p.describe(&params), case["describe"].as_str().unwrap(), "describe {params:?}");
        assert_eq!(p.validate(&params).as_deref(), case["validate"].as_str(), "validate {params:?}");
    }
}

#[test]
fn parsed_addresses_match_python() {
    let v = vectors();
    for case in v["parse"].as_array().unwrap() {
        let url = case["url"].as_str().unwrap();
        let scheme = url.split("://").next().unwrap().to_lowercase();
        let parsed = provider(&scheme).parse_url(url).unwrap();
        assert_eq!(Value::Object(parsed), case["params"], "parse_url {url}");
    }
}
