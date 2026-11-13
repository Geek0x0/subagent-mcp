//! Platform TLS trust integration tests (Task 28).
//!
//! The inline tests in `src/provider/tls.rs` pin the verifier wiring. This
//! binary is the per-platform release gate: it drives a real TLS handshake
//! against a loopback server for every trust case the plan names — enterprise
//! CA, distrusted root, intermediate chain, hostname mismatch, and an expired
//! certificate — through the production [`tls::client_config`] path.
//!
//! Every certificate and key is generated in-process with `rcgen` for this test
//! only; nothing here is a committed key or a real credential.
//!
//! Platform-specific setup is `#[cfg]`-gated:
//!
//! - **Linux:** the native bundle is the trust store. `SSL_CERT_FILE` points
//!   `rustls-native-certs` at a generated bundle, so the distrusted-root and
//!   enterprise-CA cases exercise the real system-root path without modifying
//!   the host.
//! - **macOS / Windows:** the OS store is the trust store. Those tests install
//!   the generated root into the platform store, prove acceptance and explicit
//!   distrust, and remove it again. They mutate machine/user trust state, so
//!   they run only when `SUBAGENT_MCP_PLATFORM_TRUST_TESTS=1` is set — the
//!   `platform-macos-*` and `platform-windows-*` CI jobs set it. Without the
//!   variable they print a skip line and return.
//!
//! All tests serialize on one mutex: the Linux cases change a process-wide
//! environment variable and the macOS/Windows cases change platform trust
//! state, and both affect every `client_config()` call in this process.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DnType, IsCa, KeyPair, KeyUsagePurpose,
    date_time_ymd,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
use rustls::{ClientConfig, ServerConfig};
use subagent_mcp::provider::tls;

static SERIAL: Mutex<()> = Mutex::new(());

/// Loopback SAN used by every local server. The client connects by IP, so the
/// name check is exercised against an IP SAN rather than DNS.
const LOCAL_SAN: &str = "127.0.0.1";

/// A generated chain plus the trust anchor and PEM form used by store tests.
struct TestCert {
    /// Trust anchor the tests install as an extra root or platform root.
    root: CertificateDer<'static>,
    /// PEM encoding of `root`, for the Linux `SSL_CERT_FILE` bundle and the
    /// macOS store install. Windows writes the DER form instead, so the field
    /// is compiled out there to keep `-D warnings` clean.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    root_pem: String,
    /// Chain served by the test server: leaf first, then intermediates.
    chain: Vec<CertificateDer<'static>>,
    /// Ephemeral, test-only PKCS#8 key for `chain[0]`.
    key: Vec<u8>,
    /// Common name of the anchor, used to remove it from a platform store.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    common_name: String,
}

fn lock() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

fn ca_params(constraint: IsCa, common_name: &str) -> CertificateParams {
    let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
    params.is_ca = constraint;
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    params
        .distinguished_name
        .push(DnType::CommonName, common_name);
    params
}

fn leaf_params(san: &str) -> CertificateParams {
    CertificateParams::new(vec![san.to_string()]).unwrap()
}

/// A leaf signed by a temporary root that the tests inject or install.
fn ca_signed(san: &str, common_name: &str) -> TestCert {
    let ca = CertifiedIssuer::self_signed(
        ca_params(IsCa::Ca(BasicConstraints::Unconstrained), common_name),
        KeyPair::generate().unwrap(),
    )
    .unwrap();
    let leaf_key = KeyPair::generate().unwrap();
    let leaf = leaf_params(san).signed_by(&leaf_key, &ca).unwrap();
    TestCert {
        root: ca.der().clone(),
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        root_pem: ca.pem(),
        chain: vec![leaf.der().clone()],
        key: leaf_key.serialize_der(),
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        common_name: common_name.to_string(),
    }
}

/// A root-signed leaf whose validity ended in 2020. Fixed dates keep the case
/// deterministic instead of depending on the wall clock.
fn expired_ca_signed(san: &str, common_name: &str) -> TestCert {
    let ca = CertifiedIssuer::self_signed(
        ca_params(IsCa::Ca(BasicConstraints::Unconstrained), common_name),
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
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        root_pem: ca.pem(),
        chain: vec![leaf.der().clone()],
        key: leaf_key.serialize_der(),
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        common_name: common_name.to_string(),
    }
}

/// A root -> intermediate -> leaf chain. `send_intermediate` controls whether
/// the server includes the intermediate, so the tests can prove the
/// intermediate is what makes the chain verifiable.
fn intermediate_chain(san: &str, send_intermediate: bool, common_name: &str) -> TestCert {
    let root = CertifiedIssuer::self_signed(
        ca_params(IsCa::Ca(BasicConstraints::Unconstrained), common_name),
        KeyPair::generate().unwrap(),
    )
    .unwrap();
    let intermediate = CertifiedIssuer::signed_by(
        ca_params(
            IsCa::Ca(BasicConstraints::Constrained(0)),
            &format!("{common_name} intermediate"),
        ),
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
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        root_pem: root.pem(),
        chain,
        key: leaf_key.serialize_der(),
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        common_name: common_name.to_string(),
    }
}

/// A self-signed leaf that no trust store knows.
fn self_signed(san: &str) -> TestCert {
    let key = KeyPair::generate().unwrap();
    let cert = leaf_params(san).self_signed(&key).unwrap();
    TestCert {
        root: cert.der().clone(),
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        root_pem: cert.pem(),
        chain: vec![cert.der().clone()],
        key: key.serialize_der(),
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        common_name: "self-signed".to_string(),
    }
}

/// Serve HTTP/1.1 responses over TLS on a loopback port. Every accepted
/// connection gets its own handler, so a test can drive a rejected handshake
/// and then a trusted one against the same server.
fn serve(cert: &TestCert) -> u16 {
    let config = Arc::new(
        ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                cert.chain.clone(),
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(cert.key.clone())),
            )
            .unwrap(),
    );
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for accepted in listener.incoming() {
            let Ok(tcp) = accepted else { continue };
            let config = config.clone();
            std::thread::spawn(move || {
                let _ = tcp.set_read_timeout(Some(Duration::from_secs(10)));
                let _ = tcp.set_write_timeout(Some(Duration::from_secs(10)));
                let Ok(conn) = rustls::ServerConnection::new(config) else {
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
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                );
                let _ = stream.flush();
                stream.conn.send_close_notify();
                let _ = stream.flush();
            });
        }
    });
    port
}

/// Connect with a rustls client and read the response body.
fn connect(config: &ClientConfig, port: u16, name: &str) -> std::io::Result<Vec<u8>> {
    let server_name = ServerName::try_from(name.to_string()).unwrap();
    let conn = rustls::ClientConnection::new(Arc::new(config.clone()), server_name).unwrap();
    let tcp = std::net::TcpStream::connect(("127.0.0.1", port))?;
    tcp.set_read_timeout(Some(Duration::from_secs(10)))?;
    tcp.set_write_timeout(Some(Duration::from_secs(10)))?;
    let mut stream = rustls::StreamOwned::new(conn, tcp);
    stream.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")?;
    let mut out = Vec::new();
    stream.read_to_end(&mut out)?;
    Ok(out)
}

/// Whether an error chain reports a certificate/handshake rejection rather than
/// a DNS or connection failure.
fn mentions_certificate(error: &dyn std::error::Error) -> bool {
    let mut source = Some(error);
    while let Some(err) = source {
        let text = err.to_string().to_lowercase();
        for needle in [
            "certificate",
            "unknownissuer",
            "notvalidforname",
            "expired",
            "handshake",
            "verification",
        ] {
            if text.contains(needle) {
                return true;
            }
        }
        source = err.source();
    }
    false
}

/// A rejection must be a TLS trust decision, not a failure to reach the local
/// server (which would make the test pass for the wrong reason).
fn assert_certificate_rejection(err: &std::io::Error, context: &str) {
    assert_ne!(
        err.kind(),
        std::io::ErrorKind::ConnectionRefused,
        "{context}: connection was refused, not rejected: {err}"
    );
    assert!(
        mentions_certificate(err),
        "{context}: expected a certificate rejection, got: {err}"
    );
}

fn assert_body_ok(body: &[u8], context: &str) {
    assert!(
        body.ends_with(b"ok"),
        "{context}: unexpected response body: {:?}",
        String::from_utf8_lossy(body)
    );
}

// ---------------------------------------------------------------------------
// Verifier backend
// ---------------------------------------------------------------------------

#[test]
fn backend_matches_the_platform() {
    let _g = lock();
    #[cfg(target_os = "macos")]
    assert_eq!(tls::backend_name(), tls::PLATFORM_VERIFIER_MACOS_BACKEND);
    #[cfg(target_os = "windows")]
    assert_eq!(tls::backend_name(), tls::PLATFORM_VERIFIER_WINDOWS_BACKEND);
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    assert_eq!(tls::backend_name(), tls::NATIVE_ROOTS_BACKEND);
}

// ---------------------------------------------------------------------------
// Enterprise CA, intermediate chain, hostname, expiry
// ---------------------------------------------------------------------------

/// A leaf signed by a CA that is not in any store is rejected.
#[test]
fn untrusted_self_signed_leaf_is_rejected() {
    let _g = lock();
    let cert = self_signed(LOCAL_SAN);
    let port = serve(&cert);
    let config = tls::client_config().expect("platform client config");
    let err = connect(&config, port, LOCAL_SAN).expect_err("self-signed leaf accepted");
    assert_certificate_rejection(&err, "self-signed leaf");
}

/// An enterprise CA added on top of platform trust is accepted; platform trust
/// itself is never replaced (`client_config_with_extra_roots` only adds).
#[test]
fn enterprise_ca_extra_root_is_accepted() {
    let _g = lock();
    let cert = ca_signed(LOCAL_SAN, "subagent-mcp enterprise CA");
    let port = serve(&cert);
    let config = tls::client_config_with_extra_roots(std::slice::from_ref(&cert.root))
        .expect("config with the enterprise CA");
    let body = connect(&config, port, LOCAL_SAN).expect("enterprise CA rejected");
    assert_body_ok(&body, "enterprise CA");
}

/// A root -> intermediate -> leaf chain is accepted when the server sends the
/// intermediate and only the root is trusted.
#[test]
fn intermediate_chain_is_accepted() {
    let _g = lock();
    let cert = intermediate_chain(LOCAL_SAN, true, "subagent-mcp intermediate root");
    let port = serve(&cert);
    let config = tls::client_config_with_extra_roots(std::slice::from_ref(&cert.root))
        .expect("config with the intermediate root");
    let body = connect(&config, port, LOCAL_SAN).expect("intermediate chain rejected");
    assert_body_ok(&body, "intermediate chain");
}

/// The same chain without the intermediate cannot be built from the root alone.
#[test]
fn intermediate_chain_without_the_intermediate_is_rejected() {
    let _g = lock();
    let cert = intermediate_chain(LOCAL_SAN, false, "subagent-mcp missing intermediate");
    let port = serve(&cert);
    let config = tls::client_config_with_extra_roots(std::slice::from_ref(&cert.root))
        .expect("config with the intermediate root");
    let err =
        connect(&config, port, LOCAL_SAN).expect_err("chain without the intermediate was accepted");
    assert_certificate_rejection(&err, "missing intermediate");
}

/// A trusted root does not make a name mismatch acceptable.
#[test]
fn hostname_mismatch_is_rejected() {
    let _g = lock();
    let cert = ca_signed("other.example", "subagent-mcp hostname CA");
    let port = serve(&cert);
    let config = tls::client_config_with_extra_roots(std::slice::from_ref(&cert.root))
        .expect("config with the hostname CA");
    let err = connect(&config, port, LOCAL_SAN).expect_err("hostname mismatch accepted");
    assert_certificate_rejection(&err, "hostname mismatch");
}

/// A root-signed leaf whose validity ended in 2020 is rejected even though its
/// root is trusted.
#[test]
fn expired_certificate_is_rejected() {
    let _g = lock();
    let cert = expired_ca_signed(LOCAL_SAN, "subagent-mcp expired CA");
    let port = serve(&cert);
    let config = tls::client_config_with_extra_roots(std::slice::from_ref(&cert.root))
        .expect("config with the expired leaf's root");
    let err = connect(&config, port, LOCAL_SAN).expect_err("expired certificate accepted");
    assert_certificate_rejection(&err, "expired certificate");
}

// ---------------------------------------------------------------------------
// Distrusted root: Linux native bundle
// ---------------------------------------------------------------------------

/// Restores `SSL_CERT_FILE` when the test ends, including on panic.
#[cfg(target_os = "linux")]
struct EnvVarGuard {
    name: &'static str,
    previous: Option<std::ffi::OsString>,
}

#[cfg(target_os = "linux")]
impl EnvVarGuard {
    fn set(name: &'static str, value: &std::path::Path) -> Self {
        let previous = std::env::var_os(name);
        // SAFETY: SERIAL is held for the whole test, so no other test reads or
        // writes the environment concurrently.
        unsafe { std::env::set_var(name, value) };
        Self { name, previous }
    }
}

#[cfg(target_os = "linux")]
impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        // SAFETY: SERIAL is still held while the guard is dropped.
        unsafe {
            match &self.previous {
                Some(value) => std::env::set_var(self.name, value),
                None => std::env::remove_var(self.name),
            }
        }
    }
}

/// Linux trusts the system CA bundle. A bundle that does not contain the
/// enterprise CA rejects the leaf; the same bundle with the CA added accepts
/// it, proving the native-root path is the deciding trust input.
///
/// This is an *absent root* case, not an explicit distrust marker: Linux has no
/// per-user distrust store in this crate's trust path. The explicit-distrust
/// evidence is the macOS `deny` / Windows `Disallowed` store case below.
#[cfg(target_os = "linux")]
#[test]
fn absent_root_is_rejected_by_the_native_bundle() {
    let _g = lock();
    let cert = ca_signed(LOCAL_SAN, "subagent-mcp native bundle CA");
    let unrelated = ca_signed(LOCAL_SAN, "subagent-mcp unrelated CA");
    let port = serve(&cert);
    let dir = tempfile::tempdir().unwrap();

    let distrusted = dir.path().join("distrusted-bundle.pem");
    std::fs::write(&distrusted, unrelated.root_pem.as_bytes()).unwrap();
    let _env = EnvVarGuard::set("SSL_CERT_FILE", &distrusted);
    let config = tls::client_config().expect("native bundle with an unrelated root");
    let err = connect(&config, port, LOCAL_SAN).expect_err("root absent from the bundle accepted");
    assert_certificate_rejection(&err, "native bundle without the enterprise CA");
    drop(_env);

    let trusted = dir.path().join("trusted-bundle.pem");
    std::fs::write(&trusted, format!("{}{}", unrelated.root_pem, cert.root_pem)).unwrap();
    let _env = EnvVarGuard::set("SSL_CERT_FILE", &trusted);
    let config = tls::client_config().expect("native bundle with the enterprise CA");
    let body = connect(&config, port, LOCAL_SAN).expect("native bundle root rejected");
    assert_body_ok(&body, "native bundle with the enterprise CA");
}

// ---------------------------------------------------------------------------
// Platform trust store: macOS and Windows
// ---------------------------------------------------------------------------

/// The store case mutates machine/user trust state, so it is `#[ignore]`d:
/// an ordinary `cargo test --all-targets` reports it as ignored (visible, never
/// a silent pass) and the dedicated CI jobs run it with `--ignored` plus this
/// variable. Running `--ignored` without the variable fails loudly instead of
/// touching the developer's store.
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn require_platform_trust_env() {
    assert_eq!(
        std::env::var("SUBAGENT_MCP_PLATFORM_TRUST_TESTS").as_deref(),
        Ok("1"),
        "this test installs and removes a temporary root in the platform trust store; set \
         SUBAGENT_MCP_PLATFORM_TRUST_TESTS=1 to run it (the platform-macos-* / \
         platform-windows-* CI jobs do)"
    );
}

/// Fixed common name of the temporary root, so the CI cleanup step and
/// `scripts/tls-test-fixtures.sh cleanup` can remove a leftover from a failed
/// run by name. It is not a real CA and never leaves the runner.
#[cfg(any(target_os = "macos", target_os = "windows"))]
const STORE_TEST_CA_NAME: &str = "subagent-mcp platform trust test CA";

/// A temporary root installed in the OS trust store, removed on drop.
#[cfg(any(target_os = "macos", target_os = "windows"))]
struct PlatformStoreTrust {
    #[cfg(target_os = "macos")]
    pem_path: std::path::PathBuf,
    #[cfg(target_os = "windows")]
    der_path: std::path::PathBuf,
    common_name: String,
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
impl PlatformStoreTrust {
    fn install(cert: &TestCert) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "subagent-mcp-tls-{}-{}",
            std::process::id(),
            cert.common_name.replace(' ', "-")
        ));
        std::fs::create_dir_all(&dir).unwrap();

        #[cfg(target_os = "macos")]
        {
            let pem_path = dir.join("root.pem");
            std::fs::write(&pem_path, cert.root_pem.as_bytes()).unwrap();
            let keychain = system_keychain();
            let pem = pem_path.to_string_lossy().into_owned();
            run_security(&[
                "add-trusted-cert",
                "-d",
                "-r",
                "trustRoot",
                "-k",
                &keychain,
                &pem,
            ]);
            Self {
                pem_path,
                common_name: cert.common_name.clone(),
            }
        }

        #[cfg(target_os = "windows")]
        {
            let der_path = dir.join("root.cer");
            std::fs::write(&der_path, &cert.root[..]).unwrap();
            let der = der_path.to_string_lossy().into_owned();
            run_powershell(&format!(
                "Import-Certificate -FilePath '{}' -CertStoreLocation Cert:\\CurrentUser\\Root",
                der.replace('\'', "''")
            ));
            Self {
                der_path,
                common_name: cert.common_name.clone(),
            }
        }
    }

    /// Mark the same root explicitly distrusted, so the platform must reject it
    /// even though it is also present in the trusted store.
    fn distrust(&self) {
        #[cfg(target_os = "macos")]
        {
            let keychain = system_keychain();
            let pem = self.pem_path.to_string_lossy().into_owned();
            run_security(&[
                "add-trusted-cert",
                "-d",
                "-r",
                "deny",
                "-k",
                &keychain,
                &pem,
            ]);
        }

        #[cfg(target_os = "windows")]
        {
            let der = self.der_path.to_string_lossy().into_owned();
            run_powershell(&format!(
                "Import-Certificate -FilePath '{}' -CertStoreLocation Cert:\\CurrentUser\\Disallowed",
                der.replace('\'', "''")
            ));
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
impl Drop for PlatformStoreTrust {
    fn drop(&mut self) {
        #[cfg(target_os = "macos")]
        {
            let pem = self.pem_path.to_string_lossy().into_owned();
            let _ = std::process::Command::new("sudo")
                .args(["-n", "security", "remove-trusted-cert", &pem])
                .status();
            let keychain = system_keychain();
            let _ = std::process::Command::new("sudo")
                .args([
                    "-n",
                    "security",
                    "delete-certificate",
                    "-c",
                    &self.common_name,
                    &keychain,
                ])
                .status();
        }

        #[cfg(target_os = "windows")]
        {
            for store in ["Root", "Disallowed"] {
                let _ = std::process::Command::new("certutil")
                    .args(["-delstore", "-user", store, &self.common_name])
                    .status();
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn system_keychain() -> String {
    "/Library/Keychains/System.keychain".to_string()
}

#[cfg(target_os = "macos")]
fn run_security(args: &[&str]) {
    // `sudo -n` fails immediately when the runner has no passwordless sudo,
    // instead of hanging on an authorization prompt. GitHub's macOS runners
    // provide passwordless sudo; the test is ignored unless explicitly enabled.
    let out = std::process::Command::new("sudo")
        .arg("-n")
        .arg("security")
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("sudo security {}: {e}", args.join(" ")));
    assert!(
        out.status.success(),
        "sudo security {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[cfg(target_os = "windows")]
fn run_powershell(script: &str) {
    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .output()
        .unwrap_or_else(|e| panic!("powershell: {e}"));
    assert!(
        out.status.success(),
        "powershell failed: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[cfg(target_os = "windows")]
fn run_certutil(args: &[&str]) {
    let out = std::process::Command::new("certutil")
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("certutil {}: {e}", args.join(" ")));
    assert!(
        out.status.success(),
        "certutil {} failed: {}{}",
        args.join(" "),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The decisive platform case: a root trusted only by the OS store is accepted
/// through `client_config()` (no extra roots), and the same root marked
/// distrusted is rejected. This proves the platform verifier consults OS trust
/// policy rather than a static root list.
///
/// Ignored by default so the general `verify` matrix reports it as ignored
/// rather than a pass; the dedicated macOS/Windows jobs run it with
/// `--ignored` and `SUBAGENT_MCP_PLATFORM_TRUST_TESTS=1`.
#[cfg(any(target_os = "macos", target_os = "windows"))]
#[test]
#[ignore = "mutates the platform trust store; run with --ignored and SUBAGENT_MCP_PLATFORM_TRUST_TESTS=1"]
fn platform_store_trust_and_distrust_apply() {
    require_platform_trust_env();
    let _g = lock();
    let cert = ca_signed(LOCAL_SAN, STORE_TEST_CA_NAME);
    let port = serve(&cert);
    let store = PlatformStoreTrust::install(&cert);

    let config = tls::client_config().expect("platform client config");
    let body = connect(&config, port, LOCAL_SAN).expect("platform store root rejected");
    assert_body_ok(&body, "platform store trust");

    store.distrust();
    let config = tls::client_config().expect("platform client config");
    let err = connect(&config, port, LOCAL_SAN).expect_err("distrusted platform root accepted");
    assert_certificate_rejection(&err, "platform store distrust");
}
