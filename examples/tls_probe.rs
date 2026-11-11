//! TLS verifier probe for the platform trust-policy spike.
//!
//! Connects to an HTTPS URL through the same rustls client configuration the
//! crate uses in production ([`subagent_mcp::provider::tls::client_config`]) and
//! reports whether the platform verifier accepted the certificate chain.
//!
//! Scenarios:
//!
//! ```text
//! # 1. System roots: a public host must be accepted.
//! cargo run --example tls_probe -- --url https://example.com --expect accept
//!
//! # 2. Distrusted root: a fresh self-signed leaf must be rejected.
//! cargo run --example tls_probe -- --self-signed --expect reject
//!
//! # 3. Enterprise CA: a leaf signed by a temporary CA that is installed in
//! #    the platform store (or supplied with --ca-file) must be accepted.
//! cargo run --example tls_probe -- --enterprise-ca --expect accept
//!
//! # 4. Expired certificate: a root-signed leaf valid only in 2020 must be
//! #    rejected even though its root is trusted.
//! cargo run --example tls_probe -- --expired --expect reject
//!
//! # 5. Intermediate chain: the server sends leaf + intermediate and only the
//! #    root is trusted; the chain must be accepted.
//! cargo run --example tls_probe -- --intermediate-chain --expect accept
//!
//! # 6. An externally installed enterprise CA, against a real endpoint:
//! cargo run --example tls_probe -- --url https://internal.example:8443 \
//!     --ca-file /path/to/enterprise-root.pem --expect accept
//! ```
//!
//! All local certificates and keys are generated in-process for the probe. They
//! are test-only, ephemeral, and never written to disk or committed.
//!
//! Exit codes: `0` the observed decision matched `--expect`, `1` it did not,
//! `2` the probe could not run (bad arguments, TLS config failure). The
//! evidence file for the spike records the command and this exit code.
//!
//! `--ca-file` adds roots on top of platform trust through
//! [`subagent_mcp::provider::tls::client_config_with_extra_roots`]. That proves
//! the extra-root path; the hard gate for macOS/Windows is still the OS store
//! case (`--url` with a CA installed by the OS), because native-roots-only is
//! not an acceptable substitute there.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::time::Duration;

use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, IsCa, KeyPair, KeyUsagePurpose,
    date_time_ymd,
};
use rustls::ServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use subagent_mcp::provider::tls;

const DEFAULT_URL: &str = "https://example.com";
const DEFAULT_TIMEOUT_SECONDS: u64 = 15;
const LOCAL_SAN: &str = "127.0.0.1";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Expect {
    Accept,
    Reject,
}

impl Expect {
    fn as_str(self) -> &'static str {
        match self {
            Expect::Accept => "accept",
            Expect::Reject => "reject",
        }
    }
}

#[derive(Clone, Copy)]
enum LocalServer {
    SelfSigned,
    EnterpriseCa,
    Expired,
    IntermediateChain,
}

struct Options {
    url: String,
    expect: Expect,
    extra_roots: Vec<CertificateDer<'static>>,
    local: Option<LocalServer>,
    url_explicit: bool,
    timeout: Duration,
    json: bool,
}

enum Decision {
    Accepted { status: u16, version: String },
    Rejected { detail: String },
    Failed { detail: String },
}

impl Decision {
    fn as_str(&self) -> &'static str {
        match self {
            Decision::Accepted { .. } => "accepted",
            Decision::Rejected { .. } => "rejected",
            Decision::Failed { .. } => "failed",
        }
    }
}

fn matches(expect: Expect, decision: &Decision) -> bool {
    matches!(
        (expect, decision),
        (Expect::Accept, Decision::Accepted { .. }) | (Expect::Reject, Decision::Rejected { .. })
    )
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let options = match Options::parse(std::env::args().skip(1)) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("tls_probe: {message}");
            usage();
            std::process::exit(2);
        }
    };

    let exit = match run(&options).await {
        Ok((url, decision)) => {
            print_result(&options, &url, &decision);
            if matches(options.expect, &decision) {
                0
            } else {
                1
            }
        }
        Err(message) => {
            eprintln!("tls_probe: {message}");
            2
        }
    };
    std::process::exit(exit);
}

async fn run(options: &Options) -> Result<(String, Decision), String> {
    let (url, mut extra_roots) = match options.local {
        None => (options.url.clone(), Vec::new()),
        Some(mode) => {
            let local = local_server(mode)?;
            let port = serve(local.chain, local.key)?;
            let extra = local.root.into_iter().collect();
            (format!("https://{LOCAL_SAN}:{port}/"), extra)
        }
    };
    extra_roots.extend(options.extra_roots.iter().cloned());

    let config = tls::client_config_with_extra_roots(&extra_roots)
        .map_err(|e| format!("TLS config: {e}"))?;
    let client = reqwest::Client::builder()
        .use_preconfigured_tls(config)
        .timeout(options.timeout)
        .build()
        .map_err(|e| format!("build HTTP client: {e}"))?;

    let decision = match client.get(url.as_str()).send().await {
        Ok(response) => Decision::Accepted {
            status: response.status().as_u16(),
            version: format!("{:?}", response.version()),
        },
        Err(error) => {
            let detail = error_chain(&error);
            if mentions_certificate(&error) {
                Decision::Rejected { detail }
            } else {
                Decision::Failed { detail }
            }
        }
    };
    Ok((url, decision))
}

/// A generated chain plus the trust anchor the probe installs as an extra root.
/// Every key is ephemeral and test-only.
struct LocalCert {
    root: Option<CertificateDer<'static>>,
    chain: Vec<CertificateDer<'static>>,
    key: Vec<u8>,
}

fn local_server(mode: LocalServer) -> Result<LocalCert, String> {
    match mode {
        LocalServer::SelfSigned => self_signed_local(),
        LocalServer::EnterpriseCa => ca_signed_leaf(false),
        LocalServer::Expired => ca_signed_leaf(true),
        LocalServer::IntermediateChain => intermediate_chain_local(),
    }
}

fn ca_params(constraint: IsCa) -> Result<CertificateParams, String> {
    let mut params =
        CertificateParams::new(Vec::<String>::new()).map_err(|e| format!("CA parameters: {e}"))?;
    params.is_ca = constraint;
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    Ok(params)
}

fn leaf_params() -> Result<CertificateParams, String> {
    CertificateParams::new(vec![LOCAL_SAN.to_string()]).map_err(|e| format!("leaf parameters: {e}"))
}

fn self_signed_local() -> Result<LocalCert, String> {
    let key = KeyPair::generate().map_err(|e| format!("generate key: {e}"))?;
    let cert = leaf_params()?
        .self_signed(&key)
        .map_err(|e| format!("self-signed certificate: {e}"))?;
    Ok(LocalCert {
        root: None,
        chain: vec![cert.der().clone()],
        key: key.serialize_der(),
    })
}

/// A leaf signed by a temporary root. `expired` fixes its validity to
/// 2020-01-01..2020-01-02, which keeps the rejection case deterministic instead
/// of depending on the wall clock.
fn ca_signed_leaf(expired: bool) -> Result<LocalCert, String> {
    let ca = CertifiedIssuer::self_signed(
        ca_params(IsCa::Ca(BasicConstraints::Unconstrained))?,
        KeyPair::generate().map_err(|e| format!("generate CA key: {e}"))?,
    )
    .map_err(|e| format!("self-signed CA: {e}"))?;

    let leaf_key = KeyPair::generate().map_err(|e| format!("generate leaf key: {e}"))?;
    let mut params = leaf_params()?;
    if expired {
        params.not_before = date_time_ymd(2020, 1, 1);
        params.not_after = date_time_ymd(2020, 1, 2);
    }
    let leaf = params
        .signed_by(&leaf_key, &ca)
        .map_err(|e| format!("sign leaf: {e}"))?;
    Ok(LocalCert {
        root: Some(ca.der().clone()),
        chain: vec![leaf.der().clone()],
        key: leaf_key.serialize_der(),
    })
}

/// root -> intermediate -> leaf. The server sends leaf + intermediate; the
/// probe trusts only the root, so acceptance depends on the intermediate.
fn intermediate_chain_local() -> Result<LocalCert, String> {
    let root = CertifiedIssuer::self_signed(
        ca_params(IsCa::Ca(BasicConstraints::Unconstrained))?,
        KeyPair::generate().map_err(|e| format!("generate root key: {e}"))?,
    )
    .map_err(|e| format!("self-signed root: {e}"))?;
    let intermediate = CertifiedIssuer::signed_by(
        ca_params(IsCa::Ca(BasicConstraints::Constrained(0)))?,
        KeyPair::generate().map_err(|e| format!("generate intermediate key: {e}"))?,
        &root,
    )
    .map_err(|e| format!("sign intermediate: {e}"))?;

    let leaf_key = KeyPair::generate().map_err(|e| format!("generate leaf key: {e}"))?;
    let leaf = leaf_params()?
        .signed_by(&leaf_key, &intermediate)
        .map_err(|e| format!("sign leaf: {e}"))?;
    Ok(LocalCert {
        root: Some(root.der().clone()),
        chain: vec![leaf.der().clone(), intermediate.der().clone()],
        key: leaf_key.serialize_der(),
    })
}

/// Serve one HTTP/1.1 response over TLS on a loopback port.
fn serve(chain: Vec<CertificateDer<'static>>, key: Vec<u8>) -> Result<u16, String> {
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(chain, PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key)))
        .map_err(|e| format!("server TLS config: {e}"))?;
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| format!("bind: {e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("local address: {e}"))?
        .port();
    std::thread::spawn(move || {
        let Ok((tcp, _)) = listener.accept() else {
            return;
        };
        let _ = tcp.set_read_timeout(Some(Duration::from_secs(10)));
        let _ = tcp.set_write_timeout(Some(Duration::from_secs(10)));
        let Ok(connection) = rustls::ServerConnection::new(Arc::new(config)) else {
            return;
        };
        let mut stream = rustls::StreamOwned::new(connection, tcp);
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
    Ok(port)
}

fn print_result(options: &Options, url: &str, decision: &Decision) {
    if options.json {
        let (detail, version) = match decision {
            Decision::Accepted { version, .. } => (String::new(), version.clone()),
            Decision::Rejected { detail } | Decision::Failed { detail } => {
                (detail.clone(), String::new())
            }
        };
        let value = serde_json::json!({
            "backend": tls::backend_name(),
            "url": url,
            "expect": options.expect.as_str(),
            "decision": decision.as_str(),
            "version": version,
            "detail": detail,
        });
        println!("{value}");
        return;
    }

    println!("backend: {}", tls::backend_name());
    println!("url: {url}");
    println!("expect: {}", options.expect.as_str());
    match decision {
        Decision::Accepted { status, version } => {
            println!("decision: accepted (status {status}, {version})")
        }
        Decision::Rejected { detail } => {
            println!("decision: rejected");
            println!("detail: {detail}");
        }
        Decision::Failed { detail } => {
            println!("decision: failed");
            println!("detail: {detail}");
        }
    }
}

fn error_chain(error: &dyn std::error::Error) -> String {
    let mut parts = vec![error.to_string()];
    let mut source = error.source();
    while let Some(err) = source {
        parts.push(err.to_string());
        source = err.source();
    }
    parts.join(": ")
}

/// Whether the error reports a certificate/handshake rejection rather than a
/// DNS, proxy, or connection failure. The full error chain is still printed.
fn mentions_certificate(error: &dyn std::error::Error) -> bool {
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

impl Options {
    fn parse(args: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut options = Options {
            url: DEFAULT_URL.to_string(),
            expect: Expect::Accept,
            extra_roots: Vec::new(),
            local: None,
            url_explicit: false,
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECONDS),
            json: false,
        };
        let mut args = args;
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--url" => {
                    options.url = value(&mut args, "--url")?;
                    options.url_explicit = true;
                }
                "--ca-file" => {
                    let path = value(&mut args, "--ca-file")?;
                    options.extra_roots.extend(load_pem_certificates(&path)?);
                }
                "--expect" => {
                    options.expect = match value(&mut args, "--expect")?.as_str() {
                        "accept" => Expect::Accept,
                        "reject" => Expect::Reject,
                        other => {
                            return Err(format!(
                                "--expect must be accept or reject, got {other:?}"
                            ));
                        }
                    };
                }
                "--timeout" => {
                    let seconds: u64 = value(&mut args, "--timeout")?
                        .parse()
                        .map_err(|_| "--timeout must be a number of seconds".to_string())?;
                    options.timeout = Duration::from_secs(seconds);
                }
                "--self-signed" => set_local(&mut options, LocalServer::SelfSigned)?,
                "--enterprise-ca" => set_local(&mut options, LocalServer::EnterpriseCa)?,
                "--expired" => set_local(&mut options, LocalServer::Expired)?,
                "--intermediate-chain" => {
                    set_local(&mut options, LocalServer::IntermediateChain)?;
                }
                "--json" => options.json = true,
                "-h" | "--help" => {
                    usage();
                    std::process::exit(0);
                }
                other => return Err(format!("unknown argument {other:?}")),
            }
        }
        if options.local.is_some() && options.url_explicit {
            return Err(
                "--url cannot be combined with --self-signed or --enterprise-ca".to_string(),
            );
        }
        Ok(options)
    }
}

fn set_local(options: &mut Options, local: LocalServer) -> Result<(), String> {
    if options.local.is_some() {
        return Err(
            "only one local server mode (--self-signed, --enterprise-ca, --expired, \
             --intermediate-chain) may be selected"
                .to_string(),
        );
    }
    options.local = Some(local);
    Ok(())
}

fn value(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, String> {
    args.next().ok_or_else(|| format!("{flag} needs a value"))
}

fn load_pem_certificates(path: &str) -> Result<Vec<CertificateDer<'static>>, String> {
    let text = std::fs::read(path).map_err(|e| format!("read {path}: {e}"))?;
    let blocks = pem::parse_many(text).map_err(|e| format!("parse {path}: {e}"))?;
    let mut certs = Vec::new();
    for block in blocks {
        if block.tag() == "CERTIFICATE" {
            certs.push(CertificateDer::from(block.contents().to_vec()));
        }
    }
    if certs.is_empty() {
        return Err(format!("no CERTIFICATE blocks in {path}"));
    }
    Ok(certs)
}

fn usage() {
    eprintln!(
        "usage: tls_probe [--url URL] [--expect accept|reject] [--ca-file PEM]...\n\
         \x20                [--self-signed | --enterprise-ca | --expired |\n\
         \x20                 --intermediate-chain] [--timeout SECONDS] [--json]\n\
         \n\
         Connects with the crate's platform TLS configuration and reports whether the\n\
         verifier accepted the chain. Exit 0 when the decision matches --expect."
    );
}
