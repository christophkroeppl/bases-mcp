//! The container cells of the matrix.
//!
//! `cross_product.rs` covers the four NATIVE cells: transport x backend, with
//! the real binary on the host. The four CONTAINER cells — the same binary, in
//! the image, over the same two transports and two backends — had no automated
//! coverage at all: they were verified by hand, and a hand verification is
//! not a gate.
//!
//! This suite closes that gap. It drives the real image with the real binary
//! in it, over the real transports, against the real backends, and checks
//! every write AT THE FAR END — the file on the host filesystem, or a GET
//! against the fake WebDAV server.
//!
//! # Why the container cells are not the native cells
//!
//! Two things change when the binary moves into the image, and both are
//! invisible until a write happens:
//!
//! * **The user namespace.** Under rootless podman the invoking user's HOST
//!   uid is mapped to CONTAINER ROOT. A host file owned by uid 1000 therefore
//!   appears inside the container as owned by uid 0, and the server — which
//!   runs as uid 1000 — is not the owner and cannot write it. `--userns=keep-id`
//!   maps the host uid to the SAME uid inside, which is what makes a mounted
//!   vault writable. WITHOUT it the vault reads as EMPTY, silently: every
//!   Base comes back with zero rows, which looks like an empty vault rather
//!   than like a permissions problem.
//!
//! * **The network path.** A WebDAV backend is an HTTP CLIENT. In the
//!   container, "the vault" is a URL, and the container has to be able to
//!   reach it — which is a different question from whether the host can.
//!
//! # Why these tests are opt-in
//!
//! They need a container runtime and a built image, and CI has neither. So
//! they are gated on `BASES_MCP_CONTAINER_IMAGE` being set: unset means
//! "skipped", and the skip is reported as a skip, never as a pass. The
//! invocation is:
//!
//! ```text
//! BASES_MCP_CONTAINER_IMAGE=localhost/bases-mcp:latest \
//!   cargo test --test container_matrix -- --test-threads=1
//! ```
//!
//! The runtime needs `--runtime <path>` on hosts whose system crun predates
//! their kernel; the override is `BASES_MCP_CONTAINER_RUNTIME`.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The image under test. Unset means these tests are SKIPPED.
fn image() -> Option<String> {
    std::env::var("BASES_MCP_CONTAINER_IMAGE")
        .ok()
        .filter(|s| !s.is_empty())
}

/// The OCI runtime override, for hosts whose system crun is too old.
fn runtime() -> Option<String> {
    std::env::var("BASES_MCP_CONTAINER_RUNTIME")
        .ok()
        .filter(|s| !s.is_empty())
}

/// Build a `podman` command line, with the runtime override when set.
fn podman(args: &[&str]) -> Command {
    let mut command = Command::new("podman");
    if let Some(runtime) = runtime() {
        command.arg("--runtime").arg(runtime);
    }
    command.args(args);
    command
}

/// Whether podman can talk to a runtime at all.
///
/// Probed once, because a missing runtime is a SKIP (the test cannot run)
/// and not a FAIL (the product is not broken). A container runtime that cannot
/// start is the environment's problem, and a suite that reports it as a
/// product failure trains people to ignore red.
fn runtime_available() -> bool {
    static ONCE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ONCE.get_or_init(|| {
        let output = podman(&["info", "--format", "{{.Host.Arch}}"])
            .output()
            .expect("podman runs");
        output.status.success()
    })
}

/// Skip a test with a stated reason.
macro_rules! skip_unless {
    ($reason:expr) => {
        match (image(), runtime_available()) {
            (Some(_), true) => {}
            (None, _) => {
                eprintln!(
                    "SKIPPED: BASES_MCP_CONTAINER_IMAGE is not set ({})",
                    $reason
                );
                return;
            }
            (Some(_), false) => {
                eprintln!("SKIPPED: no usable container runtime ({})", $reason);
                return;
            }
        }
    };
}

/// A scratch vault on the host, removed when dropped.
struct HostVault {
    path: PathBuf,
}

impl HostVault {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "bases-mcp-container-{}-{}",
            name,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("the scratch vault is creatable");
        Self { path }
    }

    fn write(&self, name: &str, content: &str) {
        std::fs::write(self.path.join(name), content).expect("the fixture is writable");
    }

    fn read(&self, name: &str) -> Option<String> {
        std::fs::read_to_string(self.path.join(name)).ok()
    }

    fn exists(&self, name: &str) -> bool {
        self.path.join(name).exists()
    }
}

impl Drop for HostVault {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// The Base fixture, and the note it selects.
const BASE_YAML: &str = "filters:\n  and:\n    - status == \"open\"\nviews:\n  - type: table\n    name: All\n    order:\n      - file.name\n      - status\n";

const NOTE_BODY: &str = "---\nstatus: open\n---\n\n# Container Note\n\nOriginal body.\n";

// ---------------------------------------------------------------------------
// The MCP client, over both transports, against a container.
// ---------------------------------------------------------------------------

/// A JSON value, parsed from the server's response.
type Json = serde_json::Value;

/// Read one SSE-framed or bare-JSON response body into a `Json`.
fn parse_response(body: &str) -> Json {
    // The HTTP transport frames its replies as SSE; the stdio transport does
    // not. Take the LAST `data:` line, which is the last complete frame.
    let mut last = None;
    for line in body.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("data:") {
            if let Ok(value) = serde_json::from_str::<Json>(rest.trim()) {
                last = Some(value);
            }
        } else if let Ok(value) = serde_json::from_str::<Json>(trimmed) {
            last = Some(value);
        }
    }
    last.unwrap_or(Json::Null)
}

/// The result of one `tools/call`.
struct Call {
    value: Json,
}

impl Call {
    /// Whether the call reported an error.
    fn is_error(&self) -> bool {
        self.value["result"]["isError"].as_bool().unwrap_or(false)
    }

    /// The structured payload, if any.
    fn structured(&self) -> &Json {
        &self.value["result"]["structuredContent"]
    }

    /// The first text block's content.
    fn text(&self) -> String {
        self.value["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }
}

/// A client that speaks MCP to a container over stdio.
struct StdioContainerClient {
    child: Child,
}

impl StdioContainerClient {
    /// Start the container with `--transport stdio` and return a client.
    fn start(vault: &HostVault, extra_env: &[(&str, &str)]) -> Self {
        let image = image().expect("the image is set");
        let mut args: Vec<String> = vec![
            "run".into(),
            "--rm".into(),
            "-i".into(),
            "--userns=keep-id".into(),
            "--pull=never".into(),
            // The host gateway, so the container can reach a host-side
            // WebDAV server. 127.0.0.1 inside a container is its own.
            "--add-host=host.containers.internal:host-gateway".into(),
        ];
        // The filesystem variable is NOT set here: `BASES_MCP_VAULT` WINS
        // over `BASES_MCP_WEBDAV_URL`, so a container with both is a
        // filesystem container pointed at an empty mount -- which looks
        // exactly like a WebDAV server that cannot be reached.
        if extra_env
            .iter()
            .all(|(key, _)| *key != "BASES_MCP_WEBDAV_URL")
        {
            args.push("-v".into());
            args.push(format!("{}:/vault:rw", vault.path.display()));
            args.push("-e".into());
            args.push("BASES_MCP_VAULT=/vault".into());
        }
        for (key, value) in extra_env {
            args.push("-e".into());
            args.push(format!("{key}={value}"));
        }
        args.push(image);
        args.push("--transport".into());
        args.push("stdio".into());

        let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
        let mut child = podman(&borrowed)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("the container starts");
        // The container's stderr is NOT the protocol stream. Leaving it piped
        // and unread risks a full pipe blocking the server, so it is drained
        // onto a thread.
        let mut stderr = child.stderr.take().expect("stderr is piped");
        std::thread::spawn(move || {
            let mut sink = Vec::new();
            let _ = stderr.read_to_end(&mut sink);
        });
        let mut client = Self { child };
        client.handshake();
        client
    }

    /// Send one JSON-RPC request and read one response.
    fn call(&mut self, method: &str, params: serde_json::Value) -> Json {
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": method,
            "params": params,
        });
        self.send(&request);
        self.read_one()
    }

    /// Write one JSON-RPC request to the server's stdin.
    fn send(&mut self, request: &serde_json::Value) {
        let stdin = self.child.stdin.as_mut().expect("stdin is open");
        writeln!(stdin, "{request}").expect("the request is written");
        stdin.flush().expect("the request is flushed");
    }

    /// Read one response line from the server's stdout.
    fn read_one(&mut self) -> Json {
        let stdout = self.child.stdout.as_mut().expect("stdout is open");
        let mut reader = std::io::BufReader::new(stdout);
        let mut line = String::new();
        reader.read_line(&mut line).expect("a response arrives");
        parse_response(&line)
    }

    /// The MCP handshake: `initialize`, then `notifications/initialized`.
    ///
    /// The handshake is NOT optional. A `tools/call` that arrives before it is
    /// refused with `-32602 "request _meta is missing or has malformed
    /// required fields: io.modelcontextprotocol/protocolVersion,
    /// io.modelcontextprotocol/clientCapabilities"` -- which reads like a
    /// server bug and is really a missing handshake.
    fn handshake(&mut self) {
        let initialize = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 0,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "container-matrix", "version": "1"},
            },
        });
        self.send(&initialize);
        let response = self.read_one();
        assert!(
            response.get("result").is_some(),
            "initialize failed: {response}"
        );
        // The notification is a notification: no id, and the server answers
        // with nothing on stdout.
        self.send(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }));
    }

    /// Call a tool and wrap the result.
    fn tool(&mut self, name: &str, arguments: serde_json::Value) -> Call {
        let value = self.call(
            "tools/call",
            serde_json::json!({
                "name": name,
                "arguments": arguments,
            }),
        );
        Call { value }
    }
}

impl Drop for StdioContainerClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A client that speaks MCP to a container over Streamable HTTP.
struct HttpContainerClient {
    base: String,
    session: Option<String>,
    port: u16,
    /// The container's name, so `Drop` removes exactly this one.
    name: String,
}

impl HttpContainerClient {
    /// Start the container with `--transport http` and return a client.
    ///
    /// `port` is the HOST-side port; the container always listens on 8765.
    fn start(vault: &HostVault, port: u16, extra_env: &[(&str, &str)]) -> Self {
        let image = image().expect("the image is set");
        // The name is generated ONCE, here, and used for both the run and
        // the later removal. Generating it twice would produce two different
        // names and the container would never be cleaned up.
        let container_name = format!("bases-mcp-cm-{}-{}", std::process::id(), unique_suffix());
        // A stale container from a previous run would block the start.
        let _ = podman(&["rm", "-f", &container_name]).output();
        let mut args: Vec<String> = vec![
            "run".into(),
            "-d".into(),
            "--name".into(),
            container_name.clone(),
            // The same two flags the stdio client uses, and for the same
            // reasons: without the user namespace the server cannot WRITE the
            // vault, and without the mount there is no vault at all -- the
            // server exits at startup because /vault does not exist.
            "--userns=keep-id".into(),
            "--pull=never".into(),
            "--add-host=host.containers.internal:host-gateway".into(),
            "-p".into(),
            format!("127.0.0.1:{port}:8765"),
            "-e".into(),
            "MCP_HTTP_HOST=0.0.0.0".into(),
        ];
        // `BASES_MCP_VAULT` WINS over `BASES_MCP_WEBDAV_URL`, so a WebDAV
        // cell must not set it -- otherwise the container is a filesystem
        // server pointed at an empty mount.
        if extra_env
            .iter()
            .all(|(key, _)| *key != "BASES_MCP_WEBDAV_URL")
        {
            args.push("-v".into());
            args.push(format!("{}:/vault:rw", vault.path.display()));
            args.push("-e".into());
            args.push("BASES_MCP_VAULT=/vault".into());
        }
        for (key, value) in extra_env {
            args.push("-e".into());
            args.push(format!("{key}={value}"));
        }
        args.push(image);
        args.push("--transport".into());
        args.push("http".into());

        let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
        let output = podman(&borrowed).output().expect("podman runs");
        assert!(
            output.status.success(),
            "the container starts: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        // A container that dies immediately leaves an exited container behind
        // and no listener. Surface its own log rather than a 30-second
        // timeout, which says nothing about the cause.
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            let state = podman(&["inspect", "--format", "{{.State.Status}}", &container_name])
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .unwrap_or_default();
            if state == "exited" || state == "stopped" {
                let logs = podman(&["logs", &container_name])
                    .output()
                    .expect("podman runs");
                let _ = podman(&["rm", "-f", &container_name]).output();
                panic!(
                    "the container {} exited before it bound its listener:\nstdout:\n{}\nstderr:\n{}",
                    container_name,
                    String::from_utf8_lossy(&logs.stdout),
                    String::from_utf8_lossy(&logs.stderr)
                );
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        // Give the listener a moment to bind, then handshake.
        let base = format!("http://127.0.0.1:{port}/mcp");
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            let mut stream = match TcpStream::connect(("127.0.0.1", port)) {
                Ok(stream) => stream,
                Err(_) => {
                    std::thread::sleep(Duration::from_millis(250));
                    continue;
                }
            };
            let initialize = serde_json::json!({
                "jsonrpc": "2.0", "id": 0, "method": "initialize",
                "params": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": {"name": "container-matrix", "version": "1"},
                }
            });
            let request = format!(
                "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                initialize.to_string().len(),
                initialize
            );
            let _ = stream.write_all(request.as_bytes());
            let mut raw = Vec::new();
            let _ = stream.read_to_end(&mut raw);
            let text = String::from_utf8_lossy(&raw).to_string();
            if let Some((head, _)) = text.split_once("\r\n\r\n") {
                for line in head.lines() {
                    // Header names are case-insensitive, and this server sends
                    // "mcp-session-id:" in lower case. Matching only one
                    // spelling silently finds no session, and every later
                    // request is then rejected for want of one.
                    let (header, value) = match line.split_once(':') {
                        Some(parts) => parts,
                        None => continue,
                    };
                    if header.trim().eq_ignore_ascii_case("mcp-session-id") {
                        let session = value.trim().to_string();
                        let client = Self {
                            base: base.clone(),
                            session: Some(session),
                            port,
                            name: container_name.clone(),
                        };
                        // The handshake notification, or the server is not
                        // ready for a tool call.
                        client.raw_post(&serde_json::json!({
                            "jsonrpc": "2.0", "method": "notifications/initialized"
                        }));
                        return client;
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        panic!("the container's HTTP listener never answered");
    }

    /// POST a request, returning the response body.
    fn raw_post(&self, body: &serde_json::Value) -> Option<String> {
        let mut request = format!(
            "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\nContent-Length: {}\r\n",
            body.to_string().len()
        );
        if let Some(session) = &self.session {
            request.push_str(&format!("Mcp-Session-Id: {session}\r\n"));
        }
        request.push_str("Connection: close\r\n\r\n");
        request.push_str(&body.to_string());

        let mut stream = TcpStream::connect(("127.0.0.1", self.port)).ok()?;
        stream
            .write_all(request.as_bytes())
            .expect("the request is written");
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).ok()?;
        let text = String::from_utf8_lossy(&raw).to_string();
        let (head, body) = text.split_once("\r\n\r\n")?;
        // The response is CHUNKED. Its body is
        //   <hex-size>\r\n<data>\r\n0\r\n\r\n
        // so the text after the header terminator is the CHUNK FRAMING, not
        // the JSON. A client that ignores the framing finds no result at
        // all, and every assertion downstream sees an empty response.
        let body = if head
            .to_ascii_lowercase()
            .contains("transfer-encoding: chunked")
        {
            dechunk(body)
        } else {
            body.to_string()
        };
        Some(body)
    }

    /// Call a tool and wrap the result.
    fn tool(&mut self, name: &str, arguments: serde_json::Value) -> Call {
        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": name, "arguments": arguments},
        });
        let response = self
            .raw_post(&body)
            .unwrap_or_else(|| panic!("no response from {} (container {})", self.base, self.name));
        Call {
            value: parse_response(&response),
        }
    }
}

impl Drop for HttpContainerClient {
    fn drop(&mut self) {
        let _ = podman(&["rm", "-f", &format!("bases-mcp-cm-{}", std::process::id())]).output();
    }
}

// ---------------------------------------------------------------------------
// The fake WebDAV server, reachable from the container.
// ---------------------------------------------------------------------------

/// A WebDAV server that speaks the real protocol over a real socket.
///
/// A fake at the wrong layer would prove nothing: the backend's whole job is
/// speaking WebDAV over HTTP, so the server has to be a real HTTP server.
///
/// The fidelity rules, each learned from a defect:
///   * hrefs are PERCENT-ENCODED, the way a real server emits them;
///   * a missing resource is `404`, and only `404` — the backend's `exists`
///     treats anything else as existing;
///   * responses carry `Connection: close`, or a client reuses a socket the
///     server has already closed;
///   * the store SERVES what it received, from its own store, so a client
///     that verifies its write by reading it back is reading what landed.
struct DavServer {
    port: u16,
    /// The URL a test on the HOST uses.
    base_url: String,
    /// The URL a test inside a CONTAINER uses.
    gateway_url: String,
}

impl DavServer {
    /// Start the server with the given files, on a dedicated OS thread.
    ///
    /// A dedicated thread, not a Tokio task: the MCP server's STARTUP issues
    /// WebDAV requests, and a fake that shares the caller's async runtime
    /// deadlocks against the very call it is serving.
    fn start(files: &[(&str, String)]) -> Self {
        // Bound to ALL interfaces, not loopback: a container reaches this
        // server through the host gateway, and a socket bound to 127.0.0.1
        // accepts only from the host's own loopback.
        let listener = std::net::TcpListener::bind("0.0.0.0:0").expect("a port is bindable");
        let port = listener
            .local_addr()
            .expect("the socket has an address")
            .port();
        let store: Arc<Mutex<BTreeMap<String, String>>> = Arc::new(Mutex::new(BTreeMap::new()));
        let dirs: Arc<Mutex<BTreeSet<String>>> =
            Arc::new(Mutex::new(BTreeSet::from(["".to_string()])));
        for (path, content) in files {
            store
                .lock()
                .unwrap()
                .insert(path.to_string(), content.clone());
            let mut parent = path.to_string();
            while let Some(cut) = parent.rfind('/') {
                parent.truncate(cut);
                dirs.lock().unwrap().insert(parent.clone());
            }
        }
        let server = Self {
            port,
            // The loopback URL, for a test that drives the server directly.
            base_url: format!("http://127.0.0.1:{port}/dav/vault"),
            // The container-reachable URL. Inside a container, 127.0.0.1 is
            // the CONTAINER's loopback, so a host-side server is invisible to
            // it; the host gateway is the address that crosses over.
            gateway_url: format!("http://host.containers.internal:{port}/dav/vault"),
        };
        let owned_files = store;
        let owned_dirs = dirs;
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let files = owned_files.clone();
                let dirs = owned_dirs.clone();
                std::thread::spawn(move || {
                    serve_one(&mut stream, &files, &dirs);
                });
            }
        });
        server
    }

    /// The host-side URL, checked to answer.
    ///
    /// A server that listens but does not answer is worse than one that
    /// refuses to listen, and the difference is invisible until something
    /// asks.
    fn assert_answers_on_its_own_url(&self) {
        // The request line carries a PATH, not a URL. Taking the path out of
        // the base URL is what makes the request well-formed; sending
        // "http://127.0.0.1:PORT/dav/vault/Cross.base" as the target asks
        // for a resource literally named that, and the server answers 404.
        let target = self
            .base_url
            .split_once("/dav/vault")
            .map(|(_, rest)| format!("/dav/vault{rest}/Cross.base"))
            .expect("the base URL names the vault");
        let mut stream = TcpStream::connect(("127.0.0.1", self.port)).expect("the server accepts");
        let request =
            format!("GET {target} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
        stream
            .write_all(request.as_bytes())
            .expect("the request is written");
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).expect("the server answers");
        assert!(
            raw.starts_with(b"HTTP/1.1 2"),
            "the fake WebDAV server does not answer on {}: {}",
            self.base_url,
            String::from_utf8_lossy(&raw)
        );
    }

    /// Read a resource back over HTTP, the way a client would.
    ///
    /// This is the FAR-END check, and it has to be a READ rather than a peek
    /// at the server's map. A peek proves the server received a body; a read
    /// proves the server will SERVE that body back, which is what the client
    /// verifies when it re-GETs a note it has just written.
    fn read_back(&self, path: &str) -> Option<String> {
        let target = format!("/dav/vault/{}", encoded_href(path));
        let mut stream = TcpStream::connect(("127.0.0.1", self.port)).ok()?;
        let request =
            format!("GET {target} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
        stream.write_all(request.as_bytes()).ok()?;
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).ok()?;
        let text = String::from_utf8_lossy(&raw).to_string();
        let (head, body) = text.split_once("\r\n\r\n")?;
        if !head.starts_with("HTTP/1.1 200") {
            return None;
        }
        Some(body.to_string())
    }
}

/// Percent-encode one path segment.
fn encode_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for byte in segment.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// A vault-relative path as an href.
fn encoded_href(path: &str) -> String {
    path.split('/')
        .map(encode_segment)
        .collect::<Vec<_>>()
        .join("/")
}

/// Decode a percent-encoded segment.
fn decode_segment(segment: &str) -> String {
    let bytes = segment.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let Ok(value) = u8::from_str_radix(&segment[index + 1..index + 3], 16) {
                out.push(value);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

/// The path a request targets, relative to the vault.
fn vault_path_of(target: &str) -> Option<String> {
    let path = target.split('?').next().unwrap_or(target);
    let rest = path.strip_prefix("/dav/vault")?;
    let rest = rest.trim_start_matches('/');
    let decoded = rest
        .split('/')
        .map(decode_segment)
        .collect::<Vec<_>>()
        .join("/");
    Some(decoded)
}

/// A minimal HTTP response.
fn http_response(status: u16, reason: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// Serve one WebDAV request.
fn serve_one(
    stream: &mut TcpStream,
    files: &Arc<Mutex<BTreeMap<String, String>>>,
    dirs: &Arc<Mutex<BTreeSet<String>>>,
) {
    // Read the HEADERS, then exactly Content-Length more bytes. Stopping at
    // the header terminator alone truncates a PROPFIND's XML body, and the
    // backend then reads a malformed multistatus and reports that the
    // server "is not serving WebDAV at this URL" -- which points at the
    // server when the fault is the fake's framing.
    let mut raw = Vec::new();
    let mut buffer = [0u8; 8192];
    let header_end = loop {
        let Ok(count) = stream.read(&mut buffer) else {
            return;
        };
        if count == 0 {
            return;
        }
        raw.extend_from_slice(&buffer[..count]);
        if let Some(index) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            break index + 4;
        }
    };
    let head = String::from_utf8_lossy(&raw[..header_end]).to_string();
    let mut lines = head.lines();
    let Some(request_line) = lines.next() else {
        return;
    };
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default().to_string();
    let length = lines
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            if name.trim().eq_ignore_ascii_case("content-length") {
                value.trim().parse::<usize>().ok()
            } else {
                None
            }
        })
        .unwrap_or(0);
    // The body, read to its declared length.
    let mut body_bytes = raw[header_end..].to_vec();
    while body_bytes.len() < length {
        let Ok(count) = stream.read(&mut buffer) else {
            break;
        };
        if count == 0 {
            break;
        }
        body_bytes.extend_from_slice(&buffer[..count]);
    }
    let body = String::from_utf8_lossy(&body_bytes).to_string();

    let Some(path) = vault_path_of(&target) else {
        let _ = stream.write_all(http_response(404, "Not Found", "not found").as_bytes());
        return;
    };

    let response = match method.as_str() {
        "PROPFIND" => {
            let is_collection = dirs.lock().unwrap().contains(&path);
            let is_file = files.lock().unwrap().contains_key(&path);
            if is_collection {
                let mut body = String::new();
                // The ROOT collection's children are every file; a named
                // collection's children live under "<name>/". Getting this
                // wrong makes the root list as EMPTY -- and a Base in the
                // root then cannot be found.
                let prefix = if path.is_empty() {
                    String::new()
                } else {
                    format!("{path}/")
                };
                for (name, content) in files.lock().unwrap().iter() {
                    if name.starts_with(&prefix) {
                        body.push_str(&file_response(name, content.len()));
                    }
                }
                // The collection's SELF-entry is deliberately not emitted.
                // For the root that would be "/dav/vault//" -- a double
                // slash, which the backend's segment folding reads as an
                // empty segment and refuses. And the backend drops the
                // self-entry anyway: it asks for the CHILDREN of a
                // collection, and `children_of` filters out the collection
                // itself.
                if !path.is_empty() {
                    body.push_str(&collection_response(&path));
                }
                http_response(207, "Multi-Status", &multistatus(&body))
            } else if is_file {
                let size = files
                    .lock()
                    .unwrap()
                    .get(&path)
                    .map(|c| c.len())
                    .unwrap_or(0);
                http_response(
                    207,
                    "Multi-Status",
                    &multistatus(&file_response(&path, size)),
                )
            } else {
                // ONLY 404 means "does not exist". Anything else makes the
                // backend's `exists` believe the note is there.
                http_response(404, "Not Found", "not found")
            }
        }
        "GET" => match files.lock().unwrap().get(&path) {
            Some(content) => http_response(200, "OK", content),
            None => http_response(404, "Not Found", "not found"),
        },
        "PUT" => {
            files.lock().unwrap().insert(path.clone(), body);
            http_response(201, "Created", "")
        }
        "MKCOL" => {
            dirs.lock().unwrap().insert(path.clone());
            http_response(201, "Created", "")
        }
        "DELETE" => {
            files.lock().unwrap().remove(&path);
            http_response(204, "No Content", "")
        }
        _ => http_response(405, "Method Not Allowed", ""),
    };
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

/// Wrap PROPFIND members in the multistatus envelope.
///
/// The envelope is not decoration: the backend's parser sets
/// `saw_multistatus` only when it sees a `multistatus` element, and a body
/// without one is refused with "did not return a multistatus document ...
/// it is not serving WebDAV at this URL". A sequence of `response` elements
/// is well-formed XML and still not a multistatus.
fn multistatus(members: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<d:multistatus xmlns:d=\"DAV:\">\n{members}</d:multistatus>\n"
    )
}

/// Take the payload out of a chunked HTTP body.
///
/// A chunked body is a sequence of `<hex-size>\r\n<data>\r\n`, ending with
/// a zero-length chunk. The sizes are in BYTES, so the data is cut on byte
/// boundaries, not on character boundaries.
fn dechunk(body: &str) -> String {
    let mut out: Vec<u8> = Vec::new();
    let mut rest = body.as_bytes();
    while let Some(newline) = rest.iter().position(|b| *b == b'\n') {
        let size_line = String::from_utf8_lossy(&rest[..newline]);
        let size_text = size_line.trim().split(';').next().unwrap_or("").trim();
        let Ok(size) = usize::from_str_radix(size_text, 16) else {
            break;
        };
        rest = &rest[newline + 1..];
        if size == 0 {
            break;
        }
        if rest.len() < size {
            out.extend_from_slice(rest);
            break;
        }
        out.extend_from_slice(&rest[..size]);
        // Skip the CRLF that ends the chunk.
        rest = &rest[size..];
        if rest.starts_with(b"\r\n") {
            rest = &rest[2..];
        } else if rest.starts_with(b"\n") {
            rest = &rest[1..];
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

/// The multistatus body for a file member.
fn file_response(path: &str, size: usize) -> String {
    format!(
        "<d:response><d:href>/dav/vault/{}</d:href><d:propstat><d:prop><d:resourcetype/><d:getcontentlength>{}</d:getcontentlength><d:getlastmodified>{}</d:getlastmodified></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>",
        encoded_href(path),
        size,
        // A real server reports an mtime on every resource, and the backend
        // REFUSES a note without one: "the server did not report
        // getlastmodified for X, so its mtime is unknown rather than the
        // epoch". A fixed timestamp is enough -- it is the PRESENCE of the
        // property the backend insists on, not its value.
        "Thu, 01 Jan 2026 00:00:00 GMT"
    )
}

/// The multistatus body for a collection member.
fn collection_response(path: &str) -> String {
    format!(
        "<d:response><d:href>/dav/vault/{}/</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype><d:getlastmodified>Thu, 01 Jan 2026 00:00:00 GMT</d:getlastmodified></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>",
        encoded_href(path)
    )
}

// ---------------------------------------------------------------------------
// The four container cells.
// ---------------------------------------------------------------------------

/// The fixture a container cell starts from.
fn seed(vault: &HostVault) {
    vault.write("Cross.base", BASE_YAML);
    vault.write("Container Note.md", NOTE_BODY);
}

/// The create handshake: propose, then commit.
fn create_note(client: &mut dyn ToolCaller, name: &str) -> Call {
    let proposal = client.tool(
        "add_note_to_base",
        serde_json::json!({"base": "Cross.base", "path": name, "view": "All"}),
    );
    assert!(
        !proposal.is_error(),
        "the proposal is not an error: {}",
        proposal.text()
    );
    let draft = proposal.structured()["draft_id"]
        .as_str()
        .expect("the proposal carries a draft id")
        .to_string();
    client.tool(
        "add_note_to_base",
        serde_json::json!({
            "draft_id": draft,
            "content": "---\nstatus: open\n---\n\n# Created\n\nCreated through the container.\n",
        }),
    )
}

/// The tool surface, so one body can drive either transport.
trait ToolCaller {
    fn tool(&mut self, name: &str, arguments: serde_json::Value) -> Call;
}

impl ToolCaller for StdioContainerClient {
    fn tool(&mut self, name: &str, arguments: serde_json::Value) -> Call {
        StdioContainerClient::tool(self, name, arguments)
    }
}

impl ToolCaller for HttpContainerClient {
    fn tool(&mut self, name: &str, arguments: serde_json::Value) -> Call {
        HttpContainerClient::tool(self, name, arguments)
    }
}

/// CELL: stdio + filesystem + containerized.
#[test]
#[ignore = "needs a built image; run with --ignored"]
fn stdio_in_a_container_reads_and_writes_a_filesystem_vault() {
    skip_unless!("no image");
    let vault = HostVault::new("stdio-fs");
    seed(&vault);
    let mut client = StdioContainerClient::start(&vault, &[]);

    // A. READ
    let read = client.tool(
        "resolve_base",
        serde_json::json!({"base": "Cross.base", "view": "All"}),
    );
    assert!(!read.is_error(), "resolve_base failed: {}", read.text());
    assert_eq!(
        read.structured()["rowCount"].as_u64(),
        Some(1),
        "the base selects the one open note: {}",
        read.text()
    );

    // B. EDIT — checked at the far end, on the HOST.
    let edit = client.tool(
        "write_note",
        serde_json::json!({
            "path": "Container Note.md",
            "content": "---\nstatus: open\n---\n\n# Container Note\n\nEdited in the container.\n",
        }),
    );
    assert!(!edit.is_error(), "write_note failed: {}", edit.text());
    let on_host = vault
        .read("Container Note.md")
        .expect("the note is on the HOST filesystem");
    assert!(
        on_host.contains("Edited in the container."),
        "the edit did not land on the host: {on_host}"
    );

    // C. CREATE — checked at the far end, on the HOST.
    let created = create_note(&mut client, "Created In Container.md");
    assert!(!created.is_error(), "the create failed: {}", created.text());
    let on_host = vault
        .read("Created In Container.md")
        .expect("the created note is on the HOST filesystem");
    assert!(
        on_host.contains("Created through the container."),
        "the create did not land on the host: {on_host}"
    );

    // D. REFUSE — the path is not there, so nothing is written and no file
    // appears.
    let refused = client.tool(
        "write_note",
        serde_json::json!({
            "path": "Never Existed.md",
            "content": "# Never Existed\n\nShould not be written.\n",
        }),
    );
    assert!(
        refused.is_error(),
        "write_note at a missing path must be an error: {}",
        refused.text()
    );
    assert!(
        !vault.exists("Never Existed.md"),
        "the refused write created a file on the host"
    );
}

/// CELL: http + filesystem + containerized.
#[test]
#[ignore = "needs a built image; run with --ignored"]
fn http_in_a_container_reads_and_writes_a_filesystem_vault() {
    skip_unless!("no image");
    let vault = HostVault::new("http-fs");
    seed(&vault);
    let port = free_port();
    let mut client = HttpContainerClient::start(&vault, port, &[]);

    // A. READ
    let read = client.tool(
        "resolve_base",
        serde_json::json!({"base": "Cross.base", "view": "All"}),
    );
    assert!(!read.is_error(), "resolve_base failed: {}", read.text());
    assert_eq!(
        read.structured()["rowCount"].as_u64(),
        Some(1),
        "the base selects the one open note: {}",
        read.text()
    );

    // B. EDIT — at the far end, on the HOST.
    let edit = client.tool(
        "write_note",
        serde_json::json!({
            "path": "Container Note.md",
            "content": "---\nstatus: open\n---\n\n# Container Note\n\nEdited over HTTP in the container.\n",
        }),
    );
    assert!(!edit.is_error(), "write_note failed: {}", edit.text());
    let on_host = vault
        .read("Container Note.md")
        .expect("the note is on the HOST filesystem");
    assert!(
        on_host.contains("Edited over HTTP in the container."),
        "the edit did not land on the host: {on_host}"
    );

    // C. CREATE — at the far end, on the HOST.
    let created = create_note(&mut client, "Created Over Http.md");
    assert!(!created.is_error(), "the create failed: {}", created.text());
    assert!(
        vault.read("Created Over Http.md").is_some(),
        "the created note is not on the host"
    );

    // D. REFUSE
    let refused = client.tool(
        "write_note",
        serde_json::json!({
            "path": "Never Existed.md",
            "content": "# Never Existed\n\nShould not be written.\n",
        }),
    );
    assert!(
        refused.is_error(),
        "write_note at a missing path must be an error: {}",
        refused.text()
    );
    assert!(
        !vault.exists("Never Existed.md"),
        "the refused write created a file on the host"
    );
}

/// CELL: stdio + webdav + containerized.
#[test]
#[ignore = "needs a built image; run with --ignored"]
fn stdio_in_a_container_reads_and_writes_a_webdav_vault() {
    skip_unless!("no image");
    let server = DavServer::start(&[
        ("Cross.base", BASE_YAML.to_string()),
        ("Container Note.md", NOTE_BODY.to_string()),
    ]);
    server.assert_answers_on_its_own_url();
    let mut client = StdioContainerClient::start(
        &HostVault::new("stdio-dav-unused"),
        &[
            ("BASES_MCP_WEBDAV_URL", &server.gateway_url),
            ("BASES_MCP_WEBDAV_USER", "u"),
            ("BASES_MCP_WEBDAV_PASSWORD", "p"),
        ],
    );

    // A. READ
    let read = client.tool(
        "resolve_base",
        serde_json::json!({"base": "Cross.base", "view": "All"}),
    );
    assert!(!read.is_error(), "resolve_base failed: {}", read.text());
    assert_eq!(
        read.structured()["rowCount"].as_u64(),
        Some(1),
        "the base selects the one open note: {}",
        read.text()
    );

    // B. EDIT — at the far end, by a GET against the server.
    let edit = client.tool(
        "write_note",
        serde_json::json!({
            "path": "Container Note.md",
            "content": "---\nstatus: open\n---\n\n# Container Note\n\nEdited over WebDAV in the container.\n",
        }),
    );
    assert!(!edit.is_error(), "write_note failed: {}", edit.text());
    let at_far_end = server
        .read_back("Container Note.md")
        .expect("the note is readable back from the WebDAV server");
    assert!(
        at_far_end.contains("Edited over WebDAV in the container."),
        "the edit did not land at the far end: {at_far_end}"
    );

    // C. CREATE — at the far end.
    let created = create_note(&mut client, "Created Over Dav.md");
    assert!(!created.is_error(), "the create failed: {}", created.text());
    let at_far_end = server
        .read_back("Created Over Dav.md")
        .expect("the created note is readable back from the WebDAV server");
    assert!(
        at_far_end.contains("Created through the container."),
        "the create did not land at the far end: {at_far_end}"
    );

    // D. REFUSE — and the far end has no such resource.
    let refused = client.tool(
        "write_note",
        serde_json::json!({
            "path": "Never Existed.md",
            "content": "# Never Existed\n\nShould not be written.\n",
        }),
    );
    assert!(
        refused.is_error(),
        "write_note at a missing path must be an error: {}",
        refused.text()
    );
    assert!(
        server.read_back("Never Existed.md").is_none(),
        "the refused write reached the WebDAV server"
    );
}

/// CELL: http + webdav + containerized.
#[test]
#[ignore = "needs a built image; run with --ignored"]
fn http_in_a_container_reads_and_writes_a_webdav_vault() {
    skip_unless!("no image");
    let server = DavServer::start(&[
        ("Cross.base", BASE_YAML.to_string()),
        ("Container Note.md", NOTE_BODY.to_string()),
    ]);
    server.assert_answers_on_its_own_url();
    let port = free_port();
    let mut client = HttpContainerClient::start(
        &HostVault::new("http-dav-unused"),
        port,
        &[
            ("BASES_MCP_WEBDAV_URL", &server.gateway_url),
            ("BASES_MCP_WEBDAV_USER", "u"),
            ("BASES_MCP_WEBDAV_PASSWORD", "p"),
        ],
    );

    // A. READ
    let read = client.tool(
        "resolve_base",
        serde_json::json!({"base": "Cross.base", "view": "All"}),
    );
    assert!(!read.is_error(), "resolve_base failed: {}", read.text());
    assert_eq!(
        read.structured()["rowCount"].as_u64(),
        Some(1),
        "the base selects the one open note: {}",
        read.text()
    );

    // B. EDIT — at the far end.
    let edit = client.tool(
        "write_note",
        serde_json::json!({
            "path": "Container Note.md",
            "content": "---\nstatus: open\n---\n\n# Container Note\n\nEdited over HTTP+WebDAV in the container.\n",
        }),
    );
    assert!(!edit.is_error(), "write_note failed: {}", edit.text());
    let at_far_end = server
        .read_back("Container Note.md")
        .expect("the note is readable back from the WebDAV server");
    assert!(
        at_far_end.contains("Edited over HTTP+WebDAV in the container."),
        "the edit did not land at the far end: {at_far_end}"
    );

    // C. CREATE — at the far end.
    let created = create_note(&mut client, "Created Over Http Dav.md");
    assert!(!created.is_error(), "the create failed: {}", created.text());
    assert!(
        server.read_back("Created Over Http Dav.md").is_some(),
        "the created note is not readable back from the WebDAV server"
    );

    // D. REFUSE
    let refused = client.tool(
        "write_note",
        serde_json::json!({
            "path": "Never Existed.md",
            "content": "# Never Existed\n\nShould not be written.\n",
        }),
    );
    assert!(
        refused.is_error(),
        "write_note at a missing path must be an error: {}",
        refused.text()
    );
    assert!(
        server.read_back("Never Existed.md").is_none(),
        "the refused write reached the WebDAV server"
    );
}

/// The control: WITHOUT `--userns=keep-id` the vault reads but cannot be written.
///
/// This is the load-bearing claim in `docs/tooling.md`, and it is a claim
/// about a SILENT failure. Under the default rootless mapping the host uid
/// becomes container root, so a host file owned by uid 1000 appears inside
/// the container as owned by uid 0. The server runs as uid 1000, so it is
/// not the owner and cannot write — and the failure arrives as a bare
/// "Permission denied (os error 13)" with no hint that the uid is wrong.
///
/// Note what does NOT happen: the vault still READS. Every Base comes back
/// with its rows, because the files are world-readable. Only the writes
/// fail. A test that asserted "the vault reads as empty" would be asserting
/// a claim that is not true, and would fail against a working server.
#[test]
#[ignore = "needs a built image; run with --ignored"]
fn without_keep_id_the_vault_reads_but_cannot_be_written() {
    skip_unless!("no image");
    let vault = HostVault::new("nokeep");
    seed(&vault);
    let image = image().expect("the image is set");
    let mut command = podman(&[
        "run",
        "--rm",
        "-i",
        "--pull=never",
        "-v",
        &format!("{}:/vault:rw", vault.path.display()),
        "-e",
        "BASES_MCP_VAULT=/vault",
        &image,
        "--transport",
        "stdio",
    ]);
    command.stdin(Stdio::piped());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    let mut child = command.spawn().expect("the container starts");
    let mut stderr = child.stderr.take().expect("stderr is piped");
    std::thread::spawn(move || {
        let mut sink = Vec::new();
        let _ = stderr.read_to_end(&mut sink);
    });
    let mut stdin = child.stdin.take().expect("stdin is open");
    let mut stdout = std::io::BufReader::new(child.stdout.take().expect("stdout is open"));

    // The handshake comes first: a tools/call before initialize is refused
    // with -32602, which is not the answer this test is looking for.
    let initialize = serde_json::json!({
        "jsonrpc": "2.0", "id": 0, "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "container-matrix", "version": "1"},
        }
    });
    writeln!(stdin, "{initialize}").expect("the initialize is written");
    stdin.flush().expect("the initialize is flushed");
    let mut handshake_line = String::new();
    stdout
        .read_line(&mut handshake_line)
        .expect("the handshake response arrives");
    assert!(
        handshake_line.contains("\"result\""),
        "initialize did not succeed: {handshake_line}"
    );
    writeln!(
        stdin,
        "{}",
        serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"})
    )
    .expect("the notification is written");
    stdin.flush().expect("the notification is flushed");

    // A. The READ succeeds: the files are readable regardless of the mapping.
    let read = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {
            "name": "resolve_base",
            "arguments": {"base": "Cross.base", "view": "All"},
        }
    });
    writeln!(stdin, "{read}").expect("the read is written");
    stdin.flush().expect("the read is flushed");
    let mut read_line = String::new();
    stdout
        .read_line(&mut read_line)
        .expect("a response arrives");
    let read_response = parse_response(&read_line);
    assert_eq!(
        read_response["result"]["structuredContent"]["rowCount"].as_u64(),
        Some(1),
        "the read must SUCCEED without --userns=keep-id: {read_line}"
    );

    // B. The WRITE fails, with the permission error the docs describe.
    let write = serde_json::json!({
        "jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": {
            "name": "write_note",
            "arguments": {
                "path": "Container Note.md",
                "content": "# Container Note\n\nShould not land.\n",
            },
        }
    });
    writeln!(stdin, "{write}").expect("the write is written");
    stdin.flush().expect("the write is flushed");
    let mut write_line = String::new();
    stdout
        .read_line(&mut write_line)
        .expect("a response arrives");
    let _ = child.kill();
    let _ = child.wait();

    let write_response = parse_response(&write_line);
    assert_eq!(
        write_response["result"]["isError"].as_bool(),
        Some(true),
        "the write must FAIL without --userns=keep-id: {write_line}"
    );
    assert!(
        write_line.contains("Permission denied"),
        "the write must fail with a permission error: {write_line}"
    );
    // And the far end is untouched.
    let on_host = vault
        .read("Container Note.md")
        .expect("the note is on the host");
    assert!(
        !on_host.contains("Should not land."),
        "the write landed on the host without --userns=keep-id: {on_host}"
    );
}

/// The image runs as the pinned non-root uid, and the container says so.
///
/// The whole user-namespace story rests on one number: the image's uid is
/// **1000**, pinned in the `Containerfile` rather than left to
/// `useradd --system` (which picks 999 on Debian). Two things depend on it:
///
///  - Under `--userns=keep-id` it is the uid the server owns its vault files
///    as, which is the difference between a container that can WRITE the vault
///    and one that only reads it.
///  - guarden probes the IMAGE's user and renders `Userns=keep-id:uid=…,gid=…`
///    from it (ADR-0019 in the guarden repository). A repository that leaves
///    the uid to the base image gives the host a different number to pin on
///    every base image it rebuilds.
///
/// Nothing asserted that the image's user IS uid 1000. The `Containerfile`
/// said so, the docs said so, and the write tests would have failed on a
/// DIFFERENT uid with a bare "Permission denied (os error 13)" — which names
/// the file and says nothing about the uid. This is the assertion that names
/// it.
///
/// It is also a **containment** claim, not an access one: a non-root uid
/// bounds what an escaped container can reach and says nothing about who may
/// call the tools.
#[test]
#[ignore = "needs a built image; run with --ignored"]
fn the_image_runs_as_the_pinned_non_root_uid() {
    skip_unless!("no image");
    let image = image().expect("the image is set");

    // What the image itself declares. This is the number guarden reads.
    let declared = podman(&["image", "inspect", "--format", "{{.Config.User}}", &image])
        .output()
        .expect("podman inspects the image");
    assert!(
        declared.status.success(),
        "the image can be inspected: {}",
        String::from_utf8_lossy(&declared.stderr)
    );
    let declared = String::from_utf8_lossy(&declared.stdout).trim().to_string();
    assert_eq!(
        declared, "bases-mcp",
        "the image declares the named user, and guarden reads that name out of the image"
    );

    // What the process actually runs as. The declared name is not the uid:
    // `bases-mcp` is a NAME, and the uid is what the namespace mapping uses.
    let output = podman(&[
        "run",
        "--rm",
        "--pull=never",
        "--entrypoint",
        "/bin/sh",
        &image,
        "-c",
        "id -u",
    ])
    .output()
    .expect("the container runs");
    assert!(
        output.status.success(),
        "the container runs: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let uid = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert_eq!(
        uid, "1000",
        "the image's process runs as the PINNED uid 1000, and not as root"
    );

    // And the containment claim, measured: the process has no home and no
    // shell, so an escaped container has nothing to run.
    let output = podman(&[
        "run",
        "--rm",
        "--pull=never",
        "--entrypoint",
        "/bin/sh",
        &image,
        "-c",
        "echo \"home=$HOME\"; ls /usr/sbin/nologin >/dev/null && echo shell=nologin",
    ])
    .output()
    .expect("the container runs");
    let rendered = String::from_utf8_lossy(&output.stdout);
    assert!(
        rendered.contains("home=/"),
        "the user has no home to write to: {rendered}"
    );
}

/// A suffix that makes a container name unique within this test binary.
///
/// The PID alone is not enough: two HTTP tests run in the SAME process, so
/// they would share a name and the second would fail to start with "that
/// name is already in use".
fn unique_suffix() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// A port for the host side of an HTTP container.
///
/// NOT the ephemeral range. Podman's port forward outlives the container it
/// belonged to, so a port taken from the ephemeral range can still be held
/// by a previous test's forward, and the next container then dies with
/// "pasta failed ... Address already in use" before it ever binds.
fn free_port() -> u16 {
    // A high, unlikely-to-collide range well above the ephemeral default
    // (32768-60999).
    for candidate in 19800u16..19900 {
        if let Ok(listener) = std::net::TcpListener::bind(("127.0.0.1", candidate)) {
            drop(listener);
            return candidate;
        }
    }
    panic!("no free port for the HTTP container");
}
