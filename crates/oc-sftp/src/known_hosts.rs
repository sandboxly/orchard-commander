//! Trusted host keys in OpenSSH `known_hosts` format, read and written the
//! way paramiko (the Python 1.x version) does, so hosts trusted there are
//! still trusted after upgrading:
//!
//! - entries are looked up by `host` (port 22) or `[host]:port`, exactly
//!   (no wildcard patterns), plain or hashed (`|1|salt|hmac`);
//! - a new entry is the line `name keytype base64` (`ssh-rsa` for all RSA keys).

use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::Path;

use data_encoding::BASE64;
use hmac::{Hmac, KeyInit, Mac};
use russh::keys::{HashAlg, PublicKey};
use sha1::Sha1;

/// The name a host's keys are stored under: `host` for port 22, else `[host]:port`.
pub fn host_key_name(host: &str, port: u16) -> String {
    if port == 22 { host.to_string() } else { format!("[{host}]:{port}") }
}

/// "SHA256:…" as OpenSSH shows it (base64 of the key blob's SHA-256, no padding).
pub fn fingerprint(key: &PublicKey) -> String {
    key.fingerprint(HashAlg::Sha256).to_string()
}

/// The key type name as paramiko's `PKey.get_name()` (and the file) spell it.
pub fn key_type(key: &PublicKey) -> String {
    key.algorithm().as_str().to_string()
}

/// Keys recorded for `name` in the file at `path` (empty if none or unreadable).
pub fn lookup(path: &Path, name: &str) -> Vec<PublicKey> {
    let mut text = String::new();
    match fs::File::open(path).and_then(|mut f| f.read_to_string(&mut text)) {
        Ok(_) => lookup_in(&text, name),
        Err(_) => Vec::new(),
    }
}

/// [`lookup`] on the file's contents.
pub fn lookup_in(text: &str, name: &str) -> Vec<PublicKey> {
    let mut keys = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('@') {
            continue; // paramiko ignores markers (@cert-authority, @revoked) too
        }
        let mut fields = line.split_whitespace();
        let (Some(hosts), Some(_keytype), Some(blob)) = (fields.next(), fields.next(), fields.next()) else {
            continue;
        };
        if !hosts.split(',').any(|pattern| host_matches(pattern, name)) {
            continue;
        }
        // Unknown key types are skipped, as paramiko does.
        if let Ok(key) = russh::keys::parse_public_key_base64(blob) {
            keys.push(key);
        }
    }
    keys
}

fn host_matches(pattern: &str, name: &str) -> bool {
    let Some(hashed) = pattern.strip_prefix("|1|") else {
        return pattern == name;
    };
    let Some((salt, hash)) = hashed.split_once('|') else {
        return false;
    };
    let (Ok(salt), Ok(hash)) = (BASE64.decode(salt.as_bytes()), BASE64.decode(hash.as_bytes())) else {
        return false;
    };
    match Hmac::<Sha1>::new_from_slice(&salt) {
        Ok(mac) => mac.chain_update(name.as_bytes()).verify_slice(&hash).is_ok(),
        Err(_) => false,
    }
}

/// One line as paramiko's `HostKeyEntry.to_line()` writes it.
pub fn entry_line(name: &str, key: &PublicKey) -> io::Result<String> {
    let openssh = key.to_openssh().map_err(|e| io::Error::other(e.to_string()))?;
    let blob = openssh.split_whitespace().nth(1).unwrap_or_default();
    Ok(format!("{name} {} {blob}\n", key_type(key)))
}

/// Remember `key` for `name`: appended, so whatever else is in the file stays.
pub fn append(path: &Path, name: &str, key: &PublicKey) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let line = entry_line(name, key)?;
    let needs_newline = fs::read(path).map(|d| !d.is_empty() && !d.ends_with(b"\n")).unwrap_or(false);
    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    if needs_newline {
        f.write_all(b"\n")?;
    }
    f.write_all(line.as_bytes())?;
    f.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    // An Ed25519 key line as paramiko writes it, for a server on port 2222.
    const ED: &str = "AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl";

    #[test]
    fn plain_and_port_entries() {
        let text = format!("# comment\n[127.0.0.1]:2222 ssh-ed25519 {ED}\nother ssh-ed25519 {ED}\n");
        assert_eq!(lookup_in(&text, "[127.0.0.1]:2222").len(), 1);
        assert!(lookup_in(&text, "127.0.0.1").is_empty());
        assert_eq!(host_key_name("h", 22), "h");
        assert_eq!(host_key_name("h", 2222), "[h]:2222");
    }

    #[test]
    fn hashed_entries() {
        // `ssh-keygen -H` style: |1|base64(salt)|base64(HMAC-SHA1(salt, name))
        let salt = [7u8; 20];
        let mac = Hmac::<Sha1>::new_from_slice(&salt).unwrap().chain_update(b"example.com").finalize().into_bytes();
        let line = format!("|1|{}|{} ssh-ed25519 {ED}\n", BASE64.encode(&salt), BASE64.encode(&mac));
        assert_eq!(lookup_in(&line, "example.com").len(), 1);
        assert!(lookup_in(&line, "example.org").is_empty());
    }

    #[test]
    fn written_like_paramiko() {
        let key = russh::keys::parse_public_key_base64(ED).unwrap();
        assert_eq!(entry_line("[h]:2222", &key).unwrap(), format!("[h]:2222 ssh-ed25519 {ED}\n"));
        assert!(fingerprint(&key).starts_with("SHA256:"));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("known_hosts");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "x ssh-ed25519 AAAA").unwrap(); // no trailing newline
        append(&path, "[h]:2222", &key).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), format!("x ssh-ed25519 AAAA\n[h]:2222 ssh-ed25519 {ED}\n"));
        assert_eq!(lookup(&path, "[h]:2222"), vec![key]);
    }
}
