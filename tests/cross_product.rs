//! The transport x backend cross-product, driven end to end.
//!
//! The server has two independent axes, and every combination of them has to
//! work:
//!
//! |            | filesystem vault | WebDAV vault |
//! |------------|------------------|--------------|
//! | **stdio**  | must work        | must work    |
//! | **HTTP**   | must work        | must work    |
//!
//! The axes are independent by construction: the transport decides how bytes
//! travel, the backend decides where the vault lives, and the tool surface sits
//! between them. But "independent by construction" is a claim about the design,
//! not a measurement, and the two existing suites each pin ONE cell of this
//! table — `tests/stdio_transport.rs` covers (stdio, fs), and
//! `tests/http_transport.rs` covers (http, fs). **Nothing in the repository
//! covers (stdio, webdav) or (http, webdav)**, which are exactly the two cells
//! a deployment uses.
//!
//! So this suite drives all four, and it drives them the way a CLIENT does —
//! `initialize`, `tools/list`, `tools/call` — against a real spawned binary,
//! over the real transport. The WebDAV cell is backed by a real HTTP server
//! speaking the real protocol, because the backend's whole job is speaking
//! WebDAV over HTTP and a mock at the wrong layer would test nothing.
//!
//! # Writing, not just reading
//!
//! Every cell also WRITES. A read-only matrix is half the claim: the interesting
//! failure is a write that goes to the wrong backend, or a write that is
//! accepted and silently does nothing — the second is the one that destroys a
//! user's notes, and it is invisible to any test that only reads.
//!
//! The write is checked at the far end: the filesystem cell reads the file back
//! off disk, and the WebDAV cell reads it back from the server's own store. A
//! write that "succeeds" without landing anywhere fails here.

use std::io::{BufRead, BufReader, Write};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value as Json};

/// How long to wait for a listener to come up before giving up.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(20);

/// A port that is free right now.
///
/// Bound and released: the server binds it moments later, and the window is
/// small enough that a collision is not worth a retry loop.
fn ephemeral_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a port is bindable");
    let port = listener
        .local_addr()
        .expect("the socket has an address")
        .port();
    drop(listener);
    port
}

// ---------------------------------------------------------------------------
// The WebDAV server the WebDAV cells talk to
// ---------------------------------------------------------------------------

/// A minimal WebDAV server: PROPFIND, GET, PUT, MKCOL, DELETE.
///
/// Real HTTP on a real socket, because the thing under test is a client that
/// speaks WebDAV over HTTP. It stores what it is sent, so a write can be
/// checked at the far end — the property a mock at the wrong layer cannot prove.
///
/// # It runs on its own thread, deliberately
///
/// The first version of this fixture ran on the test's own Tokio runtime, and
/// it deadlocked: the MCP server PROPFINDs the vault while it starts up, the
/// test blocks waiting for the MCP server to open its port, and the PROPFIND
/// cannot be answered because the runtime that would answer it is blocked
/// behind the very wait it is causing.
///
/// A dedicated OS thread with blocking IO has no such dependency: the server
/// answers whoever asks, whenever they ask, whatever the test is doing.
#[derive(Clone)]
struct DavServer {
    /// The port the server listens on.
    port: u16,
    /// Vault-relative path -> the bytes stored for it.
    files: Arc<Mutex<std::collections::BTreeMap<String, String>>>,
    /// Vault-relative path -> its children, for PROPFIND.
    dirs: Arc<Mutex<std::collections::BTreeSet<String>>>,
    /// The base URL a client is given.
    base_url: String,
    /// Every request the server saw, for a failure message.
    trace: Arc<Mutex<Vec<String>>>,
}

impl DavServer {
    /// Start a server on an ephemeral port, seeded with `files`.
    fn start(files: &[(&str, String)]) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a port is bindable");
        let port = listener
            .local_addr()
            .expect("the socket has an address")
            .port();
        let server = Self {
            port,
            files: Arc::new(Mutex::new(std::collections::BTreeMap::new())),
            dirs: Arc::new(Mutex::new(std::collections::BTreeSet::from([
                "".to_string()
            ]))),
            base_url: format!("http://127.0.0.1:{port}/dav/vault"),
            trace: Arc::new(Mutex::new(Vec::new())),
        };
        for (path, content) in files {
            server
                .files
                .lock()
                .unwrap()
                .insert(path.to_string(), content.clone());
            // Every parent collection of the file is a collection too.
            let mut parent = path.to_string();
            while let Some(cut) = parent.rfind('/') {
                parent.truncate(cut);
                server.dirs.lock().unwrap().insert(parent.clone());
            }
        }
        let handler = server.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                let handler = handler.clone();
                std::thread::spawn(move || {
                    handler.serve(stream);
                });
            }
        });
        server
    }

    /// The requests the server saw, in order.
    fn trace(&self) -> Vec<String> {
        self.trace.lock().unwrap().clone()
    }

    /// What the server currently holds at `path`.
    fn stored(&self, path: &str) -> Option<String> {
        self.files.lock().unwrap().get(path).cloned()
    }

    /// Read a resource back over HTTP, the way a client would.
    ///
    /// This is the FAR-END check, and it has to be a READ rather than a peek at
    /// the server's map. A peek proves the server received a body; a read
    /// proves the server will SERVE that body back, which is what the real
    /// client verifies when it re-GETs a note it has just written. The two
    /// differ for the server that accepts a PUT and stores something else —
    /// a transcoding filter, a bug — and the difference is the whole point.
    ///
    /// Plain `std::net::TcpStream`, because the point is the HTTP round trip,
    /// not a client library.
    fn read_back(&self, path: &str) -> Option<String> {
        use std::io::{Read, Write};
        let target = format!("/dav/vault/{}", encoded_href(path));
        let mut stream = std::net::TcpStream::connect(("127.0.0.1", self.port)).ok()?;
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

    /// Handle one connection: read a request, answer it.
    fn serve(&self, mut stream: std::net::TcpStream) {
        use std::io::{Read, Write};
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 8192];
        // Read until the headers are complete.
        let head_end = loop {
            if let Some(at) = find_double_crlf(&buffer) {
                break at;
            }
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => return,
                Ok(read) => buffer.extend_from_slice(&chunk[..read]),
            }
        };
        let head = String::from_utf8_lossy(&buffer[..head_end]).to_string();
        self.trace
            .lock()
            .unwrap()
            .push(format!("--- request head ---\n{head}\n---"));
        let mut rest = buffer[head_end + 4..].to_vec();
        let content_length = header_value(&head, "content-length")
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0);
        // Then the body, if the headers promised one.
        while rest.len() < content_length {
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(read) => rest.extend_from_slice(&chunk[..read]),
            }
        }
        let body = String::from_utf8_lossy(&rest[..content_length.min(rest.len())]).to_string();

        let mut lines = head.lines();
        let request_line = lines.next().unwrap_or_default().to_string();
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or_default().to_string();
        let target = parts.next().unwrap_or_default().to_string();
        let path = vault_path_of(&target);

        self.trace.lock().unwrap().push(format!(
            "{method} {target} -> vault path {path:?}, body {content_length} bytes"
        ));

        let response = match method.as_str() {
            "PROPFIND" => self.propfind(&path, &head),
            "GET" => match self.stored(&path) {
                Some(content) => ok_response(200, "OK", &content),
                None => ok_response(404, "Not Found", "not found"),
            },
            "PUT" => {
                // Store the bytes. The GET that follows serves from this
                // store, so a client that verifies its write by reading it back
                // is reading what actually landed rather than an echo.
                self.files.lock().unwrap().insert(path.clone(), body);
                ok_response(201, "Created", "")
            }
            "MKCOL" => {
                self.dirs.lock().unwrap().insert(path.clone());
                ok_response(201, "Created", "")
            }
            "DELETE" => {
                let removed = self.files.lock().unwrap().remove(&path).is_some();
                if removed {
                    ok_response(204, "No Content", "")
                } else {
                    ok_response(404, "Not Found", "not found")
                }
            }
            _ => ok_response(405, "Method Not Allowed", "not allowed"),
        };
        let _ = stream.write_all(response.as_bytes());
        let _ = stream.flush();
    }

    /// A PROPFIND answer listing the collection's members.
    ///
    /// The status is **207 Multi-Status**, which is the only status RFC 4918
    /// defines for a PROPFIND answer and the only one the backend accepts: it
    /// refuses a `2xx` that is not a multistatus document, because a `200` with
    /// a body the client cannot parse means the URL is not serving WebDAV.
    fn propfind(&self, path: &str, head: &str) -> String {
        let depth = header_value(head, "depth").unwrap_or_else(|| "1".to_string());
        let dirs = self.dirs.lock().unwrap();
        let files = self.files.lock().unwrap();
        // A PROPFIND for a resource the server does not have is **404**, which
        // is what `exists` reads as "false". An empty 207 would say "yes, here
        // is a collection with no members", which reads as TRUE, and every
        // missing note would look present.
        if !dirs.contains(path) && !files.contains_key(path) {
            return ok_response(404, "Not Found", "not found");
        }
        let mut members: Vec<String> = Vec::new();
        // The requested resource itself. It is a collection only if the server
        // knows it as one — a PROPFIND aimed at a FILE must not answer "this is
        // a collection", or the backend's depth-first walk stats the file and
        // refuses it as one.
        if dirs.contains(path) {
            members.push(collection_response(path));
        } else if let Some(content) = files.get(path) {
            members.push(file_response(path, content.len()));
        }
        if depth != "0" {
            let prefix = if path.is_empty() {
                String::new()
            } else {
                format!("{path}/")
            };
            for (file_path, content) in files.iter() {
                if file_path.starts_with(&prefix) {
                    let rest = &file_path[prefix.len()..];
                    if !rest.contains('/') {
                        members.push(file_response(file_path, content.len()));
                    }
                }
            }
            // Child collections, so the depth-first walk has somewhere to go.
            for dir in dirs.iter() {
                if dir.is_empty() || !dir.starts_with(&prefix) {
                    continue;
                }
                let rest = &dir[prefix.len()..];
                if !rest.contains('/') {
                    members.push(collection_response(dir));
                }
            }
        }
        drop(dirs);
        drop(files);
        let body = format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?><d:multistatus xmlns:d=\"DAV:\">{}</d:multistatus>",
            members.join("")
        );
        format!(
            "HTTP/1.1 207 Multi-Status\r\nContent-Length: {}\r\nContent-Type: application/xml\r\n\
             Connection: close\r\n\r\n{body}",
            body.len()
        )
    }
}

/// The response body for a collection member.
///
/// `<d:resourcetype>` holding a `<d:collection/>` child is what makes a
/// resource a collection; the backend reads the CHILD, not the element's mere
/// presence, because an empty `<d:resourcetype/>` and an absent property are
/// the same shape to it.
///
/// The href is PERCENT-ENCODED, the way a real server emits it. A server that
/// emitted a decoded href would be agreeing with the client by construction on
/// an encoding the client has to undo, and a note whose name contains a space
/// or a `#` would never round-trip — so the round trip has to be exercised
/// through a real encoding.
fn collection_response(path: &str) -> String {
    let href = encoded_href(path);
    format!(
        "<d:response><d:href>/dav/vault/{href}/</d:href>\
         <d:propstat><d:prop>\
         <d:resourcetype><d:collection/></d:resourcetype>\
         <d:getlastmodified>Tue, 01 Oct 2024 10:11:12 GMT</d:getlastmodified>\
         </d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>"
    )
}

/// The response body for a file member.
fn file_response(path: &str, size: usize) -> String {
    let href = encoded_href(path);
    format!(
        "<d:response><d:href>/dav/vault/{href}</d:href>\
         <d:propstat><d:prop>\
         <d:resourcetype/>\
         <d:getcontentlength>{size}</d:getcontentlength>\
         <d:getlastmodified>Tue, 01 Oct 2024 10:11:12 GMT</d:getlastmodified>\
         </d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>"
    )
}

/// A vault-relative path as an href: each segment percent-encoded.
///
/// This is the mirror of the client's `encode_segment`, and it is the point of
/// the fixture: the client encodes a request path and DECODES an href, and a
/// fake that decoded the request the same way the client decodes the response
/// would cancel out a mismatch. A name with a space, a `#` or a `%` only
/// survives if the two sides really are inverse.
fn encoded_href(path: &str) -> String {
    path.split('/')
        .map(encode_segment)
        .collect::<Vec<_>>()
        .join("/")
}

/// Percent-encode one path segment.
///
/// Encodes everything that is not unreserved, which is what a server does with
/// a name it has to put in a URL.
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

/// An HTTP response with a body.
///
/// `Connection: close` is not optional here. HTTP/1.1 connections are
/// keep-alive by default, so a server that answers once and closes the socket
/// without saying so leaves the client believing the connection is reusable:
/// the NEXT request goes onto a dead socket and fails as "error sending
/// request", with the request never reaching the server at all. That is
/// precisely the failure this fixture produced before the header was added.
fn ok_response(status: u16, reason: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nContent-Type: text/plain\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    )
}

/// The offset of the end of the headers.
fn find_double_crlf(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

/// A header's value, case-insensitively.
fn header_value(head: &str, name: &str) -> Option<String> {
    head.lines().skip(1).find_map(|line| {
        let (key, value) = line.split_once(':')?;
        if key.trim().eq_ignore_ascii_case(name) {
            Some(value.trim().to_string())
        } else {
            None
        }
    })
}

/// The vault-relative path a request target names.
///
/// The client percent-encodes each segment; this decodes them, because the
/// backend's own mapping is what is under test and the fake must not cancel out
/// a decoding mistake the way a matching encoder on both sides would.
fn vault_path_of(target: &str) -> String {
    let without_query = target.split('?').next().unwrap_or(target);
    let path = without_query
        .strip_prefix("/dav/vault")
        .unwrap_or(without_query)
        .trim_matches('/');
    let decoded: Vec<String> = path.split('/').map(percent_decode).collect();
    decoded.join("/")
}

/// Decode `%XX` escapes in a path segment.
fn percent_decode(segment: &str) -> String {
    let bytes = segment.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

// ---------------------------------------------------------------------------
// The stdio client
// ---------------------------------------------------------------------------

/// A spawned server driven over stdin/stdout.
struct StdioClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
    /// The server's stderr, drained on a thread and kept for a failure message.
    diagnostics: Arc<Mutex<String>>,
}

impl StdioClient {
    /// Spawn the binary with `env` and speak MCP to it over the pipes.
    fn start(env: &[(&str, &str)]) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_bases-mcp"));
        command.arg("--transport").arg("stdio");
        for (key, value) in env {
            command.env(key, value);
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("the binary spawns");
        let stdin = child.stdin.take().expect("stdin is piped");
        let stdout = BufReader::new(child.stdout.take().expect("stdout is piped"));
        // The server's stderr is read on a thread, because a pipe that nobody
        // drains fills and blocks the server. It is kept so a failure can say
        // what the server complained about, rather than "stdout closed".
        let stderr = child.stderr.take().expect("stderr is piped");
        let diagnostics = Arc::new(Mutex::new(String::new()));
        let sink = Arc::clone(&diagnostics);
        std::thread::spawn(move || {
            let reader = BufReader::new(stderr);
            for line in reader.lines().map_while(Result::ok) {
                sink.lock().unwrap().push_str(&line);
                sink.lock().unwrap().push('\n');
            }
        });
        Self {
            child,
            stdin,
            stdout,
            next_id: 0,
            diagnostics,
        }
    }

    /// What the server said on stderr.
    fn diagnostics(&self) -> String {
        self.diagnostics.lock().unwrap().clone()
    }

    /// Send one JSON-RPC message and read the answer.
    fn call(&mut self, method: &str, params: Json) -> Json {
        self.next_id += 1;
        let id = self.next_id;
        let message = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        writeln!(self.stdin, "{message}").expect("the server accepts a message");
        self.stdin.flush().expect("the message is flushed");
        loop {
            let mut line = String::new();
            let read = self
                .stdout
                .read_line(&mut line)
                .expect("the server answers");
            assert!(
                read > 0,
                "the server closed stdout before answering {method}. Its stderr:\n{}",
                self.diagnostics()
            );
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let answer: Json = serde_json::from_str(trimmed).unwrap_or_else(|error| {
                panic!("the answer to {method} is JSON: {error}: {trimmed}")
            });
            if answer.get("id").and_then(Json::as_u64) == Some(id) {
                return answer;
            }
        }
    }

    /// `initialize`, then `tools/list`.
    fn handshake(&mut self) -> Json {
        let initialized = self.call(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "cross-product-test", "version": "1"},
            }),
        );
        assert!(
            initialized.get("result").is_some(),
            "initialize failed: {initialized}"
        );
        self.call("notifications/initialized", json!({}));
        self.call("tools/list", json!({}))
    }

    /// Call a tool and return its result.
    fn tool(&mut self, name: &str, arguments: Json) -> Json {
        self.call("tools/call", json!({"name": name, "arguments": arguments}))
    }
}

impl Drop for StdioClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ---------------------------------------------------------------------------
// The HTTP client
// ---------------------------------------------------------------------------

/// A spawned server driven over Streamable HTTP.
struct HttpClient {
    child: Child,
    base_url: String,
    session: Option<String>,
    client: reqwest::Client,
}

impl HttpClient {
    /// Spawn the binary in HTTP mode and wait until it answers.
    fn start(env: &[(&str, &str)]) -> Self {
        let port = ephemeral_port();
        let mut command = Command::new(env!("CARGO_BIN_EXE_bases-mcp"));
        command
            .arg("--transport")
            .arg("http")
            .env("MCP_HTTP_HOST", "127.0.0.1")
            .env("MCP_HTTP_PORT", port.to_string());
        for (key, value) in env {
            command.env(key, value);
        }
        let mut child = command
            .stderr(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("the binary spawns");

        let bound = SocketAddr::from(([127, 0, 0, 1], port));
        let deadline = Instant::now() + STARTUP_TIMEOUT;
        loop {
            assert!(
                Instant::now() <= deadline,
                "the HTTP server did not start listening within {STARTUP_TIMEOUT:?}"
            );
            if std::net::TcpStream::connect_timeout(&bound, Duration::from_millis(100)).is_ok() {
                break;
            }
            assert!(
                child.try_wait().expect("the child is pollable").is_none(),
                "the HTTP server exited before it started listening"
            );
            std::thread::sleep(Duration::from_millis(50));
        }

        Self {
            child,
            base_url: format!("http://127.0.0.1:{port}/mcp"),
            session: None,
            client: reqwest::Client::new(),
        }
    }

    /// `initialize`, then `notifications/initialized`. The session id comes
    /// back on a header and is echoed on every later request.
    async fn handshake(&mut self) -> Json {
        let response = self
            .client
            .post(&self.base_url)
            .header("Accept", "application/json, text/event-stream")
            .header("Content-Type", "application/json")
            .body(
                serde_json::to_string(&json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": {
                        "protocolVersion": "2025-06-18",
                        "capabilities": {},
                        "clientInfo": {"name": "cross-product-test", "version": "1"},
                    },
                }))
                .expect("the payload serialises"),
            )
            .send()
            .await
            .expect("the server answers initialize");
        assert_eq!(
            response.status(),
            200,
            "initialize was refused: {}",
            response.status()
        );
        if let Some(session) = response.headers().get("Mcp-Session-Id") {
            self.session = Some(
                session
                    .to_str()
                    .expect("the session id is text")
                    .to_string(),
            );
        }
        let body = response.text().await.expect("the body is text");
        let answer = parse_sse_or_json(&body);
        assert!(
            answer.get("result").is_some(),
            "initialize failed: {answer}"
        );

        // The notification needs no answer, but the session header does.
        let _ = self
            .client
            .post(&self.base_url)
            .header("Accept", "application/json, text/event-stream")
            .header("Content-Type", "application/json")
            .header("Mcp-Session-Id", self.session.clone().unwrap_or_default())
            .body(
                serde_json::to_string(&json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/initialized",
                }))
                .expect("the payload serialises"),
            )
            .send()
            .await
            .expect("the server accepts the notification");

        self.post(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}))
            .await
    }

    /// Post one request and return the parsed answer.
    async fn post(&self, payload: Json) -> Json {
        let mut request = self
            .client
            .post(&self.base_url)
            .header("Accept", "application/json, text/event-stream")
            .header("Content-Type", "application/json")
            .body(serde_json::to_string(&payload).expect("the payload serialises"));
        if let Some(session) = &self.session {
            request = request.header("Mcp-Session-Id", session);
        }
        let response = request.send().await.expect("the server answers");
        let body = response.text().await.expect("the body is text");
        parse_sse_or_json(&body)
    }

    /// Call a tool and return its answer.
    async fn tool(&self, name: &str, arguments: Json) -> Json {
        self.post(json!({
            "jsonrpc": "2.0",
            "id": 99,
            "method": "tools/call",
            "params": {"name": name, "arguments": arguments},
        }))
        .await
    }
}

impl Drop for HttpClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Parse an answer that is either plain JSON or an SSE event carrying JSON.
fn parse_sse_or_json(body: &str) -> Json {
    let trimmed = body.trim();
    if trimmed.starts_with('{') {
        return serde_json::from_str(trimmed).expect("the body is JSON");
    }
    for line in trimmed.lines() {
        if let Some(payload) = line.strip_prefix("data:") {
            let payload = payload.trim();
            if !payload.is_empty() {
                return serde_json::from_str(payload).expect("the SSE payload is JSON");
            }
        }
    }
    panic!("no JSON-RPC answer in: {trimmed}");
}

/// Whether a tool answer says the write succeeded.
///
/// A tool result carries `isError` rather than a protocol error, so a failed
/// write is a SUCCESSFUL JSON-RPC call whose content says it failed. Checking
/// only for a `result` would let a refusal pass as a write.
fn written_ok(answer: &Json) -> bool {
    answer.get("result").is_some() && !answer["result"]["isError"].as_bool().unwrap_or(false)
}

// ---------------------------------------------------------------------------
// The cells
// ---------------------------------------------------------------------------

/// A note the fixture vault holds.
const NOTE: &str = "Cross Product Note";
const NOTE_BODY: &str = "---\nstatus: open\n---\n\n# Cross Product Note\n\nA note.\n";
const BASE: &str = "Cross.base";

/// A base that selects the note.
///
/// The filter is on a PROPERTY, not on the file name, and that is not a
/// detail. `add_note_to_base` creates a note whose name the agent chooses, so
/// a base filtered on `file.name` could only ever be satisfied by a note that
/// already existed — the create path would be unreachable. A property filter is
/// what makes the create path meaningful: any note carrying `status: open`
/// matches, whatever it is called.
fn base_yaml() -> String {
    "filters:\n  and:\n    - status == \"open\"\nviews:\n  - type: table\n    name: All\n    order:\n      - file.name\n      - status\n"
        .to_string()
}

/// A note whose NAME needs percent-encoding, so the WebDAV round trip is
/// actually exercised.
///
/// The client's `encode_segment` and its href `decode_segment` are inverses,
/// and the only way to know they are is to send a name that has to survive
/// both. A space encodes to `%20`; a `#` encodes to `%23`; a `%` encodes to
/// `%25` — and a decode that ran over the path's separators instead of per
/// segment would mangle the last one. `Q1 #2.md` and `50%.md` are both
/// ordinary note names and both impossible to get right by accident.
const TRICKY_NAME: &str = "Q1 #2 50%.md";

/// The files a fixture vault holds, for either backend.
fn fixture_files() -> Vec<(&'static str, String)> {
    vec![
        ("Cross Product Note.md", NOTE_BODY.to_string()),
        (TRICKY_NAME, NOTE_BODY.to_string()),
        ("Cross.base", base_yaml()),
    ]
}

/// Write a file into a fresh temporary directory and return the directory.
fn fixture_dir(files: &[(&str, String)]) -> PathBuf {
    let dir = tempfile::tempdir().expect("a temporary directory");
    for (name, content) in files {
        let path = dir.path().join(name);
        std::fs::write(&path, content).expect("the fixture file is written");
    }
    // The temporary directory must outlive this function, so it is kept rather
    // than dropped: `tempfile` deletes the directory when it is dropped.
    dir.keep()
}

// ---------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------

/// A filesystem vault over stdio: read the base, then write a note.
#[test]
fn stdio_serves_a_filesystem_vault_and_writes_to_it() {
    let dir = fixture_dir(&fixture_files());
    let mut client = StdioClient::start(&[("BASES_MCP_VAULT", dir.to_str().unwrap())]);

    let tools = client.handshake();
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .expect("tools/list returns an array")
        .iter()
        .map(|tool| tool["name"].as_str().expect("a tool has a name"))
        .collect();
    assert_eq!(names.len(), 6, "the surface is six tools: {names:?}");

    // READ: the base resolves and names the note.
    let resolved = client.tool("resolve_base", json!({"base": BASE, "view": "All"}));
    let text = resolved["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    assert!(
        text.contains(NOTE),
        "the resolved base should name the note: {text}"
    );

    // WRITE, half one: EDIT an existing note. `write_note` is an EDIT — it
    // refuses a path that is not there, which is the property that stops a
    // typo from creating a stray file.
    let edited = client.tool(
        "write_note",
        json!({
            "path": "Cross Product Note.md",
            "content": "# Cross Product Note\n\nEdited through the stdio transport.\n",
        }),
    );
    assert!(
        written_ok(&edited),
        "write_note over stdio failed: {edited}"
    );
    let on_disk = std::fs::read_to_string(dir.join("Cross Product Note.md"))
        .expect("the note is on disk after the write");
    assert!(
        on_disk.contains("Edited through the stdio transport."),
        "the note on disk is not what was written: {on_disk}"
    );

    // WRITE, half two: CREATE a note that matches the base. The handshake is
    // two calls: the first proposes, the second commits.
    let proposed = client.tool(
        "add_note_to_base",
        json!({"base": BASE, "path": "Created Over Stdio.md", "view": "All"}),
    );
    assert!(
        written_ok(&proposed),
        "add_note_to_base over stdio failed to propose: {proposed}"
    );
    let draft_id = proposed["result"]["structuredContent"]["draft_id"]
        .as_str()
        .expect("the proposal carries a draft id")
        .to_string();
    let committed = client.tool(
        "add_note_to_base",
        json!({
            "draft_id": draft_id,
            "content": "---\nstatus: open\n---\n\n# Created Over Stdio\n\nCreated through the stdio transport.\n",
        }),
    );
    assert!(
        written_ok(&committed),
        "add_note_to_base over stdio failed to commit: {committed}"
    );
    let created = std::fs::read_to_string(dir.join("Created Over Stdio.md"))
        .expect("the created note is on disk");
    assert!(
        created.contains("Created through the stdio transport."),
        "the created note is not what was written: {created}"
    );

    // WRITE, half three: a note in a SUBDIRECTORY. This is the path that
    // exercises `ensure_dir` -- the filesystem has to create the directory,
    // and the WebDAV backend has to MKCOL it. A flat write never touches
    // that code, so a suite that only writes at the root leaves the whole
    // "does a new folder appear" question untested.
    //
    // It is the CREATE path that creates a directory, not `write_note`:
    // `write_note` is an edit and refuses a path that is not there, which is
    // exactly what a note in a new folder is.
    let nested_proposal = client.tool(
        "add_note_to_base",
        json!({
            "base": BASE,
            "path": "Nested Folder/Nested Note.md",
            "view": "All",
        }),
    );
    assert!(
        written_ok(&nested_proposal),
        "add_note_to_base into a subdirectory over stdio failed to propose: {nested_proposal}"
    );
    let nested_draft = nested_proposal["result"]["structuredContent"]["draft_id"]
        .as_str()
        .expect("the proposal carries a draft id")
        .to_string();
    let nested_commit = client.tool(
        "add_note_to_base",
        json!({
            "draft_id": nested_draft,
            "content": "---\nstatus: open\n---\n\n# Nested Note\n\nWritten into a subdirectory.\n",
        }),
    );
    assert!(
        written_ok(&nested_commit),
        "add_note_to_base into a subdirectory over stdio failed to commit: {nested_commit}"
    );
    let nested_on_disk = std::fs::read_to_string(dir.join("Nested Folder/Nested Note.md"))
        .expect("the nested note is on disk after the write");
    assert!(
        nested_on_disk.contains("Written into a subdirectory."),
        "the nested note is not what was written: {nested_on_disk}"
    );
    assert!(
        dir.join("Nested Folder").is_dir(),
        "the directory the note went into must exist"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// A WebDAV vault over stdio: read the base, then write a note.
#[test]
fn stdio_serves_a_webdav_vault_and_writes_to_it() {
    let server = DavServer::start(&fixture_files());
    let mut client = StdioClient::start(&[
        ("BASES_MCP_WEBDAV_URL", &server.base_url),
        ("BASES_MCP_WEBDAV_USER", "u"),
        ("BASES_MCP_WEBDAV_PASSWORD", "p"),
    ]);

    let tools = client.handshake();
    assert!(
        tools["result"]["tools"].is_array(),
        "tools/list over stdio against WebDAV failed: {tools}"
    );

    // READ: the base resolves against the WebDAV vault.
    let resolved = client.tool("resolve_base", json!({"base": BASE, "view": "All"}));
    let text = resolved["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    assert!(
        text.contains(NOTE),
        "the resolved base should name the note: {text}"
    );

    // WRITE, half one: EDIT an existing note, and check it at the far end —
    // the server's own store, not the client's belief.
    let edited = client.tool(
        "write_note",
        json!({
            "path": "Cross Product Note.md",
            "content": "# Cross Product Note\n\nEdited through the stdio transport.\n",
        }),
    );
    assert!(
        written_ok(&edited),
        "write_note over stdio against WebDAV failed: {edited}\nserver saw:\n{}",
        server.trace().join("\n")
    );
    let stored = server
        .read_back("Cross Product Note.md")
        .expect("the note is readable back from the WebDAV server");
    assert!(
        stored.contains("Edited through the stdio transport."),
        "the stored note is not what was written: {stored}"
    );

    // WRITE, half two: CREATE a note that matches the base.
    let proposed = client.tool(
        "add_note_to_base",
        json!({"base": BASE, "path": "Created Over Stdio Webdav.md", "view": "All"}),
    );
    assert!(
        written_ok(&proposed),
        "add_note_to_base over stdio against WebDAV failed to propose: {proposed}"
    );
    let draft_id = proposed["result"]["structuredContent"]["draft_id"]
        .as_str()
        .expect("the proposal carries a draft id")
        .to_string();
    let committed = client.tool(
        "add_note_to_base",
        json!({
            "draft_id": draft_id,
            "content": "---\nstatus: open\n---\n\n# Created Over Stdio Webdav\n\nCreated through the stdio transport.\n",
        }),
    );
    assert!(
        written_ok(&committed),
        "add_note_to_base over stdio against WebDAV failed to commit: {committed}"
    );
    let created = server
        .read_back("Created Over Stdio Webdav.md")
        .expect("the created note is readable back from the WebDAV server");
    assert!(
        created.contains("Created through the stdio transport."),
        "the created note is not what was written: {created}"
    );

    // WRITE, half three: a note in a SUBDIRECTORY, which is the path that
    // exercises `ensure_dir` -> MKCOL. The server must end up holding BOTH
    // the collection and the note inside it.
    //
    // It is the CREATE path that creates the collection, not `write_note`:
    // `write_note` is an edit and refuses a path that is not there.
    let nested_proposal = client.tool(
        "add_note_to_base",
        json!({
            "base": BASE,
            "path": "Nested Folder/Nested Note.md",
            "view": "All",
        }),
    );
    assert!(
        written_ok(&nested_proposal),
        "add_note_to_base into a subdirectory over stdio against WebDAV failed to propose: {nested_proposal}"
    );
    let nested_draft = nested_proposal["result"]["structuredContent"]["draft_id"]
        .as_str()
        .expect("the proposal carries a draft id")
        .to_string();
    let nested_commit = client.tool(
        "add_note_to_base",
        json!({
            "draft_id": nested_draft,
            "content": "---\nstatus: open\n---\n\n# Nested Note\n\nWritten into a subdirectory.\n",
        }),
    );
    assert!(
        written_ok(&nested_commit),
        "add_note_to_base into a subdirectory over stdio against WebDAV failed to commit: {nested_commit}"
    );
    let nested_stored = server
        .read_back("Nested Folder/Nested Note.md")
        .expect("the nested note is readable back from the WebDAV server");
    assert!(
        nested_stored.contains("Written into a subdirectory."),
        "the nested stored note is not what was written: {nested_stored}"
    );
}

/// A filesystem vault over HTTP: read the base, then write a note.
#[tokio::test]
async fn http_serves_a_filesystem_vault_and_writes_to_it() {
    let dir = fixture_dir(&fixture_files());
    let mut client = HttpClient::start(&[("BASES_MCP_VAULT", dir.to_str().unwrap())]);

    let tools = client.handshake().await;
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .expect("tools/list returns an array")
        .iter()
        .map(|tool| tool["name"].as_str().expect("a tool has a name"))
        .collect();
    assert_eq!(names.len(), 6, "the surface is six tools: {names:?}");

    // READ
    let resolved = client
        .tool("resolve_base", json!({"base": BASE, "view": "All"}))
        .await;
    let text = resolved["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    assert!(
        text.contains(NOTE),
        "the resolved base should name the note: {text}"
    );

    // WRITE, half one: EDIT an existing note.
    let edited = client
        .tool(
            "write_note",
            json!({
                "path": "Cross Product Note.md",
                "content": "# Cross Product Note\n\nEdited through the HTTP transport.\n",
            }),
        )
        .await;
    assert!(written_ok(&edited), "write_note over HTTP failed: {edited}");
    let on_disk = std::fs::read_to_string(dir.join("Cross Product Note.md"))
        .expect("the note is on disk after the write");
    assert!(
        on_disk.contains("Edited through the HTTP transport."),
        "the note on disk is not what was written: {on_disk}"
    );

    // WRITE, half two: CREATE a note that matches the base.
    let proposed = client
        .tool(
            "add_note_to_base",
            json!({"base": BASE, "path": "Created Over Http.md", "view": "All"}),
        )
        .await;
    assert!(
        written_ok(&proposed),
        "add_note_to_base over HTTP failed to propose: {proposed}"
    );
    let draft_id = proposed["result"]["structuredContent"]["draft_id"]
        .as_str()
        .expect("the proposal carries a draft id")
        .to_string();
    let committed = client
        .tool(
            "add_note_to_base",
            json!({
                "draft_id": draft_id,
                "content": "---\nstatus: open\n---\n\n# Created Over Http\n\nCreated through the HTTP transport.\n",
            }),
        )
        .await;
    assert!(
        written_ok(&committed),
        "add_note_to_base over HTTP failed to commit: {committed}"
    );
    let created = std::fs::read_to_string(dir.join("Created Over Http.md"))
        .expect("the created note is on disk");
    assert!(
        created.contains("Created through the HTTP transport."),
        "the created note is not what was written: {created}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// A WebDAV vault over HTTP: read the base, then write a note.
#[tokio::test]
async fn http_serves_a_webdav_vault_and_writes_to_it() {
    let server = DavServer::start(&fixture_files());
    let mut client = HttpClient::start(&[
        ("BASES_MCP_WEBDAV_URL", &server.base_url),
        ("BASES_MCP_WEBDAV_USER", "u"),
        ("BASES_MCP_WEBDAV_PASSWORD", "p"),
    ]);

    let tools = client.handshake().await;
    assert!(
        tools["result"]["tools"].is_array(),
        "tools/list over HTTP against WebDAV failed: {tools}"
    );

    // READ
    let resolved = client
        .tool("resolve_base", json!({"base": BASE, "view": "All"}))
        .await;
    let text = resolved["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    assert!(
        text.contains(NOTE),
        "the resolved base should name the note: {text}"
    );

    // WRITE, half one: EDIT an existing note, checked at the far end.
    let edited = client
        .tool(
            "write_note",
            json!({
                "path": "Cross Product Note.md",
                "content": "# Cross Product Note\n\nEdited through the HTTP transport.\n",
            }),
        )
        .await;
    assert!(
        written_ok(&edited),
        "write_note over HTTP against WebDAV failed: {edited}"
    );
    let stored = server
        .read_back("Cross Product Note.md")
        .expect("the note is readable back from the WebDAV server");
    assert!(
        stored.contains("Edited through the HTTP transport."),
        "the stored note is not what was written: {stored}"
    );

    // WRITE, half two: CREATE a note that matches the base.
    let proposed = client
        .tool(
            "add_note_to_base",
            json!({"base": BASE, "path": "Created Over Http Webdav.md", "view": "All"}),
        )
        .await;
    assert!(
        written_ok(&proposed),
        "add_note_to_base over HTTP against WebDAV failed to propose: {proposed}"
    );
    let draft_id = proposed["result"]["structuredContent"]["draft_id"]
        .as_str()
        .expect("the proposal carries a draft id")
        .to_string();
    let committed = client
        .tool(
            "add_note_to_base",
            json!({
                "draft_id": draft_id,
                "content": "---\nstatus: open\n---\n\n# Created Over Http Webdav\n\nCreated through the HTTP transport.\n",
            }),
        )
        .await;
    assert!(
        written_ok(&committed),
        "add_note_to_base over HTTP against WebDAV failed to commit: {committed}"
    );
    let created = server
        .stored("Created Over Http Webdav.md")
        .expect("the created note reached the WebDAV server");
    assert!(
        created.contains("Created through the HTTP transport."),
        "the created note is not what was written: {created}"
    );
}

/// A note whose name needs percent-encoding survives the round trip.
///
/// Both backends: the filesystem stores the name as given, and the WebDAV
/// backend encodes it into a URL, stores it, and decodes it back out of the
/// href the server sends. A client that encoded wrongly, or a server that
/// emitted a decoded href, would fail here — and would pass every other test
/// in this file, because every other note name encodes to itself.
#[test]
fn a_note_whose_name_needs_encoding_round_trips_through_both_backends() {
    // The filesystem backend.
    let dir = fixture_dir(&fixture_files());
    let mut stdio = StdioClient::start(&[("BASES_MCP_VAULT", dir.to_str().unwrap())]);
    let tools = stdio.handshake();
    assert!(tools["result"]["tools"].is_array(), "tools/list failed");

    let listed = stdio.tool("list_bases", json!({}));
    assert!(
        listed.get("result").is_some(),
        "list_bases over stdio failed: {listed}"
    );

    // The note is readable BY ITS TRICKY NAME.
    let read = stdio.tool("get_note", json!({"path": TRICKY_NAME}));
    assert!(
        read.get("result").is_some(),
        "get_note of {TRICKY_NAME} over stdio failed: {read}"
    );
    assert!(
        !read["result"]["isError"].as_bool().unwrap_or(false),
        "get_note of {TRICKY_NAME} reported an error: {read}"
    );
    assert!(
        read["result"]["structuredContent"]["path"]
            .as_str()
            .unwrap_or_default()
            == TRICKY_NAME,
        "the note came back under the wrong name: {read}"
    );
    std::fs::remove_dir_all(&dir).ok();

    // The WebDAV backend: the same name, encoded into a URL and decoded back.
    let server = DavServer::start(&fixture_files());
    let mut webdav = StdioClient::start(&[
        ("BASES_MCP_WEBDAV_URL", &server.base_url),
        ("BASES_MCP_WEBDAV_USER", "u"),
        ("BASES_MCP_WEBDAV_PASSWORD", "p"),
    ]);
    webdav.handshake();
    let read = webdav.tool("get_note", json!({"path": TRICKY_NAME}));
    assert!(
        read.get("result").is_some(),
        "get_note of {TRICKY_NAME} over stdio against WebDAV failed: {read}"
    );
    assert!(
        !read["result"]["isError"].as_bool().unwrap_or(false),
        "get_note of {TRICKY_NAME} against WebDAV reported an error: {read}"
    );
    assert!(
        read["result"]["structuredContent"]["path"]
            .as_str()
            .unwrap_or_default()
            == TRICKY_NAME,
        "the note came back under the wrong name: {read}"
    );
}

/// `write_note` refuses a path that is not there, in every cell.
///
/// The refusal is load-bearing: it is what stops a typo from creating a stray
/// file, and it is what makes `write_note` an EDIT rather than a create. A
/// suite that only writes at paths it created first would never notice if that
/// stopped being true.
#[test]
fn write_note_refuses_a_path_that_is_not_there() {
    let dir = fixture_dir(&fixture_files());
    let mut stdio = StdioClient::start(&[("BASES_MCP_VAULT", dir.to_str().unwrap())]);
    stdio.handshake();

    let refused = stdio.tool(
        "write_note",
        json!({
            "path": "Never Existed.md",
            "content": "# Never Existed\n\nShould not be written.\n",
        }),
    );
    assert!(
        refused.get("result").is_some(),
        "write_note of a missing note failed at the protocol level: {refused}"
    );
    assert!(
        refused["result"]["isError"].as_bool().unwrap_or(false),
        "write_note of a missing note should be an error, not a success: {refused}"
    );
    assert!(
        !dir.join("Never Existed.md").exists(),
        "the refused write must not have created the note"
    );

    // And the same over the WebDAV backend.
    let server = DavServer::start(&fixture_files());
    let mut webdav = StdioClient::start(&[
        ("BASES_MCP_WEBDAV_URL", &server.base_url),
        ("BASES_MCP_WEBDAV_USER", "u"),
        ("BASES_MCP_WEBDAV_PASSWORD", "p"),
    ]);
    webdav.handshake();
    let refused = webdav.tool(
        "write_note",
        json!({
            "path": "Never Existed.md",
            "content": "# Never Existed\n\nShould not be written.\n",
        }),
    );
    assert!(
        refused["result"]["isError"].as_bool().unwrap_or(false),
        "write_note of a missing note against WebDAV should be an error: {refused}"
    );
    assert!(
        server.read_back("Never Existed.md").is_none(),
        "the refused write must not have reached the WebDAV server"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// The two transports serve the SAME vault and produce the same answer.
///
/// The axes are independent, and this is the property that says so: one vault,
/// two clients, one answer.
#[test]
fn both_transports_serve_the_same_filesystem_vault_identically() {
    let dir = fixture_dir(&fixture_files());
    let vault = dir.to_str().unwrap().to_string();

    let mut stdio = StdioClient::start(&[("BASES_MCP_VAULT", &vault)]);
    let stdio_tools = stdio.handshake();
    let stdio_resolved = stdio.tool("resolve_base", json!({"base": BASE, "view": "All"}));

    let runtime = tokio::runtime::Runtime::new().expect("a tokio runtime");
    let http = runtime.block_on(async {
        let mut client = HttpClient::start(&[("BASES_MCP_VAULT", &vault)]);
        let tools = client.handshake().await;
        let resolved = client
            .tool("resolve_base", json!({"base": BASE, "view": "All"}))
            .await;
        (tools, resolved)
    });

    assert_eq!(
        stdio_tools["result"]["tools"], http.0["result"]["tools"],
        "the two transports must serve the same tool surface"
    );
    assert_eq!(
        stdio_resolved["result"]["content"], http.1["result"]["content"],
        "the two transports must resolve the same base to the same answer"
    );

    std::fs::remove_dir_all(&dir).ok();
}
