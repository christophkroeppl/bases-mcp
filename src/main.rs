//! The server entrypoint: two transports, one vault, one tool surface.
//!
//! # stdout is sacred on the stdio path
//!
//! On stdio, stdout IS the protocol stream, so a single stray `println!`
//! anywhere in the dependency chain injects a line of prose into JSON-RPC and
//! the client drops the connection. The rule is absolute: diagnostics go to
//! stderr, and nothing goes to stdout except what the transport itself writes.
//! The HTTP path is not subject to that rule — its transport owns the socket —
//! but it honours it anyway, so one code path cannot teach a habit the other
//! one forbids.
//!
//! # Both transports are the same server
//!
//! [`serve_stdio`] and [`serve_http`] differ only in how they hand bytes to
//! rmcp. Both index the same vault through [`Resolver::open`] and both serve the
//! same [`ToolSurface`], so `tools/list` and `tools/call` cannot drift between
//! them — there is one definition of the surface, in `tools::tool_definitions`.
//! `tests/http_transport.rs` asserts both over the wire: the HTTP transport
//! ends to end, and the stdio transport's handshake.

use std::net::SocketAddr;
use std::process::ExitCode;

use rmcp::ServiceExt;
use rmcp::transport::streamable_http_server::StreamableHttpService;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;

use bases_mcp::audit::{AuditConfigResult, parse_audit_config};
use bases_mcp::config::{ConfigResult, NAME, open_source, parse_config, process_env};
use bases_mcp::service::Resolver;
use bases_mcp::tools::ToolSurface;
use bases_mcp::transport::{
    HTTP_PATH, HttpEndpoint, Transport, select_transport, usage, wants_help,
};

/// Start the server on the named transport, or exit non-zero explaining why not.
#[tokio::main]
async fn main() -> ExitCode {
    // Help is answered before the transport is chosen, and on stdout: no
    // transport has taken stdout yet, so this cannot corrupt a protocol stream.
    if wants_help(std::env::args().skip(1)) {
        println!("{}", usage());
        return ExitCode::SUCCESS;
    }

    let transport = match select_transport(std::env::args().skip(1)) {
        Ok(transport) => transport,
        Err(message) => {
            warn(&message);
            return ExitCode::FAILURE;
        }
    };

    let outcome = match transport {
        Transport::Stdio => serve_stdio().await,
        Transport::Http => serve_http().await,
    };
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            warn(&message);
            ExitCode::FAILURE
        }
    }
}

/// Serve one client over stdin/stdout, until it disconnects.
async fn serve_stdio() -> Result<(), String> {
    let resolver = open_resolver().await?;
    let audit = open_audit("stdio")?;

    // Past this line, silence. The transport owns stdout from here on.
    let server = ToolSurface::new(std::sync::Arc::new(resolver)).with_audit(audit, "stdio");
    let service = server
        .serve(rmcp::transport::stdio())
        .await
        .map_err(|error| error.to_string())?;
    let reason = service.waiting().await.map_err(|error| error.to_string())?;
    warn(&format!("disconnected: {reason:?}"));
    Ok(())
}

/// Serve any number of clients over Streamable HTTP, until the process is
/// killed.
///
/// Each `initialize` creates a session; the id comes back on the
/// `Mcp-Session-Id` header and the client echoes it on every later request.
async fn serve_http() -> Result<(), String> {
    let resolver = open_resolver().await?;
    let audit = open_audit("http")?;

    // The endpoint is parsed here rather than read at bind time: a bad
    // MCP_HTTP_HOST or MCP_HTTP_PORT is refused before anything is opened, and
    // `allowed_hosts` is derived from the same value the listener binds so the
    // two cannot disagree — see `HttpEndpoint`.
    let endpoint = HttpEndpoint::from_env()?;
    // Owned, so the borrow of `endpoint` ends here and the spawned servers below
    // can be `'static`.
    let addresses: Vec<SocketAddr> = endpoint.addresses().to_vec();

    // One vault, one Resolver, shared by every session. `Arc` rather than a
    // per-session clone because the index is the same bytes for every client,
    // and the `RwLock` inside it is what makes sharing it sound. rmcp calls the
    // factory per session, so each session gets its own `ToolSurface` wrapping
    // the shared state.
    let factory = std::sync::Arc::new(resolver);

    // The audit sink is shared by every session too: it is one trail, and a
    // per-session file would be six trails interleaved in one directory.
    let sink = std::sync::Arc::clone(&audit);

    // The service is a tower `Service`, and axum drives it directly. It handles
    // every method itself — POST for a request, GET for the SSE stream, DELETE
    // to close a session — so the whole router is that one service at `HTTP_PATH`.
    // Built once and cloned per listener: `Router` is cheap to clone and the
    // sessions are independent, so one router can serve several sockets.
    let app = {
        let service = StreamableHttpService::new(
            move || {
                Ok(ToolSurface::new(std::sync::Arc::clone(&factory))
                    .with_audit(std::sync::Arc::clone(&sink), "http"))
            },
            std::sync::Arc::new(LocalSessionManager::default()),
            endpoint.rmcp_config(),
        );
        axum::Router::new().nest_service(HTTP_PATH, service)
    };

    // A name may resolve to several addresses and every one is bound: a client
    // that resolved a different address of the same name must not be refused.
    let mut listeners = Vec::new();
    for address in addresses {
        let listener = tokio::net::TcpListener::bind(address)
            .await
            .map_err(|error| format!("cannot bind {address}: {error}"))?;
        // `MCP_HTTP_PORT=0` asks the kernel for a free port, so only the bound
        // address can be dialled.
        let bound = listener.local_addr().unwrap_or(address);
        listeners.push((bound, listener));
    }

    // `0.0.0.0` means rmcp was told to accept any `Host`, which is the
    // operator's choice and is not a default they can undo. Saying so on stderr
    // is the only place a listener that serves every interface announces it.
    if endpoint.allowed_hosts().is_empty() {
        eprintln!(
            "{}: every interface is bound, so every Host header is accepted. \
             This listener is network-accessible and has no authentication; it is \
             safe only on a network you trust. See docs/divergences.md.",
            NAME
        );
    }
    for (address, _) in &listeners {
        eprintln!("{NAME}: listening on http://{address}{HTTP_PATH} (streamable HTTP)");
    }
    // A shutdown is SIGTERM or SIGINT, and it is awaited. systemd sends SIGTERM
    // and waits; a server that ignored it would be SIGKILLed after the timeout,
    // dropping in-flight requests. `axum::serve` runs until the process dies
    // unless a shutdown is handed to it, so the signal is what makes this
    // function return at all.
    let on_shutdown = || async {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("SIGTERM is always installable");
        let mut interrupt =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
                .expect("SIGINT is always installable");
        tokio::select! {
            _ = term.recv() => {}
            _ = interrupt.recv() => {}
        }
    };

    let mut servers = Vec::new();
    for (address, listener) in listeners {
        let app = app.clone();
        servers.push(tokio::spawn(async move {
            // `into_make_service_with_connect_info` is what puts the peer's
            // address into the request, so the audit trail can name where a
            // call came from. Without it the trail says who called what and
            // not from where, which is the half of the question a caller
            // cannot forge.
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(on_shutdown())
            .await
            .map_err(|error| format!("{address}: {error}"))
        }));
    }
    for server in servers {
        server.await.map_err(|error| error.to_string())??;
    }
    Ok(())
}

/// Open the audit trail.
///
/// The configuration is read from the environment here, and a refusal is
/// fatal: a server whose audit trail silently goes nowhere is the thing the
/// trail exists to prevent, and a half-started server is worse than one that
/// never ran.
fn open_audit(transport: &str) -> Result<std::sync::Arc<dyn bases_mcp::audit::AuditSink>, String> {
    let config = match parse_audit_config(&process_env()) {
        AuditConfigResult::Ok { config, warnings } => {
            for warning in warnings {
                warn(&warning);
            }
            config
        }
        AuditConfigResult::Err { message } => return Err(message),
    };
    let sink = config.open()?;
    // Where the trail goes is said ONCE, on stderr, at startup. An operator
    // who cannot tell whether the trail is running has no way to know it is
    // recording.
    warn(&format!(
        "audit trail on the {transport} transport: {}",
        config.describe()
    ));
    Ok(std::sync::Arc::from(sink))
}

/// Parse the environment and index the vault it names.
///
/// Shared by both transports, so a bad `BASES_MCP_VAULT` is refused with the
/// same message however the client arrived. Every failure is fatal: a
/// half-started server is worse than one that never ran.
async fn open_resolver() -> Result<Resolver, String> {
    let config = match parse_config(&process_env()) {
        ConfigResult::Ok { config, warnings } => {
            for warning in warnings {
                warn(&warning);
            }
            config
        }
        ConfigResult::Err { message } => return Err(message),
    };

    // The directory is checked before the resolver opens it, because
    // `FsVaultSource` swallows a failed `readdir`: a mistyped `BASES_MCP_VAULT`
    // would otherwise index as a vault with no notes, and every Base would come
    // back with zero rows. See `config::open_source`.
    let source = open_source(&config).map_err(|error| error.message().to_string())?;
    Resolver::open(source)
        .await
        .map_err(|error| error.message().to_string())
}

/// A diagnostic on stderr. Never stdout, at no point, for any reason.
fn warn(message: &str) {
    eprintln!("{NAME}: {message}");
}
