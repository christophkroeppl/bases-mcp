//! The audit trail: who called what, when, and what came of it.
//!
//! The gap this file closes is the one D9 records — a server whose HTTP
//! transport is reachable by anyone who can open a socket, with no record of
//! which caller did what. An audit trail is not a CONTROL: it does not decide
//! who may call a tool, and it does not make the transport safe. What it does
//! is leave a record that outlives the call.
//!
//! # What a record holds
//!
//! One JSON line per tool call, and one for the session's opening:
//!
//! ```text
//! {"ts":"2026-10-22T09:15:00.123Z","tool":"write_note","caller":"claude-code 1.2.3",
//!  "transport":"http","session":"9f3c","peer":"192.168.1.5:54321",
//!  "target":"Notes/Todo.md","outcome":"ok","duration_ms":14}
//! ```
//!
//! `caller` is the client's name and version, from the MCP handshake, joined
//! by a space — the two fields the handshake carries, and not one string a
//! caller shaped freely.
//! `session` and `peer` are the HTTP transport's, and are absent over stdio.
//! `target` is the vault path the call acted on, RESOLVED rather than as
//! asked for — so the trail says which file was touched, not which spelling
//! of the name the caller used.
//!
//! The opening record is the same shape with `tool` = `initialize`: written
//! when the client completes the handshake, with no target. It is what makes
//! a caller that connects and calls nothing visible.
//!
//! # What a record NEVER holds
//!
//! - **Note contents.** `write_note` carries the agent's text in its
//!   ARGUMENTS and `get_note` carries the note's content in its ANSWER.
//!   Neither the arguments nor the answers are recorded; only the path.
//! - **Credentials.** The WebDAV password is read from the environment and
//!   never reaches this module; there is nothing here that could hold it.
//! - **Error prose.** An error message can quote a fragment of the file that
//!   failed to parse. The trail records the CONSTRUCT a failure was about —
//!   a short token of our own — and not the sentence.
//!
//! # Forgery
//!
//! A caller controls the `path` argument, so a caller could try to put a
//! newline into the trail and forge a line. JSON Lines defeats that: the
//! record is one JSON value, so a newline inside a string is escaped and the
//! file stays one record per line. Lengths are capped as well, so a caller
//! cannot bloat the trail with a megabyte of "path".
//!
//! # Where it goes
//!
//! `MCP_AUDIT_LOG` names a file, and the file rolls over at
//! `MCP_AUDIT_LOG_MAX_MB` megabytes. With no file configured the trail goes
//! to stderr, which nothing rotates — a journald or a container runtime owns
//! that. The deployment configures the file (see `compose.yaml`), and the
//! rotation guarantee is the file sink's.

use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use rmcp::Peer;
use rmcp::RoleServer;
use rmcp::model::Extensions as ContextExtensions;
use rmcp::service::{NotificationContext, RequestContext};
use serde::Serialize;

use crate::config::{Env, NAME};

/// The rotation threshold the binary ships, in megabytes.
///
/// Small on purpose. The trail is a record of CALLS, and a call is a few
/// hundred bytes, so five megabytes is tens of thousands of calls — more than
/// a busy vault produces in a day — while being small enough to keep on the
/// same volume as everything else. An operator who wants a longer tail
/// raises `MCP_AUDIT_LOG_MAX_MB`; the trail keeps ONE previous generation,
/// so the disk cost is twice the threshold however large it is.
pub const DEFAULT_MAX_MB: u64 = 5;

/// One mebibyte, in bytes.
const MB: u64 = 1024 * 1024;

/// How many characters of a caller-supplied path are kept.
const TARGET_MAX: usize = 256;
/// How many characters of a client name are kept.
const CLIENT_MAX: usize = 128;
/// How many characters of a session id are kept.
const SESSION_MAX: usize = 128;
/// How many characters of a peer address are kept.
const PEER_MAX: usize = 64;
/// How many characters of a construct are kept.
const ERROR_MAX: usize = 64;
/// How many characters of a tool name are kept.
const TOOL_MAX: usize = 64;

/// What became of a call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallOutcome {
    /// It did what it was asked to do.
    Ok,
    /// It did what it was asked to do, and refused part of it.
    Partial,
    /// It could not do its job.
    Failed,
    /// The name did not route to a tool at all.
    UnknownTool,
}

impl CallOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Partial => "partial-with-errors",
            Self::Failed => "failed",
            Self::UnknownTool => "unknown-tool",
        }
    }
}

/// Who made the call.
///
/// Every field is best-effort and none of them is an AUTHENTICATION: the
/// client name is whatever the caller said it was, and the session id is
/// whatever it sent. The peer address is the only unforgeable one of the
/// three, and only the HTTP transport has it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuditContext {
    /// The client's name and version, from the MCP handshake.
    pub client: Option<String>,
    /// The MCP session id, when the transport has one.
    pub session: Option<String>,
    /// The remote address the call arrived from, when the transport has one.
    pub peer: Option<String>,
}

impl AuditContext {
    /// The one-word name the record carries.
    ///
    /// The client's own name where the handshake supplied one, and
    /// `unknown` where it did not — which is the honest answer for a call
    /// that arrived before `initialize`, and the only answer available on a
    /// transport that never handshook at all.
    pub fn label(&self) -> String {
        match &self.client {
            Some(client) => sanitize(client, CLIENT_MAX),
            None => "unknown".to_string(),
        }
    }

    /// The identity behind a request, as the transport knows it.
    ///
    /// Three sources, and they are not equally trustworthy:
    ///
    ///  - the CLIENT NAME is what the caller said it was. It is the most
    ///    useful thing in the record and the least reliable.
    ///  - the SESSION ID is the transport's own. Over HTTP it is a token the
    ///    server issued, so it is a handle rather than a claim; over stdio
    ///    there is no session at all.
    ///  - the PEER ADDRESS is the only unforgeable one, and only the HTTP
    ///    transport has it.
    ///
    /// Nothing here authenticates anything. The trail says who claimed to
    /// call what, from where.
    pub fn from_request(context: &RequestContext<RoleServer>) -> Self {
        Self::from_parts(&context.extensions, &context.peer)
    }

    /// The identity behind a NOTIFICATION, which is the handshake.
    ///
    /// A client announces itself by calling `initialize` and then sending
    /// `notifications/initialized`; it is the notification that
    /// `ServerHandler::on_initialized` reports, and that is where the trail
    /// writes the session's opening record. The two contexts carry the same
    /// extensions and the same peer, so they read the same way.
    pub fn from_notification(context: &NotificationContext<RoleServer>) -> Self {
        Self::from_parts(&context.extensions, &context.peer)
    }

    /// The one implementation both contexts share.
    ///
    /// The extension map is rmcp's own copy of `http::Extensions`, so it is
    /// named here rather than confused with the `http` one.
    fn from_parts(extensions: &ContextExtensions, peer: &Peer<RoleServer>) -> Self {
        // The handshake's client info: the name and version the client
        // announced. Absent for a call that arrived before `initialize`,
        // which is why the label falls back to `unknown` rather than
        // guessing.
        let client = peer
            .peer_info()
            .map(|info| format!("{} {}", info.client_info.name, info.client_info.version));
        // The session id: the header the HTTP transport reads, so it is the
        // transport's own token rather than the caller's claim about it.
        let session = extensions.get::<http::request::Parts>().and_then(|parts| {
            parts
                .headers
                .get("mcp-session-id")
                .and_then(|value| value.to_str().ok())
                .map(str::to_string)
        });
        // The peer address: axum's connect info, which is the socket the
        // request arrived on. It rides in the PARTS' extension map rather
        // than the request context's: axum puts it on the HTTP request, and
        // rmcp puts the PARTS on the context.
        let peer = extensions
            .get::<http::request::Parts>()
            .and_then(|parts| {
                parts
                    .extensions
                    .get::<axum::extract::ConnectInfo<SocketAddr>>()
            })
            .map(|info| info.0.to_string());
        Self {
            client,
            session,
            peer,
        }
    }
}

/// One tool call, as the trail records it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AuditRecord {
    /// When the call finished, RFC 3339 with milliseconds, in UTC.
    pub ts: String,
    /// The tool that was called.
    pub tool: String,
    /// Who called it.
    pub caller: String,
    /// Which transport the call arrived on.
    pub transport: String,
    /// The MCP session id, when the transport has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// The remote address, when the transport has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peer: Option<String>,
    /// The vault path the call acted on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// What became of it.
    pub outcome: String,
    /// The construct a failure was about.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// How long the call took, in milliseconds.
    pub duration_ms: u64,
}

impl AuditRecord {
    /// A record for a call, with every caller-influenced field sanitised.
    pub fn new(context: &AuditContext, transport: &str, tool: &str, outcome: CallOutcome) -> Self {
        Self {
            ts: now_iso(),
            tool: sanitize(tool, TOOL_MAX),
            caller: context.label(),
            transport: transport.to_string(),
            session: context.session.as_deref().map(|s| sanitize(s, SESSION_MAX)),
            peer: context.peer.as_deref().map(|p| sanitize(p, PEER_MAX)),
            target: None,
            outcome: outcome.as_str().to_string(),
            error: None,
            duration_ms: 0,
        }
    }

    /// The vault path the call acted on.
    pub fn with_target(mut self, target: Option<&str>) -> Self {
        self.target = target.map(|target| sanitize(target, TARGET_MAX));
        self
    }

    /// The construct a failure was about.
    pub fn with_error(mut self, error: Option<&str>) -> Self {
        self.error = error.map(|error| sanitize(error, ERROR_MAX));
        self
    }

    /// How long the call took.
    pub fn with_duration(mut self, duration: Duration) -> Self {
        self.duration_ms = u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
        self
    }

    /// The record as one line of JSON.
    pub fn to_line(&self) -> Result<String, String> {
        serde_json::to_string(self).map_err(|error| error.to_string())
    }
}

/// A value a caller controls, made safe to put in the trail.
///
/// Two properties, and both are about the trail's INTEGRITY rather than its
/// tidiness. A newline in a path would forge a line — JSON Lines already
/// escapes that, and this is the second half of the same defence: the
/// control characters go, so the file stays readable. And a megabyte of
/// "path" would bloat a file that is supposed to be a record of calls.
fn sanitize(value: &str, max: usize) -> String {
    let mut out = String::new();
    // The index counts the characters already KEPT, which is the same as the
    // number pushed, because nothing is dropped without breaking out.
    for (kept, character) in value.chars().enumerate() {
        if kept >= max {
            out.push('…');
            break;
        }
        match character {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            control if control.is_control() => out.push('?'),
            other => out.push(other),
        }
    }
    out
}

/// The current instant, as RFC 3339 with milliseconds, in UTC.
fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Say on stderr that a record was lost.
///
/// The trail is not allowed to break the call it is recording: a full disk
/// must not turn a successful write into a failed one, because that would
/// teach an agent to retry a write that already landed. The loss is
/// reported instead, on stderr, where the operator will see it.
pub fn report_lost(problem: &str) {
    eprintln!("{NAME}: the audit trail lost a record: {problem}");
}

/// Where the trail is written.
pub trait AuditSink: fmt::Debug + Send + Sync {
    /// Write one record.
    ///
    /// An error means the record was LOST. The caller reports it and
    /// carries on.
    fn write(&self, record: &AuditRecord) -> Result<(), String>;
}

/// The trail, on stderr.
///
/// The default when no file is configured. Nothing rotates it — a journald or
/// a container runtime owns that — which is the reason the deployment
/// configures a file instead.
#[derive(Debug, Default)]
pub struct StderrSink;

impl AuditSink for StderrSink {
    fn write(&self, record: &AuditRecord) -> Result<(), String> {
        let line = record.to_line()?;
        let mut stderr = std::io::stderr().lock();
        writeln!(stderr, "{line}").map_err(|error| error.to_string())
    }
}

/// The trail, in a file that rolls over at a size.
///
/// One generation is kept beside the current one, so the disk cost is twice
/// the threshold and never more: the previous generation goes when the next
/// one is made. The CURRENT file never exceeds the threshold — the size is
/// checked before the write, not after — so a reader that tails the current
/// file sees a bounded thing.
#[derive(Debug)]
pub struct FileSink {
    path: PathBuf,
    max_bytes: u64,
    state: Mutex<FileState>,
}

/// The open file and how much of it there is.
#[derive(Debug)]
struct FileState {
    file: File,
    size: u64,
}

impl FileSink {
    /// Open the trail, creating it if it is not there.
    ///
    /// The parent directory is created too, because a log path that names a
    /// directory nobody made is a path the operator expected to work. The
    /// file is created 0600 on a unix host: the trail says who wrote what to
    /// a vault, which is as sensitive as the vault's own history.
    pub fn open(path: impl AsRef<Path>, max_bytes: u64) -> Result<Self, String> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent).map_err(|error| {
                format!(
                    "cannot create the audit log directory {}: {error}",
                    parent.display()
                )
            })?;
        }
        let file = open_file(&path, false)?;
        let size = file
            .metadata()
            .map(|metadata| metadata.len())
            .unwrap_or_default();
        Ok(Self {
            path,
            max_bytes,
            state: Mutex::new(FileState { file, size }),
        })
    }

    /// The path the trail is written to.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The size at which the trail rolls over.
    pub fn max_bytes(&self) -> u64 {
        self.max_bytes
    }

    /// The path the previous generation is kept at.
    fn backup_path(&self) -> PathBuf {
        let mut name = self.path.as_os_str().to_os_string();
        name.push(".1");
        PathBuf::from(name)
    }

    /// Roll the trail over: the current file becomes the previous
    /// generation, and a new one takes its place.
    fn rotate(&self, state: &mut FileState) -> Result<(), String> {
        state.file.flush().map_err(|error| {
            format!(
                "cannot flush the audit log {}: {error}",
                self.path.display()
            )
        })?;
        let backup = self.backup_path();
        // The previous generation goes: the trail keeps ONE generation, so
        // the disk cost is bounded at twice the threshold.
        let _ = std::fs::remove_file(&backup);
        match std::fs::rename(&self.path, &backup) {
            Ok(()) => {}
            // Nothing to rotate is not a failure: the trail is empty.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "cannot rotate the audit log {}: {error}",
                    self.path.display()
                ));
            }
        }
        state.file = open_file(&self.path, true)?;
        state.size = 0;
        Ok(())
    }
}

impl AuditSink for FileSink {
    fn write(&self, record: &AuditRecord) -> Result<(), String> {
        let line = record.to_line()?;
        let mut bytes = line.into_bytes();
        bytes.push(b'\n');
        let mut state = self
            .state
            .lock()
            .map_err(|_| "the audit log lock is poisoned".to_string())?;
        // The size is checked BEFORE the write, so the file never grows past
        // the threshold by more than the record being written. A record
        // larger than the whole threshold is still written — rotating on it
        // would rotate forever.
        if state.size > 0 && state.size + bytes.len() as u64 > self.max_bytes {
            self.rotate(&mut state)?;
        }
        state.file.write_all(&bytes).map_err(|error| {
            format!(
                "cannot write the audit log {}: {error}",
                self.path.display()
            )
        })?;
        // Flushed per record: an audit trail that lost its last lines to a
        // crash is not a trail. Not fsynced: the cost of an fsync per tool
        // call is a disk round trip, and the trail's job is to survive a
        // process crash, not a power cut.
        state.file.flush().map_err(|error| {
            format!(
                "cannot flush the audit log {}: {error}",
                self.path.display()
            )
        })?;
        state.size += bytes.len() as u64;
        Ok(())
    }
}

/// Open the trail file, creating it if it is not there.
///
/// Two modes, and the difference is more than cosmetic: `append` and
/// `truncate` together are an INVALID combination in `OpenOptions` — the
/// standard library refuses it with "creating or truncating a file requires
/// write or append access" — so a fresh generation is opened by truncating
/// and the current one by appending.
fn open_file(path: &Path, truncate: bool) -> Result<File, String> {
    let mut options = OpenOptions::new();
    options.write(true).create(true);
    if truncate {
        options.truncate(true);
    } else {
        options.append(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // The trail says who wrote what to a vault. It is created readable
        // by its owner only, and an operator who wants it collected can
        // widen that deliberately.
        options.mode(0o600);
    }
    options
        .open(path)
        .map_err(|error| format!("cannot open the audit log {}: {error}", path.display()))
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Where the trail goes, and how big it may get.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditConfig {
    /// The file the trail is written to. `None` means stderr.
    pub path: Option<PathBuf>,
    /// The size at which the file rolls over.
    pub max_bytes: u64,
}

/// A parsed environment, or the one thing wrong with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuditConfigResult {
    Ok {
        config: AuditConfig,
        warnings: Vec<String>,
    },
    Err {
        message: String,
    },
}

/// Read the audit configuration out of the environment.
///
/// The same rule as `config::parse_config` and `transport::read_port`: a
/// variable that is set and wrong is a refusal naming the variable and the
/// value, never a silent default. A blank path is refused rather than read as
/// "no file", because an operator who sets the variable means a file.
///
/// The threshold's DEFAULT lives here, in the binary, and the deployment
/// overrides it through the environment.
pub fn parse_audit_config(env: &Env) -> AuditConfigResult {
    let path = match env.get("MCP_AUDIT_LOG") {
        Some(value) if value.trim().is_empty() => {
            return AuditConfigResult::Err {
                message: "MCP_AUDIT_LOG is set but blank. Unset it to write the trail to \
                          stderr, or give it a path."
                    .to_string(),
            };
        }
        Some(value) => Some(PathBuf::from(value.trim())),
        None => None,
    };

    let max_bytes = match env.get("MCP_AUDIT_LOG_MAX_MB") {
        Some(value) if value.trim().is_empty() => {
            return AuditConfigResult::Err {
                message: format!(
                    "MCP_AUDIT_LOG_MAX_MB is set but blank. Unset it to use the default of \
                     {DEFAULT_MAX_MB} MB, or give it a number of megabytes."
                ),
            };
        }
        Some(value) => {
            let trimmed = value.trim();
            match trimmed.parse::<u64>() {
                // Zero is not a threshold any file can honour: the trail
                // would roll over before it held a single record.
                Ok(0) => {
                    return AuditConfigResult::Err {
                        message: format!(
                            "MCP_AUDIT_LOG_MAX_MB is 0, which no log can honour. Use a number \
                             of megabytes of at least 1, or unset it for the default of \
                             {DEFAULT_MAX_MB}."
                        ),
                    };
                }
                Ok(megabytes) => megabytes.saturating_mul(MB),
                Err(_) => {
                    return AuditConfigResult::Err {
                        message: format!(
                            "MCP_AUDIT_LOG_MAX_MB is {trimmed:?}, which is not a number of \
                             megabytes. Use a whole number of megabytes, or unset it for the \
                             default of {DEFAULT_MAX_MB}."
                        ),
                    };
                }
            }
        }
        None => DEFAULT_MAX_MB.saturating_mul(MB),
    };

    // A threshold with no file has nothing to act on, and the operator who
    // set it was asking for a capped trail. Saying so is the difference
    // between a setting that does nothing and a setting that was ignored.
    let warnings = if path.is_none() && env.contains_key("MCP_AUDIT_LOG_MAX_MB") {
        vec![
            "MCP_AUDIT_LOG_MAX_MB has no effect without MCP_AUDIT_LOG: the trail goes to \
             stderr, and nothing rotates stderr. Set MCP_AUDIT_LOG to a file to get a capped \
             trail."
                .to_string(),
        ]
    } else {
        Vec::new()
    };

    AuditConfigResult::Ok {
        config: AuditConfig { path, max_bytes },
        warnings,
    }
}

impl AuditConfig {
    /// Open the sink this configuration describes.
    ///
    /// A file that cannot be opened is a startup failure, not a warning: a
    /// server whose audit trail silently goes nowhere is the thing this
    /// module exists to prevent, and a half-started server is worse than
    /// one that never ran.
    pub fn open(&self) -> Result<Box<dyn AuditSink>, String> {
        match &self.path {
            Some(path) => Ok(Box::new(FileSink::open(path, self.max_bytes)?)),
            None => Ok(Box::new(StderrSink)),
        }
    }

    /// A line describing where the trail goes, for the startup banner.
    pub fn describe(&self) -> String {
        match &self.path {
            Some(path) => format!("{} (rotates at {} MB)", path.display(), self.max_bytes / MB),
            None => {
                "stderr (nothing rotates stderr — set MCP_AUDIT_LOG for a capped trail)".to_string()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> Env {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
            .collect()
    }

    fn config(pairs: &[(&str, &str)]) -> AuditConfig {
        match parse_audit_config(&env(pairs)) {
            AuditConfigResult::Ok { config, .. } => config,
            AuditConfigResult::Err { message } => panic!("the configuration is refused: {message}"),
        }
    }

    #[test]
    fn the_default_threshold_is_five_megabytes() {
        let config = config(&[]);
        assert_eq!(config.max_bytes, 5 * MB);
        assert_eq!(DEFAULT_MAX_MB, 5);
    }

    #[test]
    fn no_file_means_the_trail_goes_to_stderr() {
        let config = config(&[]);
        assert_eq!(config.path, None);
    }

    #[test]
    fn a_file_is_where_the_trail_goes() {
        let config = config(&[("MCP_AUDIT_LOG", "/var/log/bases-mcp/audit.log")]);
        assert_eq!(
            config.path,
            Some(PathBuf::from("/var/log/bases-mcp/audit.log"))
        );
    }

    #[test]
    fn the_threshold_is_read_in_megabytes() {
        let config = config(&[
            ("MCP_AUDIT_LOG", "/tmp/audit.log"),
            ("MCP_AUDIT_LOG_MAX_MB", "12"),
        ]);
        assert_eq!(config.max_bytes, 12 * MB);
    }

    #[test]
    fn a_blank_path_is_refused_rather_than_read_as_no_file() {
        let refusal = match parse_audit_config(&env(&[("MCP_AUDIT_LOG", "   ")])) {
            AuditConfigResult::Err { message } => message,
            AuditConfigResult::Ok { .. } => panic!("a blank path is not an absence"),
        };
        assert!(
            refusal.contains("MCP_AUDIT_LOG"),
            "the refusal names the variable: {refusal}"
        );
    }

    #[test]
    fn a_blank_threshold_is_refused_rather_than_read_as_the_default() {
        let refusal = match parse_audit_config(&env(&[("MCP_AUDIT_LOG_MAX_MB", "  ")])) {
            AuditConfigResult::Err { message } => message,
            AuditConfigResult::Ok { .. } => panic!("a blank threshold is not an absence"),
        };
        assert!(
            refusal.contains("MCP_AUDIT_LOG_MAX_MB"),
            "the refusal names the variable: {refusal}"
        );
    }

    #[test]
    fn a_threshold_that_is_not_a_number_is_refused() {
        let refusal = match parse_audit_config(&env(&[("MCP_AUDIT_LOG_MAX_MB", "big")])) {
            AuditConfigResult::Err { message } => message,
            AuditConfigResult::Ok { .. } => panic!("a word is not a number of megabytes"),
        };
        assert!(
            refusal.contains("big"),
            "the refusal names the value: {refusal}"
        );
    }

    #[test]
    fn a_threshold_of_zero_is_refused() {
        let refusal = match parse_audit_config(&env(&[("MCP_AUDIT_LOG_MAX_MB", "0")])) {
            AuditConfigResult::Err { message } => message,
            AuditConfigResult::Ok { .. } => panic!("no log honours a threshold of zero"),
        };
        assert!(
            refusal.contains("MCP_AUDIT_LOG_MAX_MB"),
            "the refusal names the variable: {refusal}"
        );
    }

    #[test]
    fn a_threshold_without_a_file_says_it_does_nothing() {
        let (config, warnings) = match parse_audit_config(&env(&[("MCP_AUDIT_LOG_MAX_MB", "5")])) {
            AuditConfigResult::Ok { config, warnings } => (config, warnings),
            AuditConfigResult::Err { message } => {
                panic!("the configuration is refused: {message}")
            }
        };
        assert_eq!(config.path, None);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].contains("MCP_AUDIT_LOG"),
            "the warning names the variable that would fix it: {}",
            warnings[0]
        );
    }

    #[test]
    fn a_threshold_with_a_file_warns_about_nothing() {
        let warnings = match parse_audit_config(&env(&[
            ("MCP_AUDIT_LOG", "/tmp/audit.log"),
            ("MCP_AUDIT_LOG_MAX_MB", "5"),
        ])) {
            AuditConfigResult::Ok { warnings, .. } => warnings,
            AuditConfigResult::Err { message } => panic!("the configuration is refused: {message}"),
        };
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn a_newline_in_a_path_cannot_forge_a_line() {
        let record = AuditRecord::new(
            &AuditContext::default(),
            "http",
            "write_note",
            CallOutcome::Ok,
        )
        .with_target(Some("Notes/Todo.md\n{\"tool\":\"resolve_base\"}"));
        let line = record.to_line().expect("a record is one line");
        assert!(!line.contains('\n'), "the record is one line: {line}");
        assert!(
            line.contains("\\n"),
            "the newline is escaped rather than dropped: {line}"
        );
    }

    #[test]
    fn a_very_long_path_is_capped() {
        let long = "x".repeat(4096);
        let record = AuditRecord::new(
            &AuditContext::default(),
            "http",
            "get_note",
            CallOutcome::Ok,
        )
        .with_target(Some(&long));
        let target = record.target.expect("the path is there");
        assert!(
            target.chars().count() <= TARGET_MAX + 1,
            "the path is capped: {} characters",
            target.chars().count()
        );
    }

    #[test]
    fn the_record_carries_the_call_and_nothing_else() {
        let context = AuditContext {
            client: Some("claude-code/1.2.3".to_string()),
            session: Some("abc".to_string()),
            peer: Some("192.168.1.5:54321".to_string()),
        };
        let record = AuditRecord::new(&context, "http", "write_note", CallOutcome::Partial)
            .with_target(Some("Notes/Todo.md"))
            .with_error(Some("concurrent-edit"))
            .with_duration(Duration::from_millis(14));
        let line = record.to_line().expect("a record is one line");
        assert!(line.contains("\"tool\":\"write_note\""), "{line}");
        assert!(line.contains("\"caller\":\"claude-code/1.2.3\""), "{line}");
        assert!(line.contains("\"transport\":\"http\""), "{line}");
        assert!(line.contains("\"target\":\"Notes/Todo.md\""), "{line}");
        assert!(
            line.contains("\"outcome\":\"partial-with-errors\""),
            "{line}"
        );
        assert!(line.contains("\"error\":\"concurrent-edit\""), "{line}");
        assert!(line.contains("\"duration_ms\":14"), "{line}");
    }

    #[test]
    fn a_caller_that_never_handshook_is_unknown_rather_than_guessed() {
        let record = AuditRecord::new(
            &AuditContext::default(),
            "stdio",
            "list_bases",
            CallOutcome::Ok,
        );
        assert_eq!(record.caller, "unknown");
    }
}
