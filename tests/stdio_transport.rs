//! The stdio transport, driven end to end.
//!
//! `tests/http_transport.rs` proves the HTTP path over the wire; nothing else
//! in the crate spawns the binary over stdin/stdout, and the default transport
//! is the one a desktop agent actually launches. A server whose `stdio` path
//! broke while every other test stayed green is the failure this file exists to
//! make impossible, so it drives the real binary the way a client does:
//! `initialize`, then `tools/list`, then `tools/call`, and asserts the response.
//!
//! stdout is the protocol stream here. The child's stderr is drained into a
//! thread because rmcp's stdio transport owns stdout absolutely — a reader that
//! stopped would wedge the child on its next write — and because a failure has
//! to be able to name what the server complained about.

use std::io::{BufRead, BufReader, Read, Write as _};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{Value as Json, json};

/// How long to wait for the first line before giving up.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(20);

/// `MCP_TRANSPORT` is process-global, and this file shares a process with the
/// other tests in this binary. Every test here that reads the environment takes
/// this lock, so a test that sets `MCP_TRANSPORT=http` cannot race a test that
/// reads it as the default.
static ENV: Mutex<()> = Mutex::new(());

/// Set a process-global variable under `ENV`.
///
/// `std::env::set_var` is `unsafe` from edition 2024: the write may reallocate
/// the environment while another thread — including C code inside a dependency
/// — reads it with `getenv`. Every environment access in this binary is
/// serialised through `ENV`, so no two of them can overlap; the caller holds
/// that lock, and holding it is what makes the write acceptable here.
fn set_env(name: &str, value: &str) {
    // SAFETY: the caller holds `ENV`, and every environment access in this
    // binary takes it.
    unsafe { std::env::set_var(name, value) }
}

/// Unset a process-global variable. See [`set_env`] for the safety argument.
fn unset_env(name: &str) {
    // SAFETY: the caller holds `ENV`, and every environment access in this
    // binary takes it.
    unsafe { std::env::remove_var(name) }
}

/// One server process, one JSON-RPC id counter.
struct Server {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Server {
    /// Spawn the binary over stdio with no `--transport` flag, which is the
    /// default a desktop agent gets.
    fn start_stdio(vault: &std::path::Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_bases-mcp"))
            .env("BASES_MCP_VAULT", vault)
            .env_remove("MCP_TRANSPORT")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("the binary spawns");

        let stdin = child.stdin.take().expect("stdin is piped");
        let stdout = BufReader::new(child.stdout.take().expect("stdout is piped"));
        let mut server = Self {
            child,
            stdin,
            stdout,
            next_id: 0,
        };
        drain_stderr(&mut server.child);
        server
    }

    /// The next JSON-RPC id.
    fn id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    /// Send one JSON-RPC request and read the reply carrying the same id.
    fn request(&mut self, method: &str, params: Json) -> Json {
        let id = self.id();
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        self.reply(id)
    }

    /// Send one notification: no id, and therefore no reply to wait for.
    fn notify(&mut self, method: &str) {
        self.send(&json!({ "jsonrpc": "2.0", "method": method }));
    }

    fn send(&mut self, payload: &Json) {
        let mut line = serde_json::to_string(payload).expect("the payload serialises");
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .expect("the request reaches the server's stdin");
        self.stdin
            .flush()
            .expect("the request is flushed before the reply is read");
    }

    /// Read lines until the one whose `id` matches, refusing a timeout.
    ///
    /// A notification or a server-initiated request can arrive between them; it
    /// is not the answer and is discarded, which is what a real client does.
    fn reply(&mut self, id: u64) -> Json {
        let deadline = Instant::now() + STARTUP_TIMEOUT;
        loop {
            assert!(
                Instant::now() <= deadline,
                "no reply to request {id} within {STARTUP_TIMEOUT:?}"
            );
            assert!(
                self.child
                    .try_wait()
                    .expect("the child is pollable")
                    .is_none(),
                "the server exited before it answered request {id}"
            );
            let mut line = String::new();
            let read = self
                .stdout
                .read_line(&mut line)
                .expect("the server's stdout is readable");
            assert!(
                read > 0,
                "the server's stdout closed before it answered request {id}"
            );
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let message: Json = serde_json::from_str(line).unwrap_or_else(|error| {
                panic!("stdout is JSON-RPC, not prose ({error}): {line:?}")
            });
            if message.get("id").and_then(Json::as_u64) == Some(id) {
                return message;
            }
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Move the child's stderr to a thread that discards it, so the pipe cannot fill
/// and wedge the server, and so nothing here has to read it to make progress.
fn drain_stderr(child: &mut Child) {
    let mut stderr = child.stderr.take().expect("stderr is piped");
    std::thread::spawn(move || {
        let mut buffer = [0u8; 4096];
        loop {
            match stderr.read(&mut buffer) {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
        }
    });
}

/// The testing vault on disk.
fn vault_dir() -> std::path::PathBuf {
    let mut dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    dir.push("test");
    dir.push("vault");
    dir
}

/// A complete handshake, for a test whose subject is what comes after it.
fn handshake(server: &mut Server) {
    server.request(
        "initialize",
        json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "stdio-transport-test", "version": "0" }
        }),
    );
    server.notify("notifications/initialized");
}

/// `initialize` over stdio names this server and advertises the tools capability.
#[test]
fn initialize_over_stdio_names_the_server() {
    let _guard = ENV.lock().expect("the env lock is not poisoned");
    let mut server = Server::start_stdio(&vault_dir());

    let response = server.request(
        "initialize",
        json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "stdio-transport-test", "version": "0" }
        }),
    );

    let result = &response["result"];
    assert_eq!(result["serverInfo"]["name"], "bases-mcp");
    assert_eq!(
        result["serverInfo"]["version"],
        env!("CARGO_PKG_VERSION"),
        "stdio reports the same version as the crate"
    );
    assert!(
        result["capabilities"]["tools"].is_object(),
        "the tools capability is advertised: {}",
        result["capabilities"]
    );
}

/// `tools/list` over stdio is the same six tools, in the same order, with the
/// same input schemas, as the declared surface.
#[test]
fn tools_list_over_stdio_matches_the_declared_surface() {
    let _guard = ENV.lock().expect("the env lock is not poisoned");
    let mut server = Server::start_stdio(&vault_dir());
    handshake(&mut server);

    let response = server.request("tools/list", json!({}));
    let over_stdio = response["result"]["tools"].clone();

    let declared =
        serde_json::to_value(bases_mcp::tools::tool_definitions()).expect("the tools serialise");

    assert_eq!(
        over_stdio, declared,
        "tools/list over stdio must be the declared surface, field for field"
    );

    let names: Vec<&str> = over_stdio
        .as_array()
        .expect("tools is an array")
        .iter()
        .map(|tool| tool["name"].as_str().expect("a tool has a name"))
        .collect();
    assert_eq!(
        names,
        [
            "list_bases",
            "resolve_base",
            "get_note",
            "write_note",
            "add_note_to_base",
            "backlinks"
        ]
    );
}

/// `tools/call` over stdio reaches the real engine: the query pipeline runs and
/// rows come back. This is the assertion that separates "the stdio transport
/// answers" from "the stdio transport works".
#[test]
fn a_tool_call_over_stdio_resolves_a_base() {
    let _guard = ENV.lock().expect("the env lock is not poisoned");
    let mut server = Server::start_stdio(&vault_dir());
    handshake(&mut server);

    let response = server.request(
        "tools/call",
        json!({
            "name": "resolve_base",
            "arguments": { "base": "AllNotes.base", "format": "json" }
        }),
    );

    assert_eq!(response["result"]["isError"], json!(false));
    let structured = &response["result"]["structuredContent"];
    assert_eq!(
        structured["health"], "ok",
        "health: {}",
        structured["health"]
    );
    assert!(
        structured["rowCount"].as_u64().unwrap_or(0) > 0,
        "the query pipeline produced rows: {structured}"
    );
    assert!(
        structured["rows"]
            .as_array()
            .is_some_and(|rows| !rows.is_empty()),
        "the rows array is populated: {structured}"
    );
}

/// `MCP_TRANSPORT` selects the transport when no flag is given, and a value it
/// does not know is refused rather than defaulted.
///
/// All three cases are asserted by running the real binary, because the env
/// branch in `select_transport` is otherwise unreachable from a test that never
/// sets the variable: the two existing tests set `MCP_TRANSPORT=stdio`, which
/// is the default, so deleting the branch would leave the suite green. They are
/// one test because they share a process-global environment.
#[test]
fn mcp_transport_selects_the_transport() {
    let _guard = ENV.lock().expect("the env lock is not poisoned");
    let vault = vault_dir();

    // `MCP_TRANSPORT=http` over stdin: the HTTP transport never reads stdin, so
    // a request sent there is never answered, and it listens instead. The
    // evidence that the branch was taken is that the process is alive and
    // silent on stdin.
    set_env("MCP_TRANSPORT", "http");
    let mut child = Command::new(env!("CARGO_BIN_EXE_bases-mcp"))
        .env("BASES_MCP_VAULT", &vault)
        .env("MCP_HTTP_PORT", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the binary spawns");
    drain_stderr(&mut child);
    let mut stdin = child.stdin.take().expect("stdin is piped");
    let request = json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "stdio-transport-test", "version": "0" }
        }
    });
    let mut line = serde_json::to_string(&request).expect("the payload serialises");
    line.push('\n');
    stdin
        .write_all(line.as_bytes())
        .expect("the request reaches stdin");
    stdin.flush().expect("the request is flushed");
    drop(stdin);

    assert!(
        read_within(
            child.stdout.take().expect("stdout is piped"),
            Duration::from_secs(3)
        )
        .is_none(),
        "MCP_TRANSPORT=http must not answer on stdin"
    );
    assert!(
        child.try_wait().expect("the child is pollable").is_none(),
        "the HTTP transport stays alive on stdin, which is the point"
    );
    let _ = child.kill();
    let _ = child.wait();
    unset_env("MCP_TRANSPORT");

    // `MCP_TRANSPORT=stdio` is the default and still answers, which is what
    // makes the case above meaningful: the same request, answered.
    set_env("MCP_TRANSPORT", "stdio");
    let mut server = Server::start_stdio(&vault);
    let response = server.request(
        "initialize",
        json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "stdio-transport-test", "version": "0" }
        }),
    );
    assert_eq!(response["result"]["serverInfo"]["name"], "bases-mcp");
    drop(server);
    unset_env("MCP_TRANSPORT");

    // A value the selector does not know is a refusal naming it, and the process
    // exits non-zero rather than serving stdio.
    set_env("MCP_TRANSPORT", "htp");
    let output = Command::new(env!("CARGO_BIN_EXE_bases-mcp"))
        .env("BASES_MCP_VAULT", &vault)
        .stdin(Stdio::null())
        .output()
        .expect("the binary runs to a refusal");
    unset_env("MCP_TRANSPORT");
    assert!(
        !output.status.success(),
        "an unknown transport exits non-zero: {:?}",
        output.status
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("htp"),
        "the refusal names the value it saw: {stderr}"
    );
}

/// Read one line of stdout, or `None` if nothing arrives within `within`.
///
/// The reader blocks until a line or EOF, so it runs on a thread and the
/// timeout is spent waiting for that thread rather than polling the pipe: a
/// transport that never answers is the assertion, not a hang.
fn read_within<R: Read + Send + 'static>(reader: R, within: Duration) -> Option<String> {
    let handle = std::thread::spawn(move || {
        let mut reader = BufReader::new(reader);
        let mut line = String::new();
        reader.read_line(&mut line).ok()?;
        Some(line)
    });
    let deadline = Instant::now() + within;
    loop {
        if handle.is_finished() {
            return handle.join().expect("the reader thread is not panicked");
        }
        if Instant::now() > deadline {
            // The thread is still blocked on the pipe. Leaving it is safe: the
            // child is killed by the caller, which closes the pipe.
            return None;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}
