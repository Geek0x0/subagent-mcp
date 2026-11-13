#![cfg(target_os = "linux")]
//! Isolated process: CA environment changes must not race other transport tests.

use subagent_mcp::provider::{TransportKind, transport::shared_client};

#[test]
fn failed_tls_build_is_not_cached_or_poisoned() {
    let dir = tempfile::tempdir().unwrap();
    let cert_file = std::env::var_os("SSL_CERT_FILE");
    let cert_dir = std::env::var_os("SSL_CERT_DIR");
    // SAFETY: this binary has one test and starts no background threads.
    unsafe {
        std::env::set_var("SSL_CERT_FILE", dir.path().join("missing.pem"));
        std::env::set_var("SSL_CERT_DIR", dir.path().join("missing-dir"));
    }
    for _ in 0..2 {
        let error =
            shared_client(TransportKind::Responses, "http://retry-tls.invalid").unwrap_err();
        assert!(
            error
                .message
                .contains("tls: no usable native root certificates"),
            "{error}"
        );
    }
    unsafe {
        match cert_file {
            Some(v) => std::env::set_var("SSL_CERT_FILE", v),
            None => std::env::remove_var("SSL_CERT_FILE"),
        }
        match cert_dir {
            Some(v) => std::env::set_var("SSL_CERT_DIR", v),
            None => std::env::remove_var("SSL_CERT_DIR"),
        }
    }
    // Same key must retry construction successfully after the roots recover.
    shared_client(TransportKind::Responses, "http://retry-tls.invalid").unwrap();
}
