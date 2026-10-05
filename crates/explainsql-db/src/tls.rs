//! TLS the way libpq does it: `prefer` and `require` encrypt without
//! checking the server's certificate, `verify-ca` checks that a trusted
//! authority signed it, and `verify-full` also checks the host name.
//! Trusted authorities come from `sslrootcert` (by default
//! `~/.postgresql/root.crt`) or else the system's store.

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, ring};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};
use tokio_postgres_rustls::MakeRustlsConnect;

use crate::conn::{Settings, SslMode};

pub fn connector(settings: &Settings) -> Result<MakeRustlsConnect, String> {
    let provider = Arc::new(ring::default_provider());
    let builder = ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|error| error.to_string())?;
    let mode = settings.ssl_mode()?;
    let config = match mode {
        SslMode::Disable | SslMode::Prefer | SslMode::Require => builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(EncryptOnly(provider)))
            .with_no_client_auth(),
        SslMode::VerifyCa | SslMode::VerifyFull => {
            let roots = Arc::new(roots(settings)?);
            let webpki = WebPkiServerVerifier::builder_with_provider(roots, provider)
                .build()
                .map_err(|error| error.to_string())?;
            builder
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(Verify {
                    webpki,
                    check_name: mode == SslMode::VerifyFull,
                }))
                .with_no_client_auth()
        }
    };
    Ok(MakeRustlsConnect::new(config))
}

fn roots(settings: &Settings) -> Result<RootCertStore, String> {
    let mut store = RootCertStore::empty();
    let file = settings.get("sslrootcert").map(PathBuf::from).or_else(|| {
        let home = std::env::var_os(if cfg!(windows) { "APPDATA" } else { "HOME" })?;
        let path = PathBuf::from(home).join(if cfg!(windows) {
            "postgresql/root.crt"
        } else {
            ".postgresql/root.crt"
        });
        path.exists().then_some(path)
    });
    match file {
        Some(file) if file.as_os_str() == "system" => {}
        Some(file) => {
            for cert in CertificateDer::pem_file_iter(&file)
                .map_err(|error| format!("{}: {error}", file.display()))?
            {
                let cert = cert.map_err(|error| format!("{}: {error}", file.display()))?;
                store
                    .add(cert)
                    .map_err(|error| format!("{}: {error}", file.display()))?;
            }
            return Ok(store);
        }
        None => {}
    }
    let native = rustls_native_certs::load_native_certs();
    for cert in native.certs {
        let _ = store.add(cert);
    }
    if store.is_empty() {
        return Err("no trusted certificate authorities: set sslrootcert".to_owned());
    }
    Ok(store)
}

/// Accepts any certificate: encryption without authentication, as libpq's
/// `prefer` and `require`.
struct EncryptOnly(Arc<CryptoProvider>);

impl fmt::Debug for EncryptOnly {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("EncryptOnly")
    }
}

impl ServerCertVerifier for EncryptOnly {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// Checks the chain, and for `verify-full` the host name.
#[derive(Debug)]
struct Verify {
    webpki: Arc<WebPkiServerVerifier>,
    check_name: bool,
}

impl ServerCertVerifier for Verify {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        match self.webpki.verify_server_cert(
            end_entity,
            intermediates,
            server_name,
            ocsp_response,
            now,
        ) {
            Err(rustls::Error::InvalidCertificate(rustls::CertificateError::NotValidForName))
            | Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::NotValidForNameContext { .. },
            )) if !self.check_name => Ok(ServerCertVerified::assertion()),
            other => other,
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.webpki.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.webpki.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.webpki.supported_verify_schemes()
    }
}
