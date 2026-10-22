//! The HTTP transport, driven end to end.
//!
//! This suite exists because the transport is the part of the server a unit
//! test cannot see: `dispatch` covers the tools, but nothing else starts a
//! listener, negotiates a session, or proves a client can reach the six tools
//! over the wire. So this file spawns the real binary against the testing vault
//! and talks to it the way an MCP client does — `initialize`, then
//! `tools/list`, then `tools/call` — over Streamable HTTP with SSE responses.
//!
//! It also asserts the property the two transports exist to share: the tool
//! surface over HTTP is the same object the tools module declares, in the same
//! order, with the same input schemas. A server that answered `tools/list`
//! differently depending on how the client arrived would be two servers, and
//! nothing else in the crate would notice.
//!
//! The same session is reused for each scenario, which is what a real client
//! does and what makes the `Mcp-Session-Id` round trip worth asserting. A
//! concurrent scenario runs the one case the `Arc`/`RwLock` refactor was for:
//! several sessions, at once.

use std::net::{SocketAddr, ToSocketAddrs};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use futures::stream::{FuturesUnordered, StreamExt};

use serde_json::{Value as Json, json};

use bases_mcp::transport::{HTTP_PATH, HttpEndpoint, Transport, select_transport};

/// How long to wait for the listener to come up before giving up.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(15);

/// How many times to retry binding an ephemeral port.
///
/// [`ephemeral_port`] asks the kernel for a free port, closes it, and spawns a
/// server to bind it. Another process can take the port in that gap, and then
/// the readiness probe connects to a stranger while our server exits on its own
/// failed bind — the probe sees an open port, never looks at the child, and the
/// first real request dies with `Connection reset by peer`. Measured exactly
/// once in a gate run at load 60, which is why the retry and the child check
/// below exist rather than a bare connect.
const START_ATTEMPTS: usize = 5;

/// `MCP_TRANSPORT`, `MCP_HTTP_HOST` and `MCP_HTTP_PORT` are process-global, and
/// this file shares a process with the other tests in this binary. The tests
/// that set them take this lock, so the endpoint tests cannot read a value
/// another test left behind. The stdio suite has its own lock for the same
/// reason; the two files run in separate processes, so the two locks never meet.
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

/// One server process, one session.
struct Server {
    child: Child,
    /// The address the process is listening on. Every readiness probe and every
    /// request goes to this address rather than to a hard-coded loopback: a
    /// server bound to a specific address is not reachable on `127.0.0.1`, and
    /// a probe that could not see the listener would time out instead of
    /// reporting the address it guessed.
    bound: SocketAddr,
    base_url: String,
    session_id: Option<String>,
}

impl Server {
    /// Spawn the binary in HTTP mode on an ephemeral port and wait until it
    /// answers.
    fn start() -> Self {
        Self::start_on("127.0.0.1")
    }

    /// Spawn the binary in HTTP mode on an ephemeral port of `host`.
    ///
    /// `host` is anything `ToSocketAddrs` accepts, so a name is resolved here
    /// rather than assumed: a test that binds `localhost` gets the address the
    /// resolver actually returned, which is the address the process is bound to.
    ///
    /// Retried, because the port can be taken between [`ephemeral_port`]
    /// releasing it and this child binding it — see [`START_ATTEMPTS`].
    fn start_on(host: &str) -> Self {
        Self::start_on_auditing(host, None)
    }

    /// A server whose audit trail is a file, so a test can read it back.
    fn start_auditing(audit: &std::path::Path) -> Self {
        Self::start_on_auditing("127.0.0.1", Some(audit))
    }

    fn start_on_auditing(host: &str, audit: Option<&std::path::Path>) -> Self {
        let mut last = String::new();
        for _ in 0..START_ATTEMPTS {
            match Self::try_start_on(host, audit) {
                Ok(server) => return server,
                Err(reason) => last = reason,
            }
        }
        panic!("the HTTP server did not start in {START_ATTEMPTS} attempts: {last}");
    }

    fn try_start_on(host: &str, audit: Option<&std::path::Path>) -> Result<Self, String> {
        let port = ephemeral_port();
        let vault = vault_dir();
        let resolved = (host, port)
            .to_socket_addrs()
            .expect("the host resolves")
            .next()
            .expect("the host resolves to an address");
        let mut command = Command::new(env!("CARGO_BIN_EXE_bases-mcp"));
        command
            .arg("--transport")
            .arg("http")
            .env("MCP_HTTP_PORT", port.to_string())
            .env("MCP_HTTP_HOST", resolved.ip().to_string())
            .env("BASES_MCP_VAULT", &vault)
            // The server's own diagnostics go to stderr; keeping them means a
            // failure prints the server's complaint rather than a bare
            // "connection refused".
            .stderr(Stdio::piped())
            .stdout(Stdio::piped());
        // A trail file only when the test asked for one: a server whose trail
        // is stderr writes a line per call into the pipe nobody reads.
        if let Some(audit) = audit {
            command.env("MCP_AUDIT_LOG", audit);
        }
        let mut child = command.spawn().expect("the binary spawns");

        let bound = SocketAddr::new(resolved.ip(), port);
        let base_url = format!("http://{bound}{HTTP_PATH}");
        let deadline = Instant::now() + STARTUP_TIMEOUT;
        loop {
            assert!(
                Instant::now() <= deadline,
                "the HTTP server did not start listening within {STARTUP_TIMEOUT:?}"
            );
            if port_is_open(bound) {
                // An open port is not proof it is OUR server: a process that
                // took the port would answer here while our child exits on its
                // own failed bind, and the first real request would then die
                // with "Connection reset by peer". So let the child's exit land
                // and require it to still be running.
                std::thread::sleep(Duration::from_millis(20));
                if let Some(status) = child.try_wait().expect("the child is pollable") {
                    return Err(format!("exited {status} after port {port} was taken"));
                }
                break;
            }
            if let Some(status) = child.try_wait().expect("the child is pollable") {
                return Err(format!("exited {status} before listening on port {port}"));
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        Ok(Self {
            child,
            bound,
            base_url,
            session_id: None,
        })
    }

    /// Complete a session: `initialize`, then `notifications/initialized`.
    async fn connect(&mut self) {
        self.connect_as("http-transport-test").await;
    }

    /// Complete a session naming `client`.
    ///
    /// Separate from [`Self::connect`] because a concurrent test has to issue
    /// several `initialize`s, and each one must complete on its own before the
    /// next — a session that is mid-handshake when another starts is not a
    /// concurrency test.
    async fn connect_as(&mut self, client: &str) {
        let (response, session_id) = self
            .post(&json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": { "name": client, "version": "0" }
                }
            }))
            .await;
        assert_eq!(response["result"]["serverInfo"]["name"], "bases-mcp");
        self.session_id = session_id;

        let (response, _) = self
            .post(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
            .await;
        // A notification carries no id and earns no response body worth
        // asserting; arriving at it without a transport error is the assertion.
        assert!(
            response.get("error").is_none(),
            "the notification was refused"
        );
    }

    /// One JSON-RPC POST, returning the payload and the `Mcp-Session-Id`.
    async fn post(&mut self, payload: &Json) -> (Json, Option<String>) {
        let mut request = client()
            .post(&self.base_url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream");
        if let Some(session) = &self.session_id {
            request = request.header("Mcp-Session-Id", session);
        }
        let response = request
            .body(serde_json::to_string(payload).expect("the payload serialises"))
            .send()
            .await
            .expect("the POST is sent");
        let session_id = response
            .headers()
            .get("Mcp-Session-Id")
            .map(|value| value.to_str().expect("the header is ASCII").to_string());
        if session_id.is_some() {
            self.session_id = session_id.clone();
        }
        let body = response.text().await.expect("the body is text");
        (sse_payload(&body), session_id)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A `reqwest` client per call: the point is not connection reuse but a clean
/// view of each response.
fn client() -> reqwest::Client {
    reqwest::Client::new()
}

/// One JSON-RPC POST, on a connection of its own.
///
/// [`Server::post`] is for one client with one session: it carries that
/// session's `Mcp-Session-Id` and remembers the id it was issued, which is what
/// a real client does. A test that opens several sessions at once cannot use it
/// — each `initialize` would echo the previous session's id — so this is the
/// same request with an explicit session and no bookkeeping.
async fn post_once(url: &str, session: Option<&str>, payload: &Json) -> (Json, Option<String>) {
    let mut request = client()
        .post(url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream");
    if let Some(session) = session {
        request = request.header("Mcp-Session-Id", session);
    }
    let response = request
        .body(serde_json::to_string(payload).expect("the payload serialises"))
        .send()
        .await
        .expect("the POST is sent");
    let session_id = response
        .headers()
        .get("Mcp-Session-Id")
        .map(|value| value.to_str().expect("the header is ASCII").to_string());
    let body = response.text().await.expect("the body is text");
    (sse_payload(&body), session_id)
}

async fn open_session(url: &str, client_name: &str) -> String {
    let (response, session_id) = post_once(
        url,
        None,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": client_name, "version": "0" }
            }
        }),
    )
    .await;
    assert_eq!(
        response["result"]["serverInfo"]["name"], "bases-mcp",
        "an initialize on a fresh connection is answered: {response}"
    );
    let session = session_id.expect("initialize issues an Mcp-Session-Id");
    assert!(
        !session.is_empty(),
        "the session id is not empty: {session:?}"
    );
    session
}

/// The testing vault on disk.
fn vault_dir() -> std::path::PathBuf {
    let mut dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    dir.push("test");
    dir.push("vault");
    dir
}

/// A port nothing is listening on, so the spawned server cannot collide with a
/// concurrent test.
fn ephemeral_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("an ephemeral port binds");
    listener.local_addr().expect("the address is known").port()
}

/// Is anything accepting on this address?
fn port_is_open(address: SocketAddr) -> bool {
    std::net::TcpStream::connect_timeout(&address, Duration::from_millis(100)).is_ok()
}

/// The JSON-RPC payload out of a `text/event-stream` body.
///
/// A Streamable HTTP response is one or more SSE events. The useful one is the
/// `data:` line that parses as JSON; the others are the retry hint and the
/// priming event. Taking the first JSON payload is what every MCP client does.
fn sse_payload(body: &str) -> Json {
    for line in body.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(payload) = line.strip_prefix("data: ")
            && payload.starts_with('{')
        {
            return serde_json::from_str(payload).expect("the data line is JSON");
        }
    }
    // An empty body is a legitimate answer to a notification, and the caller
    // asserts on whatever came back.
    if body.trim().is_empty() {
        return Json::Null;
    }
    panic!("no JSON-RPC payload in the response body: {body:?}");
}

fn tools_list(id: u64) -> Json {
    json!({ "jsonrpc": "2.0", "id": id, "method": "tools/list", "params": {} })
}

fn call(id: u64, name: &str, arguments: Json) -> Json {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": { "name": name, "arguments": arguments }
    })
}

/// `initialize` over HTTP names this server and issues a session the client must
/// echo on every later request.
#[tokio::test]
async fn initialize_over_http_names_the_server_and_issues_a_session() {
    let mut server = Server::start();
    let (response, session_id) = server
        .post(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "http-transport-test", "version": "0" }
            }
        }))
        .await;

    let result = &response["result"];
    assert_eq!(result["serverInfo"]["name"], "bases-mcp");
    assert_eq!(
        result["serverInfo"]["version"],
        env!("CARGO_PKG_VERSION"),
        "the HTTP transport reports the same version as the crate"
    );
    assert!(
        result["capabilities"]["tools"].is_object(),
        "the tools capability is advertised: {}",
        result["capabilities"]
    );
    let session = session_id.expect("initialize returns an Mcp-Session-Id");
    assert!(!session.is_empty());
}

/// `tools/list` over HTTP is the same six tools, in the same order, with the
/// same input schemas, as the declared surface.
#[tokio::test]
async fn tools_list_over_http_matches_the_declared_surface() {
    let mut server = Server::start();
    server.connect().await;

    let (response, _) = server.post(&tools_list(2)).await;
    let over_http = response["result"]["tools"].clone();

    let declared = serde_json::to_value(bases_mcp::tools::tool_definitions())
        .expect("the tool definitions serialise");

    assert_eq!(
        over_http, declared,
        "tools/list over HTTP must be the declared surface, field for field"
    );

    let names: Vec<&str> = over_http
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

/// A tool call over HTTP reaches the real engine: the query pipeline runs, rows
/// come back, and the structured content is what the tool declared.
#[tokio::test]
async fn a_tool_call_over_http_resolves_a_base() {
    let mut server = Server::start();
    server.connect().await;

    let (response, _) = server
        .post(&call(
            3,
            "resolve_base",
            json!({ "base": "AllNotes.base", "format": "json" }),
        ))
        .await;

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

/// A refusal is a tool result, not a protocol error: the same shape over HTTP as
/// over stdio, with `isError` true and the engine's own message.
#[tokio::test]
async fn an_unknown_base_is_a_tool_result_not_a_protocol_error() {
    let mut server = Server::start();
    server.connect().await;

    let (response, _) = server
        .post(&call(4, "resolve_base", json!({ "base": "Nope.base" })))
        .await;

    assert_eq!(response["result"]["isError"], json!(true));
    assert_eq!(response["result"]["structuredContent"]["health"], "failed");
    let message = response["result"]["structuredContent"]["error"]["message"]
        .as_str()
        .expect("the failure carries a message");
    assert!(
        message.contains("Nope.base"),
        "the message names the base: {message}"
    );
}

#[tokio::test]
async fn a_request_without_a_session_is_refused() {
    let server = Server::start();
    let response = client()
        .post(&server.base_url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body(serde_json::to_string(&tools_list(5)).expect("the payload serialises"))
        .send()
        .await
        .expect("the POST is sent");

    assert!(
        response.status().is_client_error(),
        "a stateless tools/list with no session is refused, not served: {}",
        response.status()
    );
}

/// Several sessions, concurrently, each calling a tool.
///
/// This is the case the `Arc`/`RwLock` refactor exists for. The nine sequential
/// scenarios above prove the wire format; only concurrent sessions prove that
/// the one shared `Resolver` can be driven by more than one client at a time
/// without a deadlock, a poisoned lock, or a session that answers another
/// session's request. Every call below is the same tool against the same vault,
/// so every session must return the same rows — a wrong or missing answer is a
/// cross-session bleed, not a scheduling artefact.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_sessions_each_resolve_the_same_vault() {
    const SESSIONS: usize = 6;

    let server = Server::start();

    // One session per connection, each opened on its own: a session id is
    // per-connection, and a real client holds one connection per session.
    let url = server.base_url.clone();
    let mut sessions = Vec::new();
    for index in 0..SESSIONS {
        let session = open_session(&url, "concurrency-test").await;
        let (response, _) = post_once(
            &url,
            Some(&session),
            &json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
        )
        .await;
        assert!(
            response.get("error").is_none(),
            "session {index} completed its handshake: {response}"
        );
        sessions.push(session);
    }
    assert_eq!(sessions.len(), SESSIONS);

    let calls: Vec<_> = sessions
        .iter()
        .enumerate()
        .map(|(index, session)| {
            let url = url.clone();
            let session = session.clone();
            async move {
                client()
                    .post(&url)
                    .header("Content-Type", "application/json")
                    .header("Accept", "application/json, text/event-stream")
                    .header("Mcp-Session-Id", session)
                    .body(
                        serde_json::to_string(&call(
                            200 + index as u64,
                            "resolve_base",
                            json!({ "base": "AllNotes.base", "format": "json" }),
                        ))
                        .expect("the payload serialises"),
                    )
                    .send()
                    .await
                    .expect("the POST is sent")
                    .text()
                    .await
                    .expect("the body is text")
            }
        })
        .collect();

    let mut pending = FuturesUnordered::from_iter(calls);
    let mut answered = 0;
    while let Some(body) = pending.next().await {
        let response = sse_payload(&body);
        let structured = &response["result"]["structuredContent"];
        assert_eq!(
            structured["health"], "ok",
            "every concurrent session resolves: {structured}"
        );
        assert!(
            structured["rowCount"].as_u64().unwrap_or(0) > 0,
            "a concurrent session produced rows: {structured}"
        );
        answered += 1;
    }
    assert_eq!(answered, SESSIONS, "every session was answered");
}

/// The two transports are the same server: `tools/call` over HTTP returns
/// exactly what the in-process tool surface returns for the same arguments.
///
/// This is the assertion the feature rests on. It is written against the library
/// rather than against a second HTTP run because a second HTTP run would prove
/// only that HTTP is deterministic, not that it is the same server.
#[tokio::test]
async fn a_tool_call_over_http_matches_the_in_process_surface() {
    use bases_mcp::service::Resolver;
    use bases_mcp::tools::ToolSurface;

    let mut server = Server::start();
    server.connect().await;

    let (response, _) = server.post(&call(6, "list_bases", json!({}))).await;
    let over_http = response["result"].clone();

    let resolver = Resolver::open_dir(vault_dir())
        .await
        .expect("the testing vault opens");
    let surface = ToolSurface::new(std::sync::Arc::new(resolver));
    let in_process = surface
        .dispatch("list_bases", &serde_json::Map::new())
        .await
        .expect("the tool is known");
    let in_process = serde_json::to_value(in_process).expect("the result serialises");

    // The whole object is not compared: rmcp's wire type omits `resultType`
    // when it is the default, so the two serialise one field apart. The fields
    // a client acts on are what has to agree.
    for field in ["content", "structuredContent", "isError"] {
        assert_eq!(
            over_http[field], in_process[field],
            "the HTTP transport and the in-process surface must agree on `{field}`"
        );
    }
}

/// `--transport` refuses a value it does not know rather than defaulting, and
/// accepts the ones it does.
#[test]
fn transport_parsing_refuses_a_typo() {
    assert_eq!(Transport::parse("stdio").unwrap(), Transport::Stdio);
    assert_eq!(Transport::parse("HTTP").unwrap(), Transport::Http);
    assert_eq!(Transport::parse(" http ").unwrap(), Transport::Http);
    let refusal = Transport::parse("htp").expect_err("a typo is not a transport");
    assert!(
        refusal.contains("htp"),
        "the refusal names what it saw: {refusal}"
    );
}

/// The path this suite probes is the one the router serves. If the two ever
/// disagree, the suite fails here rather than probing a dead endpoint.
#[test]
fn the_probed_path_is_the_served_path() {
    assert_eq!(HTTP_PATH, "/mcp");
}

/// `MCP_TRANSPORT` comes from the environment when no flag is given, and a flag
/// wins over it.
///
/// The environment half is asserted here rather than only by the subprocess
/// tests in `tests/stdio_transport.rs`, because the two files are separate
/// processes and this one has to keep its own claim about the env branch.
#[test]
fn a_flag_wins_over_the_environment() {
    let _guard = ENV.lock().expect("the env lock is not poisoned");
    set_env("MCP_TRANSPORT", "http");
    let flag = || vec!["--transport".to_string(), "stdio".to_string()].into_iter();
    assert_eq!(select_transport(flag()).unwrap(), Transport::Stdio);
    assert_eq!(
        select_transport(std::iter::empty()).unwrap(),
        Transport::Http
    );
    unset_env("MCP_TRANSPORT");
}

/// A `Host` that names the bound address is served; one that does not is a 403.
///
/// This is the defect the endpoint used to have: the listener bound the address
/// the operator named while rmcp validated `Host` against its own hardcoded
/// loopback list, so `MCP_HTTP_HOST=192.168.1.50` listened on the network and
/// served nothing, with no diagnostic. Both halves are asserted, because the
/// 403 half is what the allow-list is for and the 200 half is what the
/// operator asked for.
#[tokio::test]
async fn a_host_header_naming_the_bound_address_is_served() {
    // A second address on loopback: binding it is legal on every platform this
    // crate runs on, and it is a specific address for the purpose that matters —
    // it is not in rmcp's default allow-list, so it is refused unless the
    // endpoint derives it from the bind address.
    const HOST: &str = "127.0.0.2";
    let mut server = Server::start_on(HOST);
    server.connect_as("host-header-test").await;

    let bound = server.bound;
    let url = server.base_url.clone();
    let body = serde_json::to_string(&tools_list(1)).expect("the payload serialises");
    let post = move |host: String| {
        let url = url.clone();
        let body = body.clone();
        async move {
            client()
                .post(&url)
                .header("Host", host)
                .header("Content-Type", "application/json")
                .header("Accept", "application/json, text/event-stream")
                .body(body)
                .send()
                .await
                .expect("the POST is sent")
                .status()
        }
    };

    // A Host that names the bound address is answered. A stateless tools/list
    // earns a client error from the service; a 403 means rmcp rejected the Host
    // before the service ever saw it, which is the defect this test guards.
    let status = post(bound.to_string()).await;
    assert_ne!(
        status,
        reqwest::StatusCode::FORBIDDEN,
        "the bound address is in allowed_hosts: {status}"
    );

    // A Host that names something else is refused by the allow-list.
    let other: SocketAddr = (std::net::IpAddr::from([127, 0, 0, 3]), bound.port()).into();
    assert_eq!(
        post(other.to_string()).await,
        reqwest::StatusCode::FORBIDDEN,
        "a Host that is not the bound address is still refused"
    );

    drop(server);
}

/// A bind address given as a name resolves and is served on what it resolved to.
///
/// The review's own example is `MCP_HTTP_HOST=localhost`: it is the operator's
/// explicit value, and it must bind rather than be thrown away. What it binds
/// is whatever the resolver returns for it, so the test reads the address back
/// from the process rather than assuming it.
#[tokio::test]
async fn a_bind_address_given_as_a_name_binds_and_is_served() {
    const HOST: &str = "localhost";
    let mut server = Server::start_on(HOST);
    server.connect_as("name-bind-test").await;

    // The endpoint answered a handshake, which is the assertion: a name that did
    // not resolve, or that resolved to an address the listener was not bound to,
    // would have refused here.
    let bound = server.bound;
    assert!(
        bound.ip().is_loopback(),
        "`localhost` binds a loopback address: {bound}"
    );

    // And the name itself is in `allowed_hosts`, so a client that reaches the
    // bound address by that name is served rather than refused with a 403.
    let url = server.base_url.clone();
    let body = serde_json::to_string(&tools_list(1)).expect("the payload serialises");
    let status = client()
        .post(&url)
        .header("Host", format!("localhost:{}", bound.port()))
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body(body)
        .send()
        .await
        .expect("the POST is sent")
        .status();
    assert_ne!(
        status,
        reqwest::StatusCode::FORBIDDEN,
        "a name that resolved is served by that name: {status}"
    );

    drop(server);
}

/// The endpoint a bind address implies, asserted without a listener: the pure
/// half of the rule, covered end to end by the test above.
#[test]
fn the_endpoint_derives_allowed_hosts_from_the_bound_address() {
    let _guard = ENV.lock().expect("the env lock is not poisoned");

    // Unset is loopback.
    unset_env("MCP_HTTP_HOST");
    unset_env("MCP_HTTP_PORT");
    let endpoint = HttpEndpoint::from_env().expect("an empty environment is loopback");
    assert_eq!(endpoint.address(), "127.0.0.1:8765".parse().unwrap());

    // A specific address is served on that address and on localhost, and not on
    // a neighbouring one.
    set_env("MCP_HTTP_HOST", "192.168.1.50");
    set_env("MCP_HTTP_PORT", "8791");
    let endpoint = HttpEndpoint::from_env().expect("a literal address parses");
    assert_eq!(endpoint.address(), "192.168.1.50:8791".parse().unwrap());
    let names = endpoint.allowed_hosts();
    assert!(names.contains(&"192.168.1.50".to_string()));
    assert!(names.contains(&"localhost".to_string()));
    assert!(!names.contains(&"192.168.1.99".to_string()));

    // Every interface means every Host, which rmcp spells as an empty list.
    set_env("MCP_HTTP_HOST", "0.0.0.0");
    let endpoint = HttpEndpoint::from_env().expect("0.0.0.0 parses");
    assert!(endpoint.allowed_hosts().is_empty());

    // A name is resolved, not refused: `localhost` is the value the review
    // named, and it binds the loopback it resolves to.
    set_env("MCP_HTTP_HOST", "localhost");
    let endpoint = HttpEndpoint::from_env().expect("localhost resolves");
    assert!(
        endpoint.address().ip().is_loopback(),
        "localhost binds a loopback address: {}",
        endpoint.address()
    );
    assert!(
        endpoint.allowed_hosts().contains(&"localhost".to_string()),
        "a name that resolved is served by that name: {:?}",
        endpoint.allowed_hosts()
    );

    unset_env("MCP_HTTP_HOST");
    unset_env("MCP_HTTP_PORT");
}

/// A bad bind address or port is refused, loudly, naming the variable and the
/// value.
///
/// `localhost` is deliberately absent: it is a name, and a name this host
/// resolves is a legitimate bind address rather than a typo. The refusal case
/// is a name nothing resolves.
#[test]
fn a_bad_bind_address_or_port_is_refused_by_name() {
    let _guard = ENV.lock().expect("the env lock is not poisoned");

    for (host, port) in [
        ("no-such-host.invalid", "8765"),
        ("localhost", "notaport"),
        ("localhost", ""),
        ("localhost", "   "),
    ] {
        set_env("MCP_HTTP_HOST", host);
        set_env("MCP_HTTP_PORT", port);
        let refusal = HttpEndpoint::from_env().expect_err("a bad value is not an endpoint");
        let (named, value) = if host.contains("invalid") {
            ("MCP_HTTP_HOST", host)
        } else {
            ("MCP_HTTP_PORT", port)
        };
        assert!(
            refusal.contains(named),
            "the refusal names {named}: {refusal}"
        );
        assert!(
            refusal.contains(value.trim()),
            "the refusal names the value: {refusal}"
        );
    }

    unset_env("MCP_HTTP_HOST");
    unset_env("MCP_HTTP_PORT");
}

/// The trail over HTTP names the caller, the session and the peer address.
///
/// This is the half no unit test can reach: the client name comes from the
/// handshake, the session id from the transport's own header, and the peer
/// address from the socket the request arrived on. The peer is the only one of
/// the three a caller cannot forge, so it is the one that has to be proven over
/// the wire rather than through a `MemorySink`.
#[tokio::test]
async fn the_trail_names_the_caller_the_session_and_the_peer() {
    let dir = tempfile::TempDir::new().expect("a temp dir is writable");
    let log = dir.path().join("audit.log");
    let mut server = Server::start_auditing(&log);
    server.connect_as("trail-probe").await;
    let session = server
        .session_id
        .clone()
        .expect("the handshake issued a session");

    let (response, _) = server
        .post(&json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "get_note",
                "arguments": { "path": "Root Ticket.md", "raw": true }
            }
        }))
        .await;
    assert!(
        response.get("error").is_none(),
        "the call is answered: {response}"
    );

    // The handshake's record is written by the notification's own handler, so
    // it can land after the POST that sent the notification returns.
    let trail = wait_for_trail(&log, |text| {
        text.contains("\"tool\":\"initialize\"") && text.contains("\"tool\":\"get_note\"")
    });

    let record = |tool: &str| -> Json {
        let line = trail
            .lines()
            .find(|line| line.contains(&format!("\"tool\":\"{tool}\"")))
            .unwrap_or_else(|| panic!("the trail holds no {tool} record: {trail}"));
        serde_json::from_str(line).expect("a record is one JSON value")
    };

    // Who opened the session, and from where.
    let handshake = record("initialize");
    assert_eq!(handshake["caller"], "trail-probe 0", "{handshake}");
    assert_eq!(handshake["transport"], "http", "{handshake}");
    assert_eq!(
        handshake["session"],
        Json::String(session.clone()),
        "{handshake}"
    );
    let peer = handshake["peer"].as_str().expect("a peer address");
    assert!(
        peer.starts_with("127.0.0.1:"),
        "the peer is the socket the request arrived on: {peer}"
    );

    // What the call acted on.
    let call = record("get_note");
    assert_eq!(call["target"], "Root Ticket.md", "{call}");
    assert_eq!(call["outcome"], "ok", "{call}");
    assert_eq!(call["session"], Json::String(session), "{call}");
    // Not the same socket as the handshake's: each request here arrives on a
    // connection of its own, so the peer is per-connection rather than
    // per-session. What it proves is that the address is recorded at all.
    let call_peer = call["peer"].as_str().expect("a peer address");
    assert!(
        call_peer.starts_with("127.0.0.1:"),
        "the call's peer is its own socket: {call_peer}"
    );

    // And the note's text is nowhere in it.
    assert!(
        !trail.contains("# Root ticket"),
        "the trail carried the note's content: {trail}"
    );
}

/// Read the trail, waiting until `ready` accepts what is there.
///
/// A record is written after the call it describes, and the handshake's record
/// is written by a notification handler that runs on its own task, so the file
/// can lag the reply a client just received.
fn wait_for_trail(path: &std::path::Path, ready: impl Fn(&str) -> bool) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        if ready(&text) {
            return text;
        }
        assert!(
            Instant::now() <= deadline,
            "the trail never held what the test waited for: {text:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}
