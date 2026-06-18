//! Serving-path smoke tests for the compiled `aionforge` binary.

use std::{
    fs,
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde_json::{Value, json};

const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(15);
const TCP_ATTEMPT_TIMEOUT: Duration = Duration::from_millis(500);

#[test]
fn serve_http_binary_answers_initialize_with_server_info() {
    let temp_dir = TempDir::new("aionforge-cli-serve-smoke");
    let data_dir = temp_dir.path().join("data");
    fs::create_dir_all(&data_dir).expect("create smoke-test data directory");
    restrict_dir_permissions(&data_dir);
    let console_dir = temp_dir.path().join("console");
    write_console_dist(&console_dir);

    let config_path = temp_dir.path().join("config.toml");
    fs::write(
        &config_path,
        r#"
[embedder]
enabled = false

[consolidation]
enabled = false
"#,
    )
    .expect("write smoke-test config");

    let listen_addr = unused_loopback_addr();
    let mut server = ServerProcess::spawn(&config_path, &data_dir, &console_dir, listen_addr);
    let initialize = match wait_for_initialize(&mut server, listen_addr, INITIALIZE_TIMEOUT) {
        Ok(initialize) => initialize,
        Err(error) => {
            let output = server.terminate();
            panic!(
                "aionforge serve http did not answer initialize within {:?}: {error}\n{}",
                INITIALIZE_TIMEOUT,
                output_summary(&output)
            );
        }
    };

    let server_info = initialize
        .pointer("/result/serverInfo")
        .expect("initialize response must include result.serverInfo");
    assert_eq!(server_info["name"], "aionforge-memory");
    assert_eq!(server_info["version"], env!("CARGO_PKG_VERSION"));

    assert_http_get(listen_addr, "/console", "200", "Aionforge console shell");
    assert_http_get(
        listen_addr,
        "/console/records",
        "200",
        "Aionforge console shell",
    );
    assert_http_get(
        listen_addr,
        "/console/_app/immutable/app.js",
        "200",
        "console asset",
    );
    assert_http_get(listen_addr, "/console/_app/missing.js", "404", "Not Found");
    assert_http_get(listen_addr, "/not-console", "404", "Not Found");

    let _ = server.terminate();
}

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(prefix: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&path).expect("create smoke-test temp directory");
        restrict_dir_permissions(&path);
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

struct ServerProcess {
    child: Option<Child>,
}

impl ServerProcess {
    fn spawn(
        config_path: &Path,
        data_dir: &Path,
        console_dir: &Path,
        listen_addr: SocketAddr,
    ) -> Self {
        let child = Command::new(env!("CARGO_BIN_EXE_aionforge"))
            .arg("--config")
            .arg(config_path)
            .arg("--data-dir")
            .arg(data_dir)
            .arg("serve")
            .arg("http")
            .arg("--listen")
            .arg(listen_addr.to_string())
            .env("AIONFORGE_EMBEDDER__ENABLED", "false")
            .env("AIONFORGE_CONSOLIDATION__ENABLED", "false")
            .env("AIONFORGE_CONSOLE_DIST_DIR", console_dir)
            .env("AIONFORGE_TRAFFIC_HEARTBEAT_SECS", "0")
            .env("RUST_LOG", "warn")
            .env_remove("AIONFORGE_ACTIVE_DEPLOYMENT")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn aionforge serve http");

        Self { child: Some(child) }
    }

    fn try_wait(&mut self) -> Result<Option<std::process::ExitStatus>, String> {
        self.child
            .as_mut()
            .expect("server process already terminated")
            .try_wait()
            .map_err(|error| format!("checking server process status failed: {error}"))
    }

    fn terminate(mut self) -> Output {
        let mut child = self
            .child
            .take()
            .expect("server process already terminated");
        let _ = child.kill();
        child
            .wait_with_output()
            .expect("collect aionforge serve http output")
    }
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn write_console_dist(console_dir: &Path) {
    fs::create_dir_all(console_dir.join("_app/immutable")).expect("create console asset tree");
    fs::write(
        console_dir.join("200.html"),
        "<!doctype html><title>Aionforge console shell</title>",
    )
    .expect("write console shell");
    fs::write(
        console_dir.join("_app/immutable/app.js"),
        "console.log('console asset');",
    )
    .expect("write console asset");
}

fn wait_for_initialize(
    server: &mut ServerProcess,
    listen_addr: SocketAddr,
    timeout: Duration,
) -> Result<Value, String> {
    let deadline = Instant::now() + timeout;

    loop {
        if let Some(status) = server.try_wait()? {
            return Err(format!(
                "server process exited before readiness probe: {status}"
            ));
        }

        let last_error = match post_initialize(listen_addr) {
            Ok(initialize) => return Ok(initialize),
            Err(error) => error,
        };

        if Instant::now() >= deadline {
            return Err(format!(
                "timed out waiting for MCP initialize; last error: {}",
                last_error
            ));
        }

        thread::sleep(Duration::from_millis(100));
    }
}

fn post_initialize(listen_addr: SocketAddr) -> Result<Value, String> {
    let mut stream = TcpStream::connect_timeout(&listen_addr, TCP_ATTEMPT_TIMEOUT)
        .map_err(|error| format!("connect failed: {error}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .map_err(|error| format!("set read timeout failed: {error}"))?;
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .map_err(|error| format!("set write timeout failed: {error}"))?;

    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {
                "name": "aionforge-cli-smoke",
                "version": "0.0.0"
            }
        }
    })
    .to_string();
    let request = format!(
        "POST /mcp HTTP/1.1\r\n\
         Host: {listen_addr}\r\n\
         Accept: application/json, text/event-stream\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        body.len()
    );

    stream
        .write_all(request.as_bytes())
        .map_err(|error| format!("write initialize request failed: {error}"))?;

    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .map_err(|error| format!("read initialize response failed: {error}"))?;
    parse_initialize_response(&response)
}

fn parse_initialize_response(response: &[u8]) -> Result<Value, String> {
    let header_end = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| "HTTP response did not include a header/body split".to_owned())?;
    let headers = String::from_utf8_lossy(&response[..header_end]);
    if !headers.starts_with("HTTP/1.1 200") {
        return Err(format!("initialize returned non-200 response: {headers}"));
    }

    let body = &response[header_end + 4..];
    let body = if headers
        .lines()
        .any(|line| line.eq_ignore_ascii_case("transfer-encoding: chunked"))
    {
        decode_chunked_body(body)?
    } else {
        String::from_utf8(body.to_vec())
            .map_err(|error| format!("initialize response body was not utf-8: {error}"))?
    };

    parse_sse_json_rpc(&body)
        .or_else(|| serde_json::from_str(&body).ok())
        .ok_or_else(|| {
            format!("initialize response body was neither SSE JSON-RPC nor JSON: {body}")
        })
}

fn assert_http_get(
    listen_addr: SocketAddr,
    path: &str,
    expected_status: &str,
    expected_body: &str,
) {
    let response =
        http_get(listen_addr, path).unwrap_or_else(|error| panic!("GET {path} failed: {error}"));
    assert!(
        response.status_line.contains(expected_status),
        "GET {path} returned {}; body: {}",
        response.status_line,
        response.body
    );
    assert!(
        response.body.contains(expected_body),
        "GET {path} body did not include {expected_body:?}: {}",
        response.body
    );
}

struct HttpGetResponse {
    status_line: String,
    body: String,
}

fn http_get(listen_addr: SocketAddr, path: &str) -> Result<HttpGetResponse, String> {
    let mut stream = TcpStream::connect_timeout(&listen_addr, TCP_ATTEMPT_TIMEOUT)
        .map_err(|error| format!("connect failed: {error}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .map_err(|error| format!("set read timeout failed: {error}"))?;
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .map_err(|error| format!("set write timeout failed: {error}"))?;

    let request = format!(
        "GET {path} HTTP/1.1\r\n\
         Host: {listen_addr}\r\n\
         Connection: close\r\n\
         \r\n"
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|error| format!("write GET request failed: {error}"))?;

    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .map_err(|error| format!("read GET response failed: {error}"))?;
    parse_http_get_response(&response)
}

fn parse_http_get_response(response: &[u8]) -> Result<HttpGetResponse, String> {
    let header_end = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| "HTTP response did not include a header/body split".to_owned())?;
    let headers = String::from_utf8_lossy(&response[..header_end]);
    let status_line = headers
        .lines()
        .next()
        .ok_or_else(|| "HTTP response did not include a status line".to_owned())?
        .to_owned();
    let body = &response[header_end + 4..];
    let body = if headers
        .lines()
        .any(|line| line.eq_ignore_ascii_case("transfer-encoding: chunked"))
    {
        decode_chunked_body(body)?
    } else {
        String::from_utf8(body.to_vec())
            .map_err(|error| format!("GET response body was not utf-8: {error}"))?
    };
    Ok(HttpGetResponse { status_line, body })
}

fn parse_sse_json_rpc(body: &str) -> Option<Value> {
    for line in body.lines() {
        if let Some(data) = line.strip_prefix("data:") {
            let data = data.trim();
            if data.is_empty() {
                continue;
            }
            if let Ok(value) = serde_json::from_str(data) {
                return Some(value);
            }
        }
    }
    None
}

fn decode_chunked_body(body: &[u8]) -> Result<String, String> {
    let mut decoded = Vec::new();
    let mut index = 0;

    loop {
        let header_end = body[index..]
            .windows(2)
            .position(|window| window == b"\r\n")
            .map(|position| index + position)
            .ok_or_else(|| "chunked body ended before a chunk header".to_owned())?;
        let size_header = std::str::from_utf8(&body[index..header_end])
            .map_err(|error| format!("chunk header was not utf-8: {error}"))?;
        let size_hex = size_header.split(';').next().unwrap_or(size_header).trim();
        let size = usize::from_str_radix(size_hex, 16)
            .map_err(|error| format!("invalid chunk size {size_hex:?}: {error}"))?;
        index = header_end + 2;

        if size == 0 {
            break;
        }

        let chunk_end = index
            .checked_add(size)
            .ok_or_else(|| "chunk size overflowed response length".to_owned())?;
        if body.len() < chunk_end + 2 {
            return Err("chunked body ended before declared chunk size".to_owned());
        }
        decoded.extend_from_slice(&body[index..chunk_end]);
        if &body[chunk_end..chunk_end + 2] != b"\r\n" {
            return Err("chunk was not followed by CRLF".to_owned());
        }
        index = chunk_end + 2;
    }

    String::from_utf8(decoded).map_err(|error| format!("decoded chunk body was not utf-8: {error}"))
}

#[cfg(unix)]
fn restrict_dir_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = fs::metadata(path)
        .expect("read smoke-test directory metadata")
        .permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(path, permissions).expect("restrict smoke-test directory permissions");
}

#[cfg(not(unix))]
fn restrict_dir_permissions(_path: &Path) {}

fn unused_loopback_addr() -> SocketAddr {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind ephemeral loopback port");
    listener
        .local_addr()
        .expect("read ephemeral loopback address")
}

fn output_summary(output: &Output) -> String {
    format!(
        "status: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}
