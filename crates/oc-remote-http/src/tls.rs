//! Certificates: the operating system's trust store, and certificates the
//! user chose to trust by fingerprint (pinned). Port of the certificate half
//! of `httpclient.py`.
//!
//! Before the first HTTPS request, [`check_certificate`] does one handshake
//! with the platform verifier. If the system trusts the server, connections
//! use the system trust store ([`system_config`]). If not, the user is asked
//! once whether to trust the certificate (its SHA-256 fingerprint, like an SSH
//! host key); afterwards exactly that certificate is accepted
//! ([`pinned_config`]) and a different one is refused as a possible attack.

use std::collections::BTreeMap;
use std::io;
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{CertificateError, ClientConfig, ClientConnection, DigitallySignedStruct, SignatureScheme};
use sha2::{Digest, Sha256};

use crate::{Error, Result, describe_connect_error};

/// The question for `confirm_certificate`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CertificateQuestion {
    pub host: String,
    pub port: u16,
    /// "SHA256:AA:BB:…"
    pub fingerprint: String,
    /// Why the system doesn't trust it, e.g. "self-signed certificate".
    pub problem: String,
}

/// "SHA256:" + the colon-separated upper-case hex SHA-256 of the DER certificate.
pub fn fingerprint(der: &[u8]) -> String {
    let digest = Sha256::digest(der);
    let hex: Vec<String> = digest.iter().map(|b| format!("{b:02X}")).collect();
    format!("SHA256:{}", hex.join(":"))
}

fn provider() -> Arc<CryptoProvider> {
    static P: OnceLock<Arc<CryptoProvider>> = OnceLock::new();
    P.get_or_init(|| Arc::new(rustls::crypto::ring::default_provider())).clone()
}

fn config_with(verifier: Arc<dyn ServerCertVerifier>) -> Result<Arc<ClientConfig>> {
    let config = ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(|e| Error::Connection(format!("TLS setup failed: {e}")))?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    Ok(Arc::new(config))
}

fn platform_verifier() -> Result<Arc<dyn ServerCertVerifier>> {
    let verifier = rustls_platform_verifier::Verifier::new(provider())
        .map_err(|e| Error::Connection(format!("The system's certificate store can't be used: {e}")))?;
    Ok(Arc::new(verifier))
}

/// TLS checked against the operating system's trust store (shared, so TLS
/// sessions are resumed across connections).
pub fn system_config() -> Result<Arc<ClientConfig>> {
    static C: OnceLock<Arc<ClientConfig>> = OnceLock::new();
    if let Some(c) = C.get() {
        return Ok(c.clone());
    }
    let config = config_with(platform_verifier()?)?;
    Ok(C.get_or_init(|| config).clone())
}

/// TLS that accepts exactly the certificate with this fingerprint (for any
/// host name: the user trusted this very certificate).
pub fn pinned_config(pin: &str) -> Result<Arc<ClientConfig>> {
    config_with(Arc::new(PinnedVerifier { pin: pin.to_string(), provider: provider() }))
}

/// Returned (inside a rustls error) when a pinned server shows another certificate.
#[derive(Debug, thiserror::Error)]
#[error("the certificate doesn't match the trusted one")]
pub(crate) struct PinMismatch;

#[derive(Debug)]
struct PinnedVerifier {
    pin: String,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for PinnedVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        if fingerprint(end_entity) == self.pin {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::InvalidCertificate(CertificateError::Other(rustls::OtherError(Arc::new(PinMismatch)))))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

/// The platform verifier, remembering the certificate it was shown.
#[derive(Debug)]
struct ProbingVerifier {
    inner: Arc<dyn ServerCertVerifier>,
    seen: Mutex<Option<Vec<u8>>>,
}

impl ServerCertVerifier for ProbingVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        *self.seen.lock().unwrap_or_else(|e| e.into_inner()) = Some(end_entity.to_vec());
        self.inner.verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

/// The rustls error inside an I/O error from a TLS stream, if any.
pub(crate) fn tls_error(err: &io::Error) -> Option<&rustls::Error> {
    err.get_ref().and_then(|e| e.downcast_ref::<rustls::Error>())
}

/// Whether an I/O error is a pinned server showing another certificate.
pub(crate) fn is_pin_mismatch(err: &io::Error) -> bool {
    matches!(tls_error(err), Some(rustls::Error::InvalidCertificate(CertificateError::Other(o)))
        if o.0.downcast_ref::<PinMismatch>().is_some())
}

/// A TCP connection to the first address of `host` that answers in time.
pub(crate) fn tcp_connect(host: &str, port: u16, timeout: Duration) -> io::Result<TcpStream> {
    let mut last = None;
    for addr in (host, port).to_socket_addrs()? {
        match TcpStream::connect_timeout(&addr, timeout) {
            Ok(stream) => {
                stream.set_read_timeout(Some(timeout))?;
                stream.set_write_timeout(Some(timeout))?;
                stream.set_nodelay(true)?;
                return Ok(stream);
            }
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("no address for {host}"))))
}

pub(crate) fn server_name(host: &str) -> io::Result<ServerName<'static>> {
    ServerName::try_from(host.to_string())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("invalid server name {host}: {e}")))
}

/// Run a TLS handshake to completion on `sock`.
pub(crate) fn handshake(conn: &mut ClientConnection, sock: &mut TcpStream) -> io::Result<()> {
    while conn.is_handshaking() {
        conn.complete_io(sock)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// User-trusted certificates
// ---------------------------------------------------------------------------

/// Certificates the user chose to trust, by host and port: a small JSON file
/// (`trusted_certificates.json` in the config dir, shared with 1.x).
pub struct TrustedCertificates {
    pub path: PathBuf,
}

impl TrustedCertificates {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        TrustedCertificates { path: path.into() }
    }

    fn load(&self) -> BTreeMap<String, serde_json::Value> {
        std::fs::read(&self.path).ok().and_then(|data| serde_json::from_slice(&data).ok()).unwrap_or_default()
    }

    pub fn get(&self, host: &str, port: u16) -> Option<String> {
        self.load().get(&format!("{host}:{port}")).and_then(|v| v.as_str()).map(str::to_string)
    }

    pub fn add(&self, host: &str, port: u16, fingerprint: &str) -> io::Result<()> {
        let mut data = self.load();
        data.insert(format!("{host}:{port}"), serde_json::Value::String(fingerprint.to_string()));
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = serde_json::to_string_pretty(&data).map_err(io::Error::other)?;
        std::fs::write(&self.path, text)
    }
}

/// Before the first HTTPS request: None if the system trusts the server, else
/// the fingerprint of the certificate the user trusts (asked once, then
/// pinned). Fails with `CertificateChanged`, `CertificateRejected` or
/// `Connection`.
pub fn check_certificate(
    host: &str,
    port: u16,
    trusted_certificates: &Path,
    confirm_certificate: &dyn Fn(&CertificateQuestion) -> bool,
    timeout: Duration,
) -> Result<Option<String>> {
    let probe = Arc::new(ProbingVerifier { inner: platform_verifier()?, seen: Mutex::new(None) });
    let config = config_with(probe.clone())?;
    let connect_error = |e: io::Error| Error::Connection(describe_connect_error(host, port, &e));
    let mut sock = tcp_connect(host, port, timeout).map_err(connect_error)?;
    let mut conn = ClientConnection::new(config, server_name(host).map_err(connect_error)?)
        .map_err(|e| connect_error(io::Error::other(e)))?;
    let error = match handshake(&mut conn, &mut sock) {
        Ok(()) => return Ok(None),
        Err(e) => e,
    };
    let seen = probe.seen.lock().unwrap_or_else(|e| e.into_inner()).take();
    let (Some(rustls::Error::InvalidCertificate(problem)), Some(der)) = (tls_error(&error), seen) else {
        return Err(connect_error(error));
    };
    let fingerprint = fingerprint(&der);
    let store = TrustedCertificates::new(trusted_certificates);
    match store.get(host, port) {
        Some(known) if known == fingerprint => return Ok(Some(fingerprint)),
        Some(_) => {
            return Err(Error::CertificateChanged(format!(
                "The certificate of {host} has changed (now {fingerprint}). This could mean someone is \
                 intercepting the connection; refusing to connect."
            )));
        }
        None => {}
    }
    let question = CertificateQuestion {
        host: host.to_string(),
        port,
        fingerprint: fingerprint.clone(),
        problem: describe_problem(problem, &der),
    };
    if !confirm_certificate(&question) {
        return Err(Error::CertificateRejected(format!("The certificate of {host} was not trusted")));
    }
    store
        .add(host, port, &fingerprint)
        .map_err(|e| Error::Connection(format!("Can't remember the certificate in {}: {e}", trusted_certificates.display())))?;
    Ok(Some(fingerprint))
}

/// Why the system doesn't trust a certificate, in OpenSSL's words where possible.
fn describe_problem(problem: &CertificateError, der: &[u8]) -> String {
    if is_self_signed(der) {
        return "self-signed certificate".into();
    }
    match problem {
        CertificateError::UnknownIssuer => "unable to get local issuer certificate".into(),
        CertificateError::Expired | CertificateError::ExpiredContext { .. } => "certificate has expired".into(),
        CertificateError::NotValidYet | CertificateError::NotValidYetContext { .. } => {
            "certificate is not yet valid".into()
        }
        CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. } => {
            "the certificate is for another server name".into()
        }
        CertificateError::Revoked => "certificate revoked".into(),
        CertificateError::Other(other) => other.to_string(),
        other => format!("{other:?}"),
    }
}

/// Whether the certificate's issuer equals its subject (DER, compared raw).
fn is_self_signed(der: &[u8]) -> bool {
    fn tlv(data: &[u8]) -> Option<(u8, &[u8], &[u8])> {
        let (&tag, rest) = data.split_first()?;
        let (&first, rest) = rest.split_first()?;
        let (len, rest) = if first < 0x80 {
            (first as usize, rest)
        } else {
            let n = (first & 0x7f) as usize;
            if n == 0 || n > 4 || rest.len() < n {
                return None;
            }
            (rest[..n].iter().fold(0usize, |a, &b| (a << 8) | b as usize), &rest[n..])
        };
        (rest.len() >= len).then(|| (tag, &rest[..len], &rest[len..]))
    }
    let parse = || -> Option<bool> {
        let (_, cert, _) = tlv(der)?;
        let (_, tbs, _) = tlv(cert)?;
        let mut rest = tbs;
        let (tag, _, after) = tlv(rest)?;
        if tag == 0xa0 {
            rest = after; // explicit version
        }
        let (_, _, rest) = tlv(rest)?; // serial
        let (_, _, rest) = tlv(rest)?; // signature algorithm
        let (_, issuer, rest) = tlv(rest)?;
        let (_, _, rest) = tlv(rest)?; // validity
        let (_, subject, _) = tlv(rest)?;
        Some(issuer == subject)
    };
    parse().unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprints_look_like_python() {
        let fp = fingerprint(b"abc");
        assert!(fp.starts_with("SHA256:BA:78:16:BF:"));
        assert_eq!(fp.len(), "SHA256:".len() + 32 * 3 - 1);
    }

    #[test]
    fn trusted_certificates_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = TrustedCertificates::new(dir.path().join("sub").join("trusted.json"));
        assert_eq!(store.get("nas", 443), None);
        store.add("nas", 443, "SHA256:00").unwrap();
        store.add("nas", 8443, "SHA256:11").unwrap();
        assert_eq!(store.get("nas", 443).as_deref(), Some("SHA256:00"));
        let text = std::fs::read_to_string(&store.path).unwrap();
        assert_eq!(text, "{\n  \"nas:443\": \"SHA256:00\",\n  \"nas:8443\": \"SHA256:11\"\n}");
        std::fs::write(&store.path, "not json").unwrap();
        assert_eq!(store.get("nas", 443), None);
    }
}
