//! Platform TLS verifier configuration.
//!
//! Every outbound HTTPS client in this crate is built from
//! [`client_config`] so trust decisions follow the platform policy:
//!
//! - **macOS:** `rustls-platform-verifier` delegates chain building and
//!   trust evaluation to Security.framework, so Keychain roots, enterprise CAs,
//!   and explicit distrust markers all apply.
//! - **Windows:** `rustls-platform-verifier` delegates to CryptoAPI/CNG, so
//!   the machine/user certificate stores and their distrust entries apply.
//! - **Linux (and other Unix):** `rustls-native-certs` loads the system CA
//!   bundle and rustls' webpki verifier checks the chain, matching Go's
//!   `crypto/x509` system-root behavior.
//!
//! There is no insecure fallback and no bundled root store: a platform whose
//! roots cannot be loaded fails the client construction instead of silently
//! trusting a different set of roots. `webpki-roots` and `aws-lc-rs` are
//! deliberately absent (see the manifest test below).

use std::sync::Arc;

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
use rustls::RootCertStore;
use rustls::crypto::CryptoProvider;
use rustls::pki_types::CertificateDer;
use rustls::{ClientConfig, ConfigBuilder, WantsVerifier};

/// Backend label for Linux and other platforms that use the native CA bundle
/// with the rustls/webpki verifier.
pub const NATIVE_ROOTS_BACKEND: &str = "native-roots+webpki";
/// Backend label for macOS (Security.framework).
pub const PLATFORM_VERIFIER_MACOS_BACKEND: &str = "platform-verifier:security-framework";
/// Backend label for Windows (CryptoAPI/CNG).
pub const PLATFORM_VERIFIER_WINDOWS_BACKEND: &str = "platform-verifier:cryptoapi";

#[cfg(target_os = "macos")]
const BACKEND: &str = PLATFORM_VERIFIER_MACOS_BACKEND;
#[cfg(target_os = "windows")]
const BACKEND: &str = PLATFORM_VERIFIER_WINDOWS_BACKEND;
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const BACKEND: &str = NATIVE_ROOTS_BACKEND;

/// Human-readable name of the verifier this build uses. Recorded in the TLS
/// spike evidence so a probe result is attributable to one backend.
pub const fn backend_name() -> &'static str {
    BACKEND
}

/// A TLS client-configuration failure. Construction fails closed: callers never
/// get a client that skips verification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsError {
    message: String,
}

impl TlsError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for TlsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for TlsError {}

/// Build the shared client configuration with the platform trust policy.
pub fn client_config() -> Result<ClientConfig, TlsError> {
    client_config_with_extra_roots(&[])
}

/// Build the shared client configuration with additional trusted roots on top
/// of the platform policy.
///
/// Production callers use [`client_config`]; this variant exists for the TLS
/// probe and the trust tests, which need a temporary root without modifying the
/// machine's certificate store. It never removes or weakens platform trust.
pub fn client_config_with_extra_roots(
    extra_roots: &[CertificateDer<'static>],
) -> Result<ClientConfig, TlsError> {
    build_config(extra_roots)
}

fn ring_provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn base_builder(
    provider: Arc<CryptoProvider>,
) -> Result<ConfigBuilder<ClientConfig, WantsVerifier>, TlsError> {
    ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| TlsError::new(format!("tls: configure protocol versions: {e}")))
}

/// Shared post-processing: offer HTTP/2 like Go's transport does. reqwest uses
/// the negotiated ALPN to pick HTTP/2 when the server supports it, so the
/// preconfigured TLS config must advertise `h2` itself.
fn finish(mut config: ClientConfig) -> ClientConfig {
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    config
}

/// Linux and other non-platform-verifier targets: the system CA bundle loaded
/// by `rustls-native-certs`, verified by rustls' webpki verifier. This matches
/// Go's system-root behavior. No roots is a hard error, never an empty store.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn build_config(extra_roots: &[CertificateDer<'static>]) -> Result<ClientConfig, TlsError> {
    let mut roots = native_roots()?;
    for cert in extra_roots {
        roots
            .add(cert.clone())
            .map_err(|e| TlsError::new(format!("tls: add extra root certificate: {e}")))?;
    }
    Ok(finish(
        base_builder(ring_provider())?
            .with_root_certificates(roots)
            .with_no_client_auth(),
    ))
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn native_roots() -> Result<RootCertStore, TlsError> {
    let loaded = rustls_native_certs::load_native_certs();
    let mut roots = RootCertStore::empty();
    let mut usable = 0usize;
    for cert in loaded.certs {
        if roots.add(cert).is_ok() {
            usable += 1;
        }
    }
    if usable == 0 {
        let mut message = String::from("tls: no usable native root certificates");
        if !loaded.errors.is_empty() {
            let details: Vec<String> = loaded.errors.iter().map(|e| e.to_string()).collect();
            message.push_str(": ");
            message.push_str(&details.join("; "));
        }
        return Err(TlsError::new(message));
    }
    Ok(roots)
}

/// macOS and Windows: the OS verifier, so platform roots, enterprise CAs, and
/// explicit distrust markers all apply. Extra roots are *added* to platform
/// trust; they never replace it.
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn build_config(extra_roots: &[CertificateDer<'static>]) -> Result<ClientConfig, TlsError> {
    let provider = ring_provider();
    let verifier = if extra_roots.is_empty() {
        rustls_platform_verifier::Verifier::new(provider.clone())
    } else {
        rustls_platform_verifier::Verifier::new_with_extra_roots(
            extra_roots.iter().cloned(),
            provider.clone(),
        )
    }
    .map_err(|e| TlsError::new(format!("tls: create platform verifier: {e}")))?;
    Ok(finish(
        base_builder(provider)?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(verifier))
            .with_no_client_auth(),
    ))
}

#[cfg(test)]
pub(crate) mod test_support {
    //! Minimal blocking TLS server for the inline trust tests and the
    //! decision-client wiring tests. It speaks one HTTP/1.1 response so the
    //! same server exercises both the raw rustls client and reqwest.

    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::Arc;
    use std::time::Duration;

    use rcgen::{
        BasicConstraints, CertificateParams, CertifiedIssuer, IsCa, KeyPair, KeyUsagePurpose,
        date_time_ymd,
    };
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
    use rustls::{ClientConfig, ServerConfig};

    /// A generated chain plus the trust anchor the tests inject.
    ///
    /// Every key is generated in-process for the test only; nothing here is a
    /// committed key or a real credential.
    pub(crate) struct TestCert {
        /// Trust anchor the tests install as an extra root.
        pub root: CertificateDer<'static>,
        /// Chain served by the test server: leaf first, then intermediates.
        pub chain: Vec<CertificateDer<'static>>,
        /// Ephemeral, test-only PKCS#8 key for `chain[0]`.
        pub key: Vec<u8>,
    }

    fn ca_params(constraint: IsCa) -> CertificateParams {
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = constraint;
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        params
    }

    fn leaf_params(san: &str) -> CertificateParams {
        CertificateParams::new(vec![san.to_string()]).unwrap()
    }

    /// A leaf certificate signed by a temporary root that the tests inject.
    pub(crate) fn ca_signed(san: &str) -> TestCert {
        let ca = CertifiedIssuer::self_signed(
            ca_params(IsCa::Ca(BasicConstraints::Unconstrained)),
            KeyPair::generate().unwrap(),
        )
        .unwrap();
        let leaf_key = KeyPair::generate().unwrap();
        let leaf = leaf_params(san).signed_by(&leaf_key, &ca).unwrap();
        TestCert {
            root: ca.der().clone(),
            chain: vec![leaf.der().clone()],
            key: leaf_key.serialize_der(),
        }
    }

    /// A root-signed leaf whose validity ended in 2020. Fixed dates keep the
    /// case deterministic instead of depending on the wall clock.
    pub(crate) fn expired_ca_signed(san: &str) -> TestCert {
        let ca = CertifiedIssuer::self_signed(
            ca_params(IsCa::Ca(BasicConstraints::Unconstrained)),
            KeyPair::generate().unwrap(),
        )
        .unwrap();
        let leaf_key = KeyPair::generate().unwrap();
        let mut params = leaf_params(san);
        params.not_before = date_time_ymd(2020, 1, 1);
        params.not_after = date_time_ymd(2020, 1, 2);
        let leaf = params.signed_by(&leaf_key, &ca).unwrap();
        TestCert {
            root: ca.der().clone(),
            chain: vec![leaf.der().clone()],
            key: leaf_key.serialize_der(),
        }
    }

    /// A root -> intermediate -> leaf chain. `send_intermediate` controls
    /// whether the server includes the intermediate, so the tests can prove the
    /// intermediate is what makes the chain verifiable.
    pub(crate) fn intermediate_chain(san: &str, send_intermediate: bool) -> TestCert {
        let root = CertifiedIssuer::self_signed(
            ca_params(IsCa::Ca(BasicConstraints::Unconstrained)),
            KeyPair::generate().unwrap(),
        )
        .unwrap();
        let intermediate = CertifiedIssuer::signed_by(
            ca_params(IsCa::Ca(BasicConstraints::Constrained(0))),
            KeyPair::generate().unwrap(),
            &root,
        )
        .unwrap();
        let leaf_key = KeyPair::generate().unwrap();
        let leaf = leaf_params(san)
            .signed_by(&leaf_key, &intermediate)
            .unwrap();
        let mut chain = vec![leaf.der().clone()];
        if send_intermediate {
            chain.push(intermediate.der().clone());
        }
        TestCert {
            root: root.der().clone(),
            chain,
            key: leaf_key.serialize_der(),
        }
    }

    /// A self-signed leaf that no trust store knows.
    pub(crate) fn self_signed(san: &str) -> TestCert {
        let key = KeyPair::generate().unwrap();
        let cert = leaf_params(san).self_signed(&key).unwrap();
        TestCert {
            root: cert.der().clone(),
            chain: vec![cert.der().clone()],
            key: key.serialize_der(),
        }
    }

    /// Serve one HTTP/1.1 response over TLS on a loopback port.
    pub(crate) fn serve(cert: &TestCert) -> u16 {
        let config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                cert.chain.clone(),
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(cert.key.clone())),
            )
            .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let Ok((tcp, _)) = listener.accept() else {
                return;
            };
            let _ = tcp.set_read_timeout(Some(Duration::from_secs(10)));
            let _ = tcp.set_write_timeout(Some(Duration::from_secs(10)));
            let Ok(conn) = rustls::ServerConnection::new(Arc::new(config)) else {
                return;
            };
            let mut stream = rustls::StreamOwned::new(conn, tcp);
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while stream.read(&mut byte).unwrap_or(0) == 1 {
                head.push(byte[0]);
                if head.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
            let _ = stream.flush();
            stream.conn.send_close_notify();
            let _ = stream.flush();
        });
        port
    }

    /// Connect with a rustls client and read the response body.
    pub(crate) fn connect(
        config: &ClientConfig,
        port: u16,
        name: &str,
    ) -> std::io::Result<Vec<u8>> {
        let server_name = ServerName::try_from(name.to_string()).unwrap();
        let conn = rustls::ClientConnection::new(Arc::new(config.clone()), server_name).unwrap();
        let tcp = TcpStream::connect(("127.0.0.1", port))?;
        tcp.set_read_timeout(Some(Duration::from_secs(10)))?;
        tcp.set_write_timeout(Some(Duration::from_secs(10)))?;
        let mut stream = rustls::StreamOwned::new(conn, tcp);
        stream.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")?;
        let mut out = Vec::new();
        stream.read_to_end(&mut out)?;
        Ok(out)
    }

    /// Whether an error chain reports a certificate/handshake rejection rather
    /// than, say, a DNS or connection failure.
    pub(crate) fn mentions_certificate(error: &dyn std::error::Error) -> bool {
        let mut source = Some(error);
        while let Some(err) = source {
            let text = err.to_string().to_lowercase();
            if text.contains("certificate")
                || text.contains("unknownissuer")
                || text.contains("notvalidforname")
                || text.contains("handshake")
            {
                return true;
            }
            source = err.source();
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{
        ca_signed, connect, expired_ca_signed, intermediate_chain, mentions_certificate,
        self_signed, serve,
    };
    use super::*;

    #[test]
    fn backend_name_matches_the_platform() {
        #[cfg(target_os = "macos")]
        assert_eq!(backend_name(), PLATFORM_VERIFIER_MACOS_BACKEND);
        #[cfg(target_os = "windows")]
        assert_eq!(backend_name(), PLATFORM_VERIFIER_WINDOWS_BACKEND);
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        assert_eq!(backend_name(), NATIVE_ROOTS_BACKEND);
    }

    #[test]
    fn client_config_builds_with_a_verifier() {
        let config = client_config().expect("platform client config");
        // The verifier itself is exercised by the handshake tests below. Here,
        // pin the transport-facing part of the configuration: HTTP/2 is offered
        // (reqwest selects h2 from the negotiated ALPN), with HTTP/1.1 fallback.
        assert_eq!(
            config.alpn_protocols,
            vec![b"h2".to_vec(), b"http/1.1".to_vec()]
        );
    }

    #[test]
    fn invalid_extra_root_fails_closed() {
        let bogus = CertificateDer::from(vec![0xde, 0xad, 0xbe, 0xef]);
        let err = client_config_with_extra_roots(&[bogus])
            .expect_err("an invalid root must not be ignored");
        assert!(
            err.to_string().starts_with("tls:"),
            "unexpected error text: {err}"
        );
    }

    #[test]
    fn self_signed_certificate_is_rejected() {
        let cert = self_signed("localhost");
        let port = serve(&cert);
        let config = client_config().expect("platform client config");
        let err = connect(&config, port, "localhost").expect_err("self-signed cert accepted");
        assert!(
            mentions_certificate(&err),
            "expected a certificate rejection, got: {err}"
        );
    }

    #[test]
    fn temporary_root_certificate_is_accepted() {
        let cert = ca_signed("127.0.0.1");
        let port = serve(&cert);
        let config = client_config_with_extra_roots(std::slice::from_ref(&cert.root))
            .expect("config with temporary root");
        let body = connect(&config, port, "127.0.0.1").expect("temporary root rejected");
        assert!(
            body.ends_with(b"ok"),
            "unexpected response body: {:?}",
            String::from_utf8_lossy(&body)
        );
    }

    #[test]
    fn hostname_mismatch_is_rejected() {
        let cert = ca_signed("other.example");
        let port = serve(&cert);
        let config = client_config_with_extra_roots(std::slice::from_ref(&cert.root))
            .expect("config with temporary root");
        let err = connect(&config, port, "localhost").expect_err("hostname mismatch accepted");
        assert!(
            mentions_certificate(&err),
            "expected a certificate rejection, got: {err}"
        );
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    #[test]
    fn native_roots_are_loaded() {
        let roots = native_roots().expect("native roots");
        assert!(!roots.is_empty(), "no native root certificates were loaded");
    }

    #[test]
    fn expired_certificate_is_rejected() {
        let cert = expired_ca_signed("localhost");
        let port = serve(&cert);
        let config = client_config_with_extra_roots(std::slice::from_ref(&cert.root))
            .expect("config with temporary root");
        let err = connect(&config, port, "localhost").expect_err("expired certificate accepted");
        assert!(
            mentions_certificate(&err),
            "expected a certificate rejection, got: {err}"
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn intermediate_chain_is_accepted() {
        let cert = intermediate_chain("127.0.0.1", true);
        let port = serve(&cert);
        let config = client_config_with_extra_roots(std::slice::from_ref(&cert.root))
            .expect("config with temporary root");
        let body = connect(&config, port, "127.0.0.1").expect("intermediate chain rejected");
        assert!(
            body.ends_with(b"ok"),
            "unexpected response body: {:?}",
            String::from_utf8_lossy(&body)
        );
    }

    #[test]
    fn intermediate_chain_without_the_intermediate_is_rejected() {
        let cert = intermediate_chain("localhost", false);
        let port = serve(&cert);
        let config = client_config_with_extra_roots(std::slice::from_ref(&cert.root))
            .expect("config with temporary root");
        let err = connect(&config, port, "localhost")
            .expect_err("chain without the intermediate was accepted");
        assert!(
            mentions_certificate(&err),
            "expected a certificate rejection, got: {err}"
        );
    }

    #[test]
    fn manifest_uses_manual_roots_and_ring_without_bundled_roots() {
        let manifest: toml::Value =
            toml::from_str(include_str!("../../Cargo.toml")).expect("parse Cargo.toml");
        let dependencies = manifest["dependencies"]
            .as_table()
            .expect("dependencies table");

        let reqwest = &dependencies["reqwest"];
        assert_eq!(
            reqwest["default-features"].as_bool(),
            Some(false),
            "reqwest must not enable default features"
        );
        let reqwest_features: Vec<&str> = reqwest["features"]
            .as_array()
            .expect("reqwest features")
            .iter()
            .map(|v| v.as_str().expect("feature string"))
            .collect();
        assert!(
            reqwest_features.contains(&"rustls-tls-manual-roots"),
            "reqwest must use rustls-tls-manual-roots: {reqwest_features:?}"
        );
        for forbidden in [
            "rustls-tls-native-roots",
            "rustls-tls-webpki-roots",
            "native-tls",
        ] {
            assert!(
                !reqwest_features.contains(&forbidden),
                "reqwest must not enable {forbidden}"
            );
        }

        let rustls = &dependencies["rustls"];
        let rustls_features: Vec<&str> = rustls["features"]
            .as_array()
            .expect("rustls features")
            .iter()
            .map(|v| v.as_str().expect("feature string"))
            .collect();
        assert!(
            rustls_features.contains(&"ring"),
            "rustls must use the ring provider: {rustls_features:?}"
        );
        assert!(
            !rustls_features.contains(&"aws_lc_rs"),
            "rustls must not enable aws-lc-rs"
        );

        assert!(
            dependencies.contains_key("rustls-native-certs"),
            "Linux roots come from rustls-native-certs"
        );
        assert!(
            dependencies.contains_key("rustls-platform-verifier"),
            "macOS/Windows use the platform verifier"
        );
        for forbidden in ["webpki-roots", "aws-lc-rs", "openssl", "native-tls"] {
            assert!(
                !dependencies.contains_key(forbidden),
                "Cargo.toml must not depend on {forbidden}"
            );
        }
    }
}
