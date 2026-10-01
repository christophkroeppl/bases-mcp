//! Startup configuration.
//!
//! The one thing this must get right is refusing to start against a vault the
//! caller did not name. A server that quietly falls back to some other vault
//! answers questions from it, and the client has no way to tell — which is the
//! single confusion this project exists to prevent.
//!
//! Two variables SELECT a backend, `BASES_MCP_VAULT` and
//! `BASES_MCP_WEBDAV_URL`, so a blank one is an error rather than an absence:
//! the intent is unmistakable and "no vault configured" would send whoever typo'd
//! it looking in the wrong place. A blank CREDENTIAL is just an absent
//! credential, and falls through to the same message an unset one would.
//!
//! `BASES_MCP_VAULT` wins when both are configured. That precedence is not
//! silent — the caller is handed a warning, since a WebDAV URL that quietly does
//! nothing is its own kind of surprise.
//!
//! A URL is operator-supplied and this file builds error messages out of it, so
//! credentials in a URL are refused rather than honoured: they would ride along
//! in every string derived from it, including the messages printed to stderr.
//! Every URL this module echoes is redacted first.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;

use crate::error::BasesError;
use crate::vault::{WebdavVaultOptions, WebdavVaultSource};

/// Prefix on every line this process writes to stderr.
pub const NAME: &str = "bases-mcp";

/// The environment this reads.
///
/// A map rather than `std::env::vars()` directly, so parsing is testable
/// without spawning a process — which is the property that made the TypeScript
/// original's configuration testable at all.
pub type Env = BTreeMap<String, String>;

/// Read the process environment.
pub fn process_env() -> Env {
    std::env::vars().collect()
}

/// The vault this process will serve for its whole life.
///
/// A union rather than one shape with optional fields, so an fs-only path cannot
/// be handed a WebDAV URL and read a wrong directory: the two have nothing in
/// common to default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VaultConfig {
    Fs {
        /// Absolute path to the vault directory.
        dir: PathBuf,
    },
    Webdav {
        /// Base URL of the WebDAV collection backing the vault.
        url: String,
        /// Basic-auth username. Never logged, never echoed in an error.
        user: String,
        /// Basic-auth password. Never logged, never echoed in an error.
        password: String,
    },
}

impl VaultConfig {
    /// A word for the backend, for a diagnostic that never carries a secret.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Fs { .. } => "fs",
            Self::Webdav { .. } => "webdav",
        }
    }
}

/// A parsed environment, or the one thing wrong with it.
///
/// The failure carries a finished, user-facing message rather than a field
/// describing what was missing, because there is exactly one consumer and it
/// only ever prints. Nothing downstream re-derives what went wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigResult {
    Ok {
        config: VaultConfig,
        warnings: Vec<String>,
    },
    Err {
        message: String,
    },
}

/// A variable with content in it: present, non-blank, trimmed.
fn filled(env: &Env, name: &str) -> Option<String> {
    env.get(name)
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// Read the vault configuration out of the environment.
pub fn parse_config(env: &Env) -> ConfigResult {
    for name in ["BASES_MCP_VAULT", "BASES_MCP_WEBDAV_URL"] {
        if env.contains_key(name) && filled(env, name).is_none() {
            return ConfigResult::Err {
                message: format!("{name} is set but blank. Unset it, or give it a value."),
            };
        }
    }

    if let Some(dir) = filled(env, "BASES_MCP_VAULT") {
        let ignored = webdav_variables(env);
        let warnings = if ignored.is_empty() {
            Vec::new()
        } else {
            vec![format!(
                "{} {} also set and ignored: BASES_MCP_VAULT wins.",
                list(&ignored),
                plural(&ignored, "is", "are")
            )]
        };
        return ConfigResult::Ok {
            config: VaultConfig::Fs {
                dir: absolute(Path::new(&dir)),
            },
            warnings,
        };
    }

    if let Some(url) = filled(env, "BASES_MCP_WEBDAV_URL") {
        let user = filled(env, "BASES_MCP_WEBDAV_USER");
        let password = filled(env, "BASES_MCP_WEBDAV_PASSWORD");
        let missing = missing_webdav_credentials(&user, &password);
        if !missing.is_empty() {
            return ConfigResult::Err {
                message: format!(
                    "BASES_MCP_WEBDAV_URL is set but {} {} not. A WebDAV vault needs both, \
                     because an unauthenticated request would read a 401 as an empty vault.",
                    list(&missing),
                    plural(&missing, "is", "are")
                ),
            };
        }
        // Unreachable by construction: `missing` is empty exactly when both are
        // present, and the message above is the only path that reports otherwise.
        let (Some(user), Some(password)) = (user, password) else {
            return ConfigResult::Err {
                message: usage().to_string(),
            };
        };
        let Some(parsed) = parse_webdav_url(&url) else {
            return ConfigResult::Err {
                message: invalid_webdav_url(&url),
            };
        };
        return ConfigResult::Ok {
            config: VaultConfig::Webdav {
                url: parsed,
                user,
                password,
            },
            warnings: Vec::new(),
        };
    }

    let orphans = webdav_variables(env);
    if !orphans.is_empty() {
        return ConfigResult::Err {
            message: format!(
                "{} {} set without BASES_MCP_WEBDAV_URL. Set BASES_MCP_WEBDAV_URL to the base \
                 URL of the WebDAV collection backing the vault.",
                list(&orphans),
                plural(&orphans, "is", "are")
            ),
        };
    }

    ConfigResult::Err {
        message: usage().to_string(),
    }
}

/// The WebDAV variables that carry a value, in the order the usage text lists them.
fn webdav_variables(env: &Env) -> Vec<&'static str> {
    [
        "BASES_MCP_WEBDAV_URL",
        "BASES_MCP_WEBDAV_USER",
        "BASES_MCP_WEBDAV_PASSWORD",
    ]
    .into_iter()
    .filter(|name| filled(env, name).is_some())
    .collect()
}

/// Join names the way a sentence does: "A", "A and B", "A, B and C".
fn list(names: &[&str]) -> String {
    match names {
        [] => String::new(),
        [one] => (*one).to_string(),
        [one, two] => format!("{one} and {two}"),
        many => format!(
            "{} and {}",
            many[..many.len() - 1].join(", "),
            many[many.len() - 1]
        ),
    }
}

/// "is" for one name, "are" for more, so the sentence is never ungrammatical.
fn plural(names: &[&str], one: &'static str, many: &'static str) -> &'static str {
    if names.len() == 1 {
        one
    } else {
        many
    }
}

/// The credentials a WebDAV configuration is missing, in the order usage lists them.
fn missing_webdav_credentials(
    user: &Option<String>,
    password: &Option<String>,
) -> Vec<&'static str> {
    [
        user.is_none().then_some("BASES_MCP_WEBDAV_USER"),
        password.is_none().then_some("BASES_MCP_WEBDAV_PASSWORD"),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// What the WebDAV case has to say, whichever variable revealed it.
///
/// The refusal to fall back is stated explicitly because the fallback is the
/// obvious "fix" and it is wrong: it would answer queries from a directory
/// nobody asked for, and the client would have no way to tell.
fn invalid_webdav_url(url: &str) -> String {
    // The URL is echoed with any userinfo REDACTED. Naming the rejected URL is
    // useful; quoting its password is not.
    format!(
        "BASES_MCP_WEBDAV_URL is {redacted}, which is not a usable WebDAV base URL.\n\
         It must be an http(s) URL with no credentials in it: `http://host:port/path/`.\n\
         Credentials in the URL would end up in any message built from it, so put them in\n\
         BASES_MCP_WEBDAV_USER and BASES_MCP_WEBDAV_PASSWORD instead.",
        redacted = redact_url(url)
    )
}

/// Replace any `user:password@` in a URL with `***:***@`.
fn redact_url(url: &str) -> String {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN
        .get_or_init(|| Regex::new(r"//[^/@]*@").expect("the userinfo pattern compiles"))
        .replace_all(url, "//***:***@")
        .into_owned()
}

/// Normalise the WebDAV base URL, or refuse it.
///
/// Returns `None` for anything we cannot serve from, and a href-safe form for
/// what we can: a trailing slash, because the backend appends collection names
/// to this and a missing slash would make `/vaultProjects` out of `/vault` plus
/// `Projects`.
fn parse_webdav_url(url: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(url).ok()?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return None;
    }
    // Userinfo in the URL is refused, not honoured: it would ride along in every
    // string this file builds, including error messages.
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return None;
    }
    let path = parsed.path();
    let path = if path.ends_with('/') {
        path.to_string()
    } else {
        format!("{path}/")
    };
    // Only the origin and the path survive. A query string or a fragment would
    // be dropped on the first request anyway, and carrying one here would make
    // the configured URL differ from the URL actually requested.
    Some(format!("{}{path}", parsed.origin().ascii_serialization()))
}

/// `path.resolve()`: absolute against the working directory.
///
/// Folding of `.` and `..` is left to the backend, which already does it
/// lexically. A process with no working directory has no absolute vault path to
/// offer, and the backend's own refusal is a better message than a guess made
/// here.
fn absolute(dir: &Path) -> PathBuf {
    if dir.is_absolute() {
        return dir.to_path_buf();
    }
    match std::env::current_dir() {
        Ok(cwd) => cwd.join(dir),
        Err(_) => dir.to_path_buf(),
    }
}

const USAGE: &str = concat!(
    "No vault configured. This server serves exactly one vault, and it must be told which.\n",
    "\n",
    "  BASES_MCP_VAULT             path to a vault directory on the local filesystem\n",
    "\n",
    "  BASES_MCP_WEBDAV_URL        WebDAV base URL, no credentials in it\n",
    "  BASES_MCP_WEBDAV_USER       WebDAV username\n",
    "  BASES_MCP_WEBDAV_PASSWORD   WebDAV password\n",
    "\n",
    "  Exactly one backend is used. BASES_MCP_VAULT wins if both are set.\n",
    "\n",
    "  WebDAV example:\n",
    "  BASES_MCP_WEBDAV_URL=http://localhost:5000/vault BASES_MCP_WEBDAV_USER=u ",
    "BASES_MCP_WEBDAV_PASSWORD=p bases-mcp\n",
    "\n",
    "Example:\n",
    "  BASES_MCP_VAULT=/path/to/vault bases-mcp",
);

/// The usage message, for a caller that wants to print it itself.
pub fn usage() -> &'static str {
    USAGE
}

// ---------------------------------------------------------------------------
// Opening the vault
// ---------------------------------------------------------------------------

/// The backend a configuration asks for, before it is known to be reachable.
///
/// Split from [`crate::service::Resolver::open`] because the directory check
/// has to happen BEFORE the resolver opens it, and because a test wants the
/// backend without indexing anything.
pub fn open_source(
    config: &VaultConfig,
) -> crate::error::Result<Box<dyn crate::vault::VaultSource>> {
    match config {
        VaultConfig::Fs { dir } => {
            // `FsVaultSource` swallows a failed `readdir`, so a mistyped
            // `BASES_MCP_VAULT` would otherwise index as a vault with no notes
            // and every Base would come back with zero rows. An empty vault is
            // indistinguishable from a vault that genuinely matches nothing,
            // which is the one confusion this server exists to avoid.
            let stats = std::fs::metadata(dir).map_err(|error| {
                BasesError::new(format!("cannot open vault at {}: {error}", dir.display()))
                    .with_construct("BASES_MCP_VAULT")
            })?;
            if !stats.is_dir() {
                return Err(BasesError::new(format!(
                    "cannot open vault at {}: not a directory",
                    dir.display()
                ))
                .with_construct("BASES_MCP_VAULT"));
            }
            Ok(Box::new(crate::vault::FsVaultSource::new(dir)?))
        }
        VaultConfig::Webdav {
            url,
            user,
            password,
        } => {
            // The URL is safe to name; the credentials are never in this
            // string, and the backend is built so they cannot be.
            let source = WebdavVaultSource::new(
                WebdavVaultOptions::new(url.clone())
                    .with_user(user.clone())
                    .with_password(password.clone()),
            )
            .map_err(|error| {
                // Redacted, like every other URL this file echoes: the caller
                // of `open_source` is not `parse_config`, so the URL is not
                // guaranteed to have been through the credential check.
                BasesError::new(format!(
                    "cannot open WebDAV vault at {url}: {error}",
                    url = redact_url(url)
                ))
                .with_construct("BASES_MCP_WEBDAV_URL")
            })?;
            Ok(Box::new(source))
        }
    }
}
