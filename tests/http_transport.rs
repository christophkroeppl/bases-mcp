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
//! does and what makes the `Mcp-Session-Id` round trip worth asserting.

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value as Json};

use bases_mcp::transport::{select_transport, Transport, HTTP_PATH};

/// How long to wait for the listener to come up before giving up.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(15);

/// One server process, one session.
struct Server {
    child: Child,
    base_url: String,
    session_id: Option<String>,
}

impl Server {
    /// Spawn the binary in HTTP mode on an ephemeral port and wait until it
    /// answers.
    fn start() -> Self {
        let port = ephemeral_port();
        let vault = vault_dir();
        let mut child = Command::new(env!("CARGO_BIN_EXE_bases-mcp"))
            .arg("--transport")
            .arg("http")
            .env("MCP_HTTP_PORT", port.to_string())
            .env("MCP_HTTP_HOST", "127.0.0.1")
            .env("BASES_MCP_VAULT", &vault)
            // The server's own diagnostics go to stderr; keeping them means a
            // failure prints the server's complaint rather than a bare
            // "connection refused".
            .stderr(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("the binary spawns");

        let base_url = format!("http://127.0.0.1:{port}{HTTP_PATH}");
        let deadline = Instant::now() + STARTUP_TIMEOUT;
        loop {
            assert!(
                Instant::now() <= deadline,
                "the HTTP server did not start listening within {STARTUP_TIMEOUT:?}"
            );
            if port_is_open(port) {
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
            base_url,
            session_id: None,
        }
    }

    /// Complete a session: `initialize`, then `notifications/initialized`.
    async fn connect(&mut self) {
        let (response, session_id) = self
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

/// Is anything accepting on this port?
fn port_is_open(port: u16) -> bool {
    std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_millis(100),
    )
    .is_ok()
}

/// The JSON-RPC payload out of a `text/event-stream` body.
///
/// A Streamable HTTP response is one or more SSE events. The useful one is the
/// `data:` line that parses as JSON; the others are the retry hint and the
/// priming event. Taking the first JSON payload is what every MCP client does.
fn sse_payload(body: &str) -> Json {
    for line in body.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(payload) = line.strip_prefix("data: ") {
            if payload.starts_with('{') {
                return serde_json::from_str(payload).expect("the data line is JSON");
            }
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

/// The `--transport` value comes from the environment when no flag is given, and
/// a flag wins over it.
#[test]
fn a_flag_wins_over_the_environment() {
    std::env::set_var("MCP_TRANSPORT", "stdio");
    let flag = || vec!["--transport".to_string(), "http".to_string()].into_iter();
    assert_eq!(select_transport(flag()).unwrap(), Transport::Http);
    assert_eq!(
        select_transport(std::iter::empty()).unwrap(),
        Transport::Stdio
    );
}
