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
//! `tests/http_transport.rs` asserts that over the wire.

use std::process::ExitCode;

use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::ServiceExt;

use bases_mcp::config::{open_source, parse_config, process_env, ConfigResult, NAME};
use bases_mcp::service::Resolver;
use bases_mcp::tools::ToolSurface;
use bases_mcp::transport::{select_transport, usage, wants_help, Transport, HTTP_PATH};

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

    // Past this line, silence. The transport owns stdout from here on.
    let server = ToolSurface::new(std::sync::Arc::new(resolver));
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

    // One vault, one Resolver, shared by every session. `Arc` rather than a
    // per-session clone because the index is the same bytes for every client,
    // and the `RwLock` inside it is what makes sharing it sound. rmcp calls the
    // factory per session, so each session gets its own `ToolSurface` wrapping
    // the shared state.
    let factory = std::sync::Arc::new(resolver);
    let service = StreamableHttpService::new(
        move || Ok(ToolSurface::new(std::sync::Arc::clone(&factory))),
        std::sync::Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    );

    // The service is a tower `Service`, and axum drives it directly. It handles
    // every method itself — POST for a request, GET for the SSE stream, DELETE
    // to close a session — so the whole router is that one service at `HTTP_PATH`.
    let app = axum::Router::new().nest_service(HTTP_PATH, service);
    let address = Transport::Http.http_address();
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .map_err(|error| format!("cannot bind {address}: {error}"))?;

    eprintln!("{NAME}: listening on http://{address}{HTTP_PATH} (streamable HTTP)");
    axum::serve(listener, app)
        .await
        .map_err(|error| error.to_string())
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
