#[cfg(unix)]
use std::io::BufRead as _;
#[cfg(unix)]
use std::io::BufReader;
use std::io::Read as _;
use std::io::Write as _;
use std::net::TcpListener;
#[cfg(unix)]
use std::net::TcpStream;
use std::path::Path;
#[cfg(unix)]
use std::process::Stdio;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use std::time::Instant;

use anyhow::Result;
use predicates::prelude::PredicateBooleanExt;
use predicates::str::contains;
use tempfile::TempDir;

fn codex_command(codex_home: &Path) -> Result<assert_cmd::Command> {
    let mut cmd = assert_cmd::Command::new(codex_utils_cargo_bin::cargo_bin("codex")?);
    cmd.env("CODEX_HOME", codex_home);
    Ok(cmd)
}

#[test]
fn strict_config_rejects_unknown_config_fields_for_exec_server() -> Result<()> {
    let codex_home = TempDir::new()?;
    std::fs::write(
        codex_home.path().join("config.toml"),
        r#"
foo = "bar"
"#,
    )?;

    let mut cmd = codex_command(codex_home.path())?;
    cmd.args([
        "exec-server",
        "--strict-config",
        "--listen",
        "http://127.0.0.1:0",
    ])
    .assert()
    .failure()
    .stderr(contains("unknown configuration field"));

    Ok(())
}

#[test]
fn local_exec_server_ignores_invalid_config_without_strict_config() -> Result<()> {
    let codex_home = TempDir::new()?;
    std::fs::write(codex_home.path().join("config.toml"), "not valid toml = [")?;

    let mut cmd = codex_command(codex_home.path())?;
    cmd.args(["exec-server", "--listen", "stdio"])
        .assert()
        .success()
        .stderr(contains("not valid toml").not());

    Ok(())
}
#[test]
fn local_exec_server_exports_real_otel_metrics() -> Result<()> {
    let collector = TestCollector::start()?;
    let codex_home = TempDir::new()?;
    let base_url = &collector.base_url;
    std::fs::write(
        codex_home.path().join("config.toml"),
        format!(
            r#"
[analytics]
enabled = true

[otel]
environment = "test"
metrics_exporter = {{ otlp-http = {{ endpoint = "{base_url}/v1/metrics", protocol = "json" }} }}
"#
        ),
    )?;

    let mut cmd = codex_command(codex_home.path())?;
    cmd.args(["exec-server", "--listen", "stdio"])
        .write_stdin(
            r#"{"id":1,"method":"initialize","params":{"clientName":"otel-test","resumeSessionId":null}}"#,
        )
        .assert()
        .success();

    let requests = collector.finish_after_exec_server_metrics(Duration::from_secs(10))?;
    let metrics = requests
        .iter()
        .filter(|request| request.path == "/v1/metrics")
        .map(|request| request.body.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        metrics.contains("exec_server_connections_active"),
        "{metrics}"
    );
    assert!(metrics.contains("exec_server_requests_total"), "{metrics}");
    assert!(metrics.contains("initialize"), "{metrics}");
    assert!(
        metrics.contains("success") || metrics.contains("disconnected"),
        "{metrics}"
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn local_exec_server_exits_successfully_on_sigterm() -> Result<()> {
    let codex_home = TempDir::new()?;
    let mut child = std::process::Command::new(codex_utils_cargo_bin::cargo_bin("codex")?)
        .env("CODEX_HOME", codex_home.path())
        .args(["exec-server", "--listen", "ws://127.0.0.1:0"])
        .stdout(Stdio::piped())
        .spawn()?;
    let mut listen_url = String::new();
    BufReader::new(child.stdout.take().expect("child stdout")).read_line(&mut listen_url)?;
    assert!(listen_url.starts_with("ws://127.0.0.1:"), "{listen_url}");

    let listen_addr = listen_url
        .trim()
        .strip_prefix("ws://")
        .expect("listen URL should use ws://")
        .parse()?;
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut ready = false;
    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        if let Ok(mut stream) =
            TcpStream::connect_timeout(&listen_addr, remaining.min(Duration::from_millis(100)))
        {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
            let request =
                format!("GET /readyz HTTP/1.1\r\nHost: {listen_addr}\r\nConnection: close\r\n\r\n");
            let mut response = String::new();
            if stream.write_all(request.as_bytes()).is_ok()
                && stream.read_to_string(&mut response).is_ok()
                && response.starts_with("HTTP/1.1 200")
            {
                ready = true;
                break;
            }
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(ready, "exec-server did not become ready at {listen_url}");

    // SAFETY: `child.id()` is the live process spawned above.
    let result = unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
    assert_eq!(result, 0);
    let status = child.wait()?;
    assert!(status.success(), "{status}");
    Ok(())
}

struct CapturedRequest {
    path: String,
    body: String,
}

struct TestCollector {
    base_url: String,
    requests: mpsc::Receiver<CapturedRequest>,
    stop: mpsc::Sender<()>,
    server: thread::JoinHandle<()>,
}

impl TestCollector {
    fn start() -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        listener.set_nonblocking(true)?;
        let (tx, requests) = mpsc::channel();
        let (stop, stop_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            loop {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        if let Ok(request) = read_http_request(&mut stream)
                            && tx.send(request).is_err()
                        {
                            break;
                        }
                        let _ = stream.write_all(
                            b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        );
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        if stop_rx.try_recv().is_ok() {
                            break;
                        }
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(Self {
            base_url: format!("http://{addr}"),
            requests,
            stop,
            server,
        })
    }

    fn finish_after_exec_server_metrics(self, timeout: Duration) -> Result<Vec<CapturedRequest>> {
        let deadline = Instant::now() + timeout;
        let mut requests = Vec::new();
        while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            match self.requests.recv_timeout(remaining) {
                Ok(request) => {
                    let found = request.path == "/v1/metrics"
                        && request.body.contains("exec_server_connections_active")
                        && request.body.contains("exec_server_requests_total")
                        && request.body.contains("initialize")
                        && (request.body.contains("success")
                            || request.body.contains("disconnected"));
                    requests.push(request);
                    if found {
                        break;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        let _ = self.stop.send(());
        self.server
            .join()
            .map_err(|_| anyhow::anyhow!("collector thread panicked"))?;
        while let Ok(request) = self.requests.try_recv() {
            requests.push(request);
        }
        Ok(requests)
    }
}

fn read_http_request(stream: &mut std::net::TcpStream) -> std::io::Result<CapturedRequest> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    let mut bytes = Vec::new();
    let mut scratch = [0_u8; 8192];
    let header_end = loop {
        let read = stream.read(&mut scratch)?;
        if read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "request closed before headers",
            ));
        }
        bytes.extend_from_slice(&scratch[..read]);
        if let Some(header_end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break header_end;
        }
    };
    let headers = String::from_utf8_lossy(&bytes[..header_end]);
    let mut lines = headers.split("\r\n");
    let path = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or_default()
        .to_string();
    let content_length = lines
        .filter_map(|line| line.split_once(':'))
        .find(|(key, _)| key.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        .unwrap_or_default();
    let mut body = bytes[header_end + 4..].to_vec();
    while body.len() < content_length {
        let read = stream.read(&mut scratch)?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&scratch[..read]);
    }
    body.truncate(content_length);
    Ok(CapturedRequest {
        path,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}
