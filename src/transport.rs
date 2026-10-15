//! Which transport to serve, and how to choose it.
//!
//! This lives in the library rather than in `main.rs` because the choice is
//! application behaviour with a refusal path, and a refusal path nobody tests
//! is a refusal nobody has seen. The binary only decides *when* to ask; the
//! rules of the answer are here, next to the vault configuration they sit
//! beside.
//!
//! `stdio` is the default: it is what a desktop agent launches, and it needs
//! no address. `http` needs one, so it also owns the address and the path.

use std::net::SocketAddr;

/// The path the HTTP transport serves on. rmcp's service handles every method
/// itself — POST for a request, GET for the SSE stream, DELETE to close a
/// session — so the router is that one service nested at this prefix. A prefix
/// rather than the bare root: a deployment usually has something else on `/`.
pub const HTTP_PATH: &str = "/mcp";

/// The transport to serve on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Transport {
    /// One client, over stdin/stdout. The default.
    #[default]
    Stdio,
    /// Any number of clients, over HTTP. Streamable HTTP with SSE responses.
    Http,
}

impl Transport {
    /// Parse a `--transport` / `MCP_TRANSPORT` value.
    ///
    /// Anything else is a refusal rather than a silent default: a typo that
    /// quietly served stdio would leave an agent waiting for an HTTP handshake
    /// that never comes.
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "stdio" => Ok(Self::Stdio),
            "http" => Ok(Self::Http),
            other => Err(format!(
                "unknown transport \"{other}\". Expected `stdio` or `http`. \
                 Set MCP_TRANSPORT or pass --transport."
            )),
        }
    }

    /// The address the HTTP transport binds.
    ///
    /// `127.0.0.1` by default: this serves a vault, and a vault on `0.0.0.0` is
    /// a vault on the network. `MCP_HTTP_HOST` / `MCP_HTTP_PORT` move it, and
    /// that is the operator's explicit choice rather than a default they have
    /// to undo.
    pub fn http_address(&self) -> SocketAddr {
        let host = std::env::var("MCP_HTTP_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
        let port = std::env::var("MCP_HTTP_PORT")
            .ok()
            .and_then(|value| value.parse::<u16>().ok())
            .unwrap_or(8765);
        format!("{host}:{port}")
            .parse()
            .unwrap_or_else(|_| SocketAddr::from(([127, 0, 0, 1], port)))
    }
}

/// `--transport stdio|http` if given, otherwise `MCP_TRANSPORT`, otherwise
/// stdio. The argument wins: a caller who passed a flag meant it.
///
/// `-h` and `--help` are not transports; they are answered by [`usage`] and
/// asked about before this runs. Everything else is a refusal: this server has
/// no other flags, and accepting one would be a promise to honour it.
pub fn select_transport<I: Iterator<Item = String>>(args: I) -> Result<Transport, String> {
    let mut flag = None;
    let mut iter = args;
    while let Some(arg) = iter.next() {
        if arg == "--transport" {
            // A flag with no value is a refusal, not an empty transport name.
            // A following flag is not a value either: `--transport --help` is
            // the caller asking for help, not naming a transport `--help`.
            let value = iter
                .next()
                .ok_or_else(|| "--transport needs a value: `stdio` or `http`.".to_string())?;
            if value.starts_with('-') {
                return Err(format!(
                    "--transport needs a value: `stdio` or `http`, not \"{value}\"."
                ));
            }
            flag = Some(value);
        } else if let Some(value) = arg.strip_prefix("--transport=") {
            flag = Some(value.to_string());
        } else {
            return Err(format!(
                "unexpected argument \"{arg}\". This server takes --transport stdio|http and nothing else."
            ));
        }
    }
    let value = match flag {
        Some(value) => value,
        None => std::env::var("MCP_TRANSPORT").unwrap_or_else(|_| "stdio".to_string()),
    };
    Transport::parse(&value)
}

/// The usage message, for a caller that wants to print it itself.
///
/// Stated here rather than in `main.rs` because it is the same text
/// [`select_transport`] refuses with, and two copies of the flag surface would
/// be two things to update.
pub fn usage() -> &'static str {
    concat!(
        "bases-mcp — an MCP server that resolves Obsidian Bases to markdown\n",
        "\n",
        "One binary, one vault, two transports:\n",
        "\n",
        "  --transport stdio   one client over stdin/stdout (the default)\n",
        "  --transport http    any number of clients over Streamable HTTP (SSE)\n",
        "\n",
        "  MCP_TRANSPORT=stdio|http   same choice, without a flag\n",
        "\n",
        "The vault is chosen by the environment, never by a flag:\n",
        "\n",
        "  BASES_MCP_VAULT              path to a vault directory on the local filesystem\n",
        "  BASES_MCP_WEBDAV_URL         WebDAV base URL, no credentials in it\n",
        "  BASES_MCP_WEBDAV_USER        WebDAV username\n",
        "  BASES_MCP_WEBDAV_PASSWORD    WebDAV password\n",
        "\n",
        "  Exactly one backend is used. BASES_MCP_VAULT wins if both are set.\n",
        "\n",
        "HTTP transport only:\n",
        "\n",
        "  MCP_HTTP_HOST    bind address (default 127.0.0.1)\n",
        "  MCP_HTTP_PORT    bind port (default 8765)\n",
        "\n",
        "  The endpoint is http://HOST:PORT/mcp\n",
    )
}

/// Whether the arguments ask for help rather than a transport.
pub fn wants_help<I: Iterator<Item = String>>(args: I) -> bool {
    args.into_iter().any(|arg| arg == "-h" || arg == "--help")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args<'a>(values: &'a [&'a str]) -> impl Iterator<Item = String> + 'a {
        values.iter().map(|value| (*value).to_string())
    }

    #[test]
    fn the_two_transports_parse_and_a_typo_is_refused() {
        assert_eq!(Transport::parse("stdio").unwrap(), Transport::Stdio);
        assert_eq!(Transport::parse("HTTP").unwrap(), Transport::Http);
        assert_eq!(Transport::parse(" http ").unwrap(), Transport::Http);
        let refusal = Transport::parse("htp").expect_err("a typo is not a transport");
        assert!(
            refusal.contains("htp"),
            "the refusal names what it saw: {refusal}"
        );
    }

    #[test]
    fn a_flag_wins_over_the_environment() {
        std::env::set_var("MCP_TRANSPORT", "stdio");
        assert_eq!(
            select_transport(args(&["--transport", "http"])).unwrap(),
            Transport::Http
        );
        assert_eq!(
            select_transport(args(&["--transport=http"])).unwrap(),
            Transport::Http
        );
        assert_eq!(select_transport(args(&[])).unwrap(), Transport::Stdio);
    }

    #[test]
    fn an_unknown_argument_is_refused_by_name() {
        let refusal = select_transport(args(&["--port", "8080"])).expect_err("no such flag");
        assert!(
            refusal.contains("--port"),
            "the refusal names the flag: {refusal}"
        );
    }
}
