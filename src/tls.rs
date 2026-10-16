//! Wisp's local certificate authority and the per-domain certificates it issues.
//!
//! A single CA is generated once under `~/.config/wisp/ca/`, and each project's
//! domain gets a leaf certificate whose SANs cover `<domain>`, `*.<domain>` and
//! localhost. Trusting the CA in the current user's root store needs no
//! administrator rights (Windows shows a single confirmation dialog).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::PathBuf;

use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose, SanType,
};

use crate::config;

#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    #[error("{0}")]
    Rcgen(#[from] rcgen::Error),

    #[error("{0}")]
    Io(#[from] std::io::Error),

    #[error("{0}")]
    Trust(String),
}

pub type Result<T> = std::result::Result<T, TlsError>;

/// The certificate and key nginx should serve for a domain.
#[derive(Clone, Debug)]
pub struct TlsPaths {
    pub cert: PathBuf,
    pub key: PathBuf,
}

impl TlsPaths {
    /// Both files exist, so the certificate is usable.
    pub fn is_ready(&self) -> bool {
        self.cert.is_file() && self.key.is_file()
    }
}

pub fn ca_cert() -> PathBuf {
    config::ca().join("wisp-ca.crt")
}

pub fn ca_key() -> PathBuf {
    config::ca().join("wisp-ca.key")
}

pub fn domain_dir(domain: &str) -> PathBuf {
    config::certs().join(domain)
}

pub fn paths_for(domain: &str) -> TlsPaths {
    let dir = domain_dir(domain);
    TlsPaths {
        cert: dir.join("cert.pem"),
        key: dir.join("key.pem"),
    }
}

/// Create the CA if it does not exist yet.
pub fn ensure_ca() -> Result<()> {
    if ca_cert().is_file() && ca_key().is_file() {
        return Ok(());
    }
    config::ensure();

    let mut params = CertificateParams::new(Vec::<String>::new())?;
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params
        .distinguished_name
        .push(DnType::CommonName, "Wisp Local Development CA");
    params
        .distinguished_name
        .push(DnType::OrganizationName, "Wisp");
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];

    let key = KeyPair::generate()?;
    let cert = params.self_signed(&key)?;

    std::fs::write(ca_cert(), cert.pem())?;
    std::fs::write(ca_key(), key.serialize_pem())?;
    Ok(())
}

/// Issue (or re-issue) the certificate for a domain, returning its paths.
pub fn issue(domain: &str) -> Result<TlsPaths> {
    ensure_ca()?;
    let paths = paths_for(domain);
    if paths.is_ready() {
        return Ok(paths);
    }
    std::fs::create_dir_all(domain_dir(domain))?;

    let ca_key = KeyPair::from_pem(&std::fs::read_to_string(ca_key())?)?;
    let ca_pem = std::fs::read_to_string(ca_cert())?;
    let issuer = Issuer::from_ca_cert_pem(&ca_pem, ca_key)?;

    let mut params =
        CertificateParams::new(vec![domain.to_string(), format!("*.{domain}")])?;
    params.distinguished_name.push(DnType::CommonName, domain);
    params.subject_alt_names.extend([
        SanType::DnsName("localhost".try_into()?),
        SanType::IpAddress(IpAddr::V4(Ipv4Addr::LOCALHOST)),
        SanType::IpAddress(IpAddr::V6(Ipv6Addr::LOCALHOST)),
    ]);
    params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyEncipherment,
    ];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];

    let key = KeyPair::generate()?;
    let cert = params.signed_by(&key, &issuer)?;

    std::fs::write(&paths.cert, cert.pem())?;
    std::fs::write(&paths.key, key.serialize_pem())?;
    Ok(paths)
}

/// Trust the CA in the current user's root store. Windows shows one dialog.
pub fn trust_ca() -> Result<()> {
    ensure_ca()?;
    let certutil = system32().join("certutil.exe");
    let status = std::process::Command::new(certutil)
        .arg("-user")
        .arg("-addstore")
        .arg("Root")
        .arg(ca_cert())
        .status()
        .map_err(|error| TlsError::Trust(format!("could not run certutil: {error}")))?;

    if status.success() {
        Ok(())
    } else {
        Err(TlsError::Trust(format!(
            "certutil exited with code {}",
            status.code().unwrap_or(-1)
        )))
    }
}

fn system32() -> PathBuf {
    std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
        .join("System32")
}

#[cfg(test)]
mod tests {
    use super::{ca_cert, paths_for, ensure_ca, issue};

    /// Exercises real certificate generation. Touches no certificate store.
    #[test]
    fn issues_a_ca_and_a_domain_certificate() {
        let _guard = crate::test_support::ENV_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join("wisp-tls-test");
        let _ = std::fs::remove_dir_all(&root);
        std::env::set_var("WISP_CONFIG_DIR", &root);

        ensure_ca().expect("the CA should be created");
        assert!(ca_cert().is_file());
        assert!(std::fs::read_to_string(ca_cert())
            .unwrap()
            .contains("BEGIN CERTIFICATE"));

        let paths = issue("demo.test").expect("a certificate should be issued");
        assert!(paths.cert.is_file() && paths.key.is_file());
        assert!(std::fs::read_to_string(&paths.cert)
            .unwrap()
            .contains("BEGIN CERTIFICATE"));
        assert!(std::fs::read_to_string(&paths.key)
            .unwrap()
            .contains("PRIVATE KEY"));

        // Re-issuing returns the same, already-present paths.
        let again = issue("demo.test").unwrap();
        assert_eq!(again.cert, paths_for("demo.test").cert);

        let _ = std::fs::remove_dir_all(&root);
    }
}
