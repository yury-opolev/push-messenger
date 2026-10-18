//! The crate's tests against the stub issuer and TLS containers (Docker must be running).

mod azure;
mod redis_entra;
mod stub_tls;
mod tls;

#[test]
fn only_rejected_certificates_count_as_certificate_errors() {
    use rustls::{CertificateError, Error};
    let rejected = std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        Error::InvalidCertificate(CertificateError::UnknownIssuer),
    );
    assert!(crate::is_certificate_error(&rejected));
    let reset = std::io::Error::new(
        std::io::ErrorKind::ConnectionReset,
        "reset during the handshake",
    );
    assert!(!crate::is_certificate_error(&reset));
}
