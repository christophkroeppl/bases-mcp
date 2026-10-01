//! The stdio MCP server entrypoint.
//!
//! Everything here exists to protect one thing: stdout. On a stdio transport
//! stdout IS the protocol stream, so a single stray `println!` anywhere in the
//! dependency chain — ours, the SDK's, or a future tool's — injects a line of
//! prose into JSON-RPC and the client drops the connection. The rule is
//! therefore absolute: startup diagnostics go to stderr, and nothing goes to
//! stdout except what the transport itself writes. Once the server is serving
//! this file is silent forever.
//!
//! The other job is refusing to start against the wrong vault. This server is
//! one vault per process, configured by environment, and every path where that
//! configuration is absent, blank, incomplete or unusable is a hard failure
//! naming the variable to set. The tempting alternative — defaulting
//! `BASES_MCP_VAULT` to `test/vault` or the home directory — is worse than not
//! starting at all: a server answering confidently from the wrong vault is a
//! corruption no agent downstream can see.
//!
//! Every failure below is fatal by design. A half-started MCP server is worse
//! than one that never ran: the client sees a process that accepted a connection
//! and then answered from nothing.

use std::process::ExitCode;

use rmcp::transport::stdio;
use rmcp::ServiceExt;

use bases_mcp::config::{open_source, parse_config, process_env, ConfigResult, NAME};
use bases_mcp::service::Resolver;
use bases_mcp::tools::ToolSurface;

/// Start the server on stdio, or exit non-zero explaining why not.
#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            warn(&message);
            ExitCode::FAILURE
        }
    }
}

/// Serve until the client disconnects.
///
/// Wrapped in a `LocalSet` because rmcp's `local` mode spawns the serve loop
/// with `spawn_local`: the handler holds an `Rc<Vault>`, which is the honest
/// shape for a single-threaded vault index and is why the mode is on at all.
async fn run() -> Result<(), String> {
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
    let resolver = Resolver::open(source)
        .await
        .map_err(|error| error.message().to_string())?;

    // Past this line, silence. The transport owns stdout from here on.
    let server = ToolSurface::new(std::rc::Rc::new(resolver));
    let transport = stdio();
    tokio::task::LocalSet::new()
        .run_until(async move {
            let service = server
                .serve(transport)
                .await
                .map_err(|error| error.to_string())?;
            let reason = service.waiting().await.map_err(|error| error.to_string())?;
            warn(&format!("disconnected: {reason:?}"));
            Ok(())
        })
        .await
}

/// A diagnostic on stderr. Never stdout, at no point, for any reason.
fn warn(message: &str) {
    eprintln!("{NAME}: {message}");
}
