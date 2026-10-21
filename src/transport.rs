//! Which transport to serve, and how to choose it.
//!
//! This lives in the library rather than in `main.rs` because the choice is
//! application behaviour with a refusal path, and a refusal path nobody tests
//! is a refusal nobody has seen. The binary only decides *when* to ask; the
//! rules of the answer are here, next to the vault configuration they sit
//! beside.
//!
//! `stdio` is the default: it is what a desktop agent launches, and it needs
//! no address. `http` needs one, so it also owns the address, the path, and
//! the one refusal the path implies — a bind address is served only to clients
//! whose `Host` names it.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};

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

/// The address the HTTP transport binds, and every name rmcp will accept on it.
///
/// One value rather than an address plus a list, because rmcp's
/// `allowed_hosts` cannot be derived from the address at request time: rmcp
/// rejects a `Host` it does not recognise with a bare `403 Forbidden`, so the
/// two have to agree at startup or the server listens on the network and
/// serves nothing. See [`allowed_hosts`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpEndpoint {
    addresses: Vec<SocketAddr>,
    allowed_hosts: Vec<String>,
}

impl HttpEndpoint {
    /// Read the address the operator asked for, refusing what will not work.
    ///
    /// `MCP_HTTP_HOST` names the bind address and `MCP_HTTP_PORT` its port;
    /// unset, they are `127.0.0.1:8765`. A value that is set and wrong is a
    /// refusal naming the variable and the value, never a silent fallback: a
    /// typo'd bind address that fell back to loopback would leave an operator
    /// waiting for a handshake that never arrives, and a blank port would
    /// look like an unset one to the code that replaced it.
    ///
    /// `MCP_HTTP_HOST` is read but cannot fail on its own: a value that is not a
    /// literal IP is a name, and a name the operator's network resolves is a
    /// legitimate bind address. So the value is carried to
    /// [`HttpEndpoint::resolve`], which is where a name either resolves to
    /// address(es) or is refused with the value named.
    pub fn from_env() -> Result<Self, String> {
        let host = read_host()?;
        let port = read_port()?;
        let addresses = Self::resolve(host, port)?;
        Ok(Self {
            allowed_hosts: allowed_hosts_for(&addresses),
            addresses,
        })
    }

    /// Turn a host and port into every address to bind.
    ///
    /// A literal IP is one address. A name goes through `ToSocketAddrs`, which
    /// resolves it with the operating system's resolver — `localhost`, a
    /// container name, a LAN name — and **every** address it returns is bound,
    /// not just the first: a dual-stack name that resolves to both `::1` and
    /// `127.0.0.1` must answer on both, and a name with several A records must
    /// answer on each, or a client that resolved the other one gets a connection
    /// refused. A name that resolves to nothing is a refusal naming the value,
    /// because a bind address that cannot be resolved cannot be bound and the
    /// listener would otherwise fail with the OS's own message.
    ///
    /// Split out so a test can exercise the refusal without a listener.
    fn resolve(host: Host, port: u16) -> Result<Vec<SocketAddr>, String> {
        match host {
            Host::Ip(ip) => Ok(vec![SocketAddr::new(ip, port)]),
            Host::Name(name) => {
                let mut found: Vec<SocketAddr> = (name.as_str(), port)
                    .to_socket_addrs()
                    .map_err(|_| bad_host(&name, port))?
                    .collect();
                if found.is_empty() {
                    return Err(bad_host(&name, port));
                }
                found.dedup();
                Ok(found)
            }
        }
    }

    /// The first address, for the startup line and for callers that want one.
    pub fn address(&self) -> SocketAddr {
        self.addresses[0]
    }

    /// Every address this endpoint binds, in resolver order.
    pub fn addresses(&self) -> &[SocketAddr] {
        &self.addresses
    }

    /// The `Host` authorities rmcp will serve. Empty means "any", and is
    /// therefore returned as an explicit empty list rather than by omission.
    pub fn allowed_hosts(&self) -> &[String] {
        &self.allowed_hosts
    }

    /// Build rmcp's server config with this endpoint's `allowed_hosts` applied.
    ///
    /// Every other field is rmcp's own default, so a change there is upstream's
    /// business rather than a second copy to keep in step. `allowed_hosts` is
    /// the one field that must not be: see [`allowed_hosts_for`].
    pub fn rmcp_config(
        &self,
    ) -> rmcp::transport::streamable_http_server::StreamableHttpServerConfig {
        rmcp::transport::streamable_http_server::StreamableHttpServerConfig::default()
            .with_allowed_hosts(self.allowed_hosts.clone())
    }
}

/// A bind address that parsed, and is therefore either a literal IP or a name.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Host {
    Ip(IpAddr),
    Name(String),
}

/// `MCP_HTTP_HOST`, defaulting to loopback.
///
/// A value that parses as an IP is the address; anything else is a name the
/// operator's network may still resolve, so it is carried as-is and refused at
/// `HttpEndpoint::resolve`, where the value can be named.
///
/// One shape is refused here instead: a `host:port` pair. The variable name
/// reads like a full address and operators set `127.0.0.1:8765` into it, which
/// is a `SocketAddr` and not a host — carrying it as a name would make the
/// resolver look up the literal string `127.0.0.1:8765` and refuse with a
/// message about a name that does not resolve, which names the wrong mistake.
/// The port belongs to `MCP_HTTP_PORT`, and saying so is the whole point.
fn read_host() -> Result<Host, String> {
    parse_host(filled("MCP_HTTP_HOST"))
}

fn parse_host(value: Option<String>) -> Result<Host, String> {
    let Some(value) = value else {
        return Ok(Host::Ip(IpAddr::V4(Ipv4Addr::LOCALHOST)));
    };
    if let Ok(ip) = value.parse::<IpAddr>() {
        return Ok(Host::Ip(ip));
    }
    if value.parse::<SocketAddr>().is_ok() {
        return Err(format!(
            "MCP_HTTP_HOST is {value:?}, which is a host and a port together. \
             MCP_HTTP_HOST names the address alone — use MCP_HTTP_PORT for the port"
        ));
    }
    Ok(Host::Name(value))
}

/// `MCP_HTTP_PORT`, defaulting to 8765.
fn read_port() -> Result<u16, String> {
    match std::env::var("MCP_HTTP_PORT") {
        Ok(value) => parse_port(&value),
        Err(_) => Ok(8765),
    }
}

/// The port rule, over the value rather than over the environment.
///
/// A variable that is set and blank is a refusal, not an absence: blank
/// trims to nothing, which would otherwise look like an unset variable to the
/// code that replaced it. Split out the way [`parse_host`] is, so the rule is
/// testable without writing a process-global variable — which a test in this
/// binary must not do, because other tests here read the environment.
fn parse_port(value: &str) -> Result<u16, String> {
    value.trim().parse::<u16>().map_err(|_| bad_port(value))
}

/// A variable with content in it: present, non-blank, trimmed.
///
/// The same rule `config::parse_config` applies to a backend selector: a
/// variable that is set and blank is an error rather than an absence.
fn filled(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn bad_host(name: &str, port: u16) -> String {
    format!(
        "MCP_HTTP_HOST is \"{name}\", which is not an IP address and does not \
         resolve to one at \":{port}\". Set it to an IP address (127.0.0.1, \
         0.0.0.0, ::, or a specific interface address), to a name this host can \
         resolve, or unset it to bind 127.0.0.1."
    )
}

fn bad_port(value: &str) -> String {
    format!(
        "MCP_HTTP_PORT is \"{value}\", which is not a port. Set it to a number \
         between 1 and 65535, or unset it to use 8765."
    )
}

/// Every `Host` authority this bind address serves.
///
/// rmcp validates the inbound `Host` header against this list and answers a
/// request it does not recognise with `403 Forbidden: Host header is not
/// allowed`. It has no way to know which address a connection arrived on, so
/// the list has to be built at startup from the one thing that does know: the
/// address the operator told us to bind.
///
/// Three cases, and the third is the reason this is a list rather than a
/// constant:
///
///  - **Loopback.** `localhost`, `127.0.0.1` and `::1` — rmcp's own default,
///    and the only names a client reaching this host can legitimately use.
///  - **A specific interface address.** That address, plus `localhost`: a client
///    that connects to `192.168.1.50:8791` must send that in `Host`, and a
///    client that reaches it as `localhost` is still a client on this host.
///  - **`0.0.0.0` / `::`.** Binding every interface means the address the
///    request arrived on is not knowable here, so **every host is allowed** —
///    the list is empty, which is exactly what rmcp's `disable_allowed_hosts`
///    means and is not recommended for a public deployment. This is the
///    operator's explicit choice, it is printed at startup, and it is recorded
///    as a Divergence in `docs/divergences.md`. No other case opens the server
///    to a name it was not told to serve.
fn allowed_hosts_for(addresses: &[SocketAddr]) -> Vec<String> {
    // `localhost` is always served: a client that reaches this host by name is
    // a client on this host, whatever the listener was bound to.
    let mut allowed = vec!["localhost".to_string()];
    // Every interface was bound if any address is unspecified, so the address a
    // request arrived on is not knowable here and rmcp's empty list is the only
    // correct answer.
    if addresses
        .iter()
        .any(|address| address.ip().is_unspecified())
    {
        return Vec::new();
    }
    for address in addresses {
        let ip = address.ip();
        // The loopback of the bound family only: a client on the other stack
        // could not have reached this listener, so granting its loopback as a
        // Host header grants nothing — and granting nothing keeps the list a
        // statement about what was bound.
        let loopback = match ip {
            IpAddr::V4(_) => Ipv4Addr::LOCALHOST.to_string(),
            IpAddr::V6(_) => Ipv6Addr::LOCALHOST.to_string(),
        };
        for name in [ip.to_string(), loopback] {
            if !allowed.contains(&name) {
                allowed.push(name);
            }
        }
    }
    allowed
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
        "The vault is chosen by the environment, never by a flag. See\n",
        "BASES_MCP_VAULT and the BASES_MCP_WEBDAV_* variables in\n",
        "docs/tooling.md and .env.example.\n",
        "\n",
        "HTTP transport only:\n",
        "\n",
        "  MCP_HTTP_HOST    bind address (default 127.0.0.1). A name rmcp will\n",
        "                   accept is derived from it: a specific address is\n",
        "                   served on that address, and 0.0.0.0 serves any Host.\n",
        "  MCP_HTTP_PORT    bind port (default 8765)\n",
        "\n",
        "  The endpoint is http://HOST:PORT/mcp\n",
        "  A refused host or port names the variable and the value.\n",
    )
}

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
    fn an_unknown_argument_is_refused_by_name() {
        let refusal = select_transport(args(&["--port", "8080"])).expect_err("no such flag");
        assert!(
            refusal.contains("--port"),
            "the refusal names the flag: {refusal}"
        );
    }

    #[test]
    fn the_default_endpoint_binds_loopback_and_serves_its_own_names() {
        let endpoint = HttpEndpoint::from_env().expect("an empty environment is loopback");
        assert_eq!(
            endpoint.address(),
            SocketAddr::from(([127, 0, 0, 1], 8765)),
            "unset is 127.0.0.1:8765"
        );
        let mut names = endpoint.allowed_hosts().to_vec();
        names.sort();
        // Loopback is IPv4 here, so the IPv4 loopback is served and the IPv6 one
        // is not: the other stack could not have reached this listener.
        assert_eq!(names, ["127.0.0.1", "localhost"]);
    }

    #[test]
    fn a_specific_bind_address_is_served_on_that_address() {
        // This test cannot set MCP_HTTP_HOST: the environment is process-global
        // and other tests in this binary read it. `allowed_hosts_for` is the
        // pure half of the same rule, so it is what is asserted here; the
        // environment half is covered end to end by tests/http_transport.rs,
        // which spawns the binary with a real MCP_HTTP_HOST.
        let bound = SocketAddr::from(([192, 168, 1, 50], 8791));
        let endpoint = HttpEndpoint {
            addresses: vec![bound],
            allowed_hosts: allowed_hosts_for(&[bound]),
        };
        assert!(
            endpoint
                .allowed_hosts()
                .contains(&"192.168.1.50".to_string()),
            "the bound address is accepted in a Host header: {:?}",
            endpoint.allowed_hosts()
        );
        assert!(
            endpoint.allowed_hosts().contains(&"localhost".to_string()),
            "localhost still reaches a bound specific address: {:?}",
            endpoint.allowed_hosts()
        );
        assert!(
            !endpoint
                .allowed_hosts()
                .contains(&"192.168.1.99".to_string()),
            "another address does not: {:?}",
            endpoint.allowed_hosts()
        );
    }

    #[test]
    fn binding_every_interface_allows_every_host_and_says_so() {
        let bound = SocketAddr::from(([0, 0, 0, 0], 8765));
        let endpoint = HttpEndpoint {
            addresses: vec![bound],
            allowed_hosts: allowed_hosts_for(&[bound]),
        };
        assert!(
            endpoint.allowed_hosts().is_empty(),
            "0.0.0.0 serves any Host, which rmcp spells as an empty list: {:?}",
            endpoint.allowed_hosts()
        );
    }

    #[test]
    fn a_resolvable_name_binds_and_a_name_that_does_not_resolve_is_refused() {
        // `localhost` is the name the review named: it must resolve, not be
        // refused. The refusal case is a name nothing resolves.
        let addresses = HttpEndpoint::resolve(Host::Name("localhost".to_string()), 8765)
            .expect("localhost resolves");
        assert!(
            addresses.iter().any(|address| address.ip().is_loopback()),
            "localhost resolves to a loopback address: {addresses:?}"
        );

        let refusal = HttpEndpoint::resolve(Host::Name("no-such-host.invalid".to_string()), 8765)
            .expect_err("a name nothing resolves is a refusal");
        assert!(
            refusal.contains("MCP_HTTP_HOST"),
            "the refusal names the variable: {refusal}"
        );
        assert!(
            refusal.contains("no-such-host.invalid"),
            "the refusal names the value: {refusal}"
        );
    }

    #[test]
    fn a_name_with_several_addresses_resolves_to_all_of_them() {
        // The reason `resolve` returns a slice rather than one address: a
        // dual-stack name must bind both stacks, or a client that resolved the
        // other one is refused.
        let addresses = HttpEndpoint::resolve(Host::Name("localhost".to_string()), 8765)
            .expect("localhost resolves");
        assert!(
            !addresses.is_empty(),
            "localhost resolves to at least one address: {addresses:?}"
        );
        assert!(
            addresses.iter().all(|address| address.ip().is_loopback()),
            "every address of `localhost` is loopback: {addresses:?}"
        );
        let unique: std::collections::HashSet<_> = addresses.iter().collect();
        assert_eq!(
            addresses.len(),
            unique.len(),
            "no address is returned twice: {addresses:?}"
        );
    }

    #[test]
    fn a_host_and_port_together_is_refused_rather_than_resolved_as_a_name() {
        // The variable reads like a full address, so `127.0.0.1:8765` is the
        // mistake operators make. Carried as a name it would be looked up as a
        // hostname and refused with a message about a name that does not
        // resolve — which names the wrong mistake.
        let refusal = parse_host(Some("127.0.0.1:8765".to_string()))
            .expect_err("a host and a port is not a host");
        assert!(
            refusal.contains("MCP_HTTP_PORT"),
            "the refusal names where the port belongs: {refusal}"
        );
        assert!(
            refusal.contains("127.0.0.1:8765"),
            "the refusal names the value: {refusal}"
        );
    }

    #[test]
    fn an_ipv6_host_with_port_is_refused_too() {
        let refusal = parse_host(Some("[::1]:8765".to_string()))
            .expect_err("an ipv6 host and a port is not a host");
        assert!(refusal.contains("MCP_HTTP_PORT"), "{refusal}");
    }

    #[test]
    fn a_bare_name_is_a_name_and_a_bare_ip_is_an_ip() {
        assert!(matches!(
            parse_host(Some("localhost".to_string())),
            Ok(Host::Name(_))
        ));
        assert!(matches!(
            parse_host(Some("127.0.0.1".to_string())),
            Ok(Host::Ip(_))
        ));
        assert!(matches!(parse_host(None), Ok(Host::Ip(_))));
    }

    #[test]
    fn the_loopback_of_the_bound_family_is_allowed_and_the_other_is_not() {
        let v4 = allowed_hosts_for(&[SocketAddr::from(([192, 168, 1, 50], 8791))]);
        assert!(v4.contains(&"127.0.0.1".to_string()), "{v4:?}");
        assert!(!v4.contains(&"::1".to_string()), "{v4:?}");

        let v6 = allowed_hosts_for(&[SocketAddr::from(([0xfd00, 0, 0, 0, 0, 0, 0, 1], 8791))]);
        assert!(v6.contains(&"::1".to_string()), "{v6:?}");
        assert!(!v6.contains(&"127.0.0.1".to_string()), "{v6:?}");
    }

    #[test]
    fn a_blank_port_is_refused_rather_than_defaulted() {
        // `filled` is the rule `config::parse_config` applies to a selector, so
        // the blank case is exercised here rather than by a subprocess: the
        // refusal text is the same code path.
        //
        // Over the value, not over the environment. `MCP_HTTP_PORT` is
        // process-global, and this binary's other tests read it — the test
        // above asserts that an unset environment is loopback:8765 — so a test
        // that set it would be racing them. `parse_port` is the half that
        // holds the rule, the way `config`'s `Env` map is the half that holds
        // that parse.
        let refusal = parse_port("   ").expect_err("a blank port is not an absence");
        assert!(
            refusal.contains("MCP_HTTP_PORT"),
            "names the variable: {refusal}"
        );
    }
}
