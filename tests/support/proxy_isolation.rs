//! Run proxy-environment tests in one-test child processes, avoiding env races
//! with parallel client builders. The parent records a local HTTP proxy.
use std::io::{Read, Write};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

pub fn run(test: &str, proxy_var: &str) -> Option<Vec<String>> {
    if std::env::var("SUBAGENT_PROXY_TEST_CHILD").as_deref() == Ok(test) {
        return None;
    }
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let proxy = format!("http://{}", listener.local_addr().unwrap());
    listener.set_nonblocking(true).unwrap();
    let done = Arc::new(AtomicBool::new(false));
    let stopped = done.clone();
    let recorder = std::thread::spawn(move || {
        let mut requests = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(20);
        while !stopped.load(Ordering::SeqCst) && Instant::now() < deadline {
            match listener.accept() {
                Ok((mut socket, _)) => {
                    socket
                        .set_read_timeout(Some(Duration::from_secs(1)))
                        .unwrap();
                    let mut bytes = Vec::new();
                    let mut buf = [0; 4096];
                    while !bytes.windows(4).any(|w| w == b"\r\n\r\n") {
                        match socket.read(&mut buf) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => bytes.extend_from_slice(&buf[..n]),
                        }
                    }
                    requests.push(String::from_utf8_lossy(&bytes).into_owned());
                    let _ = socket.write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\nConnection: close\r\n\r\nproxied",
                    );
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5))
                }
                Err(e) => panic!("proxy accept: {e}"),
            }
        }
        requests
    });
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture"])
        .env("SUBAGENT_PROXY_TEST_CHILD", test)
        .env_remove("HTTP_PROXY")
        .env_remove("http_proxy")
        .env_remove("HTTPS_PROXY")
        .env_remove("https_proxy")
        .env_remove("NO_PROXY")
        .env_remove("no_proxy")
        .env_remove("ALL_PROXY")
        .env_remove("all_proxy")
        .env(proxy_var, proxy)
        .output()
        .unwrap();
    done.store(true, Ordering::SeqCst);
    let requests = recorder.join().unwrap();
    assert!(
        output.status.success(),
        "proxy regression child failed:\n{}\n{}\nproxy requests: {requests:?}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Some(requests)
}
