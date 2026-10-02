//! WebDAV vault source: the second implementation of [`VaultSource`].
//!
//! Three constraints from the project plan shape everything below, and each is a
//! constraint rather than a preference:
//!
//!   - RECURSION IS `Depth: 1`, ALWAYS. `Depth: infinity` is what a WebDAV client
//!     would naturally send and what no mainstream server accepts, so `list()`
//!     PROPFINDs one collection at a time and recurses into each child collection
//!     itself. The cost is one round trip per directory; the alternative is a
//!     backend that works against no server at all. [`Depth`] is an enum with no
//!     `infinity` variant, so the guarantee is a property of the type rather
//!     than of anyone's care at a call site.
//!   - WRITES ARE VERIFIED WITH OUR OWN HASH, NOT AN ETAG. A server ETag is not
//!     merely untrustworthy here, it is often absent — the server this targets
//!     does not emit `getetag` in its WebDAV test suite at all. After every `PUT`
//!     this backend re-`GET`s the resource and compares [`content_hash`], the one
//!     hash both backends share. A `PUT` that succeeds while storing something
//!     else is the one failure a client cannot detect from the response, and that
//!     read-back is what catches it.
//!   - A CREDENTIAL NEVER APPEARS IN A STRING. No log line, no error message, no
//!     URL. A URL carrying userinfo is refused outright, so there is exactly one
//!     place a credential can live and nothing this file builds can echo it.
//!
//! Snapshot semantics match [`crate::vault::fs::FsVaultSource`] exactly —
//! `list()` is a snapshot until `refresh` or a write, and text and hashes are
//! cached beneath it — because the two backends are meant to be interchangeable
//! and the equivalence suite treats a difference as a finding. Exactly two
//! behaviours deliberately differ, both commented where they happen: a refused
//! `PROPFIND` is an error here rather than a silently smaller vault
//! (`children_of`), and a `delete` of something absent is tolerated rather than
//! reported as a success it was not (`delete`).

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, FixedOffset, NaiveDate, TimeZone};

use crate::error::{BasesError, Result};
use crate::vault::fs::content_hash;
use crate::vault::source::{
    is_indexable, normalise_line_endings, FileStat, SourceKind, VaultSource,
};

/// The media type a `PROPFIND` request body is sent as.
const XML_CONTENT_TYPE: &str = r#"application/xml; charset="utf-8""#;

/// The media type a note is written as.
const TEXT_CONTENT_TYPE: &str = "text/plain; charset=utf-8";

/// How long one request may take before it is abandoned.
pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;

/// The properties a `PROPFIND` asks for.
///
/// Asked for by name rather than with `<allprop/>` so the request states exactly
/// what the backend will read, and so a `propstat` carrying a `404` for a
/// property the server declines to invent is visible instead of quietly absent.
/// None of the three needs `getetag`, which is precisely why nothing here reads
/// one.
pub const DAV_PROPFIND_BODY: &str = concat!(
    r#"<?xml version="1.0" encoding="utf-8"?>"#,
    r#"<d:propfind xmlns:d="DAV:"><d:prop>"#,
    r#"<d:resourcetype/><d:getcontentlength/><d:getlastmodified/>"#,
    r#"</d:prop></d:propfind>"#,
);

// ---------------------------------------------------------------------------
// The vocabulary
// ---------------------------------------------------------------------------

/// The `VaultSource` method a request was issued for.
///
/// `hash` is absent because it issues no request of its own: it hashes the text
/// a `GET` returned, so a hash and a read of the same path are one round trip
/// rather than two.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DavOperation {
    List,
    Read,
    Stat,
    Exists,
    Write,
    EnsureDir,
    Delete,
}

impl DavOperation {
    const fn as_str(self) -> &'static str {
        match self {
            DavOperation::List => "list",
            DavOperation::Read => "read",
            DavOperation::Stat => "stat",
            DavOperation::Exists => "exists",
            DavOperation::Write => "write",
            DavOperation::EnsureDir => "ensureDir",
            DavOperation::Delete => "delete",
        }
    }
}

impl fmt::Display for DavOperation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The HTTP methods a vault needs, WebDAV's own two included.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WebdavMethod {
    Propfind,
    Get,
    Put,
    Mkcol,
    Delete,
}

impl WebdavMethod {
    pub const fn as_str(self) -> &'static str {
        match self {
            WebdavMethod::Propfind => "PROPFIND",
            WebdavMethod::Get => "GET",
            WebdavMethod::Put => "PUT",
            WebdavMethod::Mkcol => "MKCOL",
            WebdavMethod::Delete => "DELETE",
        }
    }
}

impl fmt::Display for WebdavMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `Depth` values this client is allowed to send. Never `infinity`.
///
/// An enum with two variants, because `infinity` is a string on the wire and
/// nothing but this type stands between a client and a server that will not
/// answer it. There is no way to spell it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Depth {
    Zero,
    One,
}

impl Depth {
    pub const fn as_str(self) -> &'static str {
        match self {
            Depth::Zero => "0",
            Depth::One => "1",
        }
    }
}

impl fmt::Display for Depth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One request, as this backend builds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DavRequest {
    pub url: String,
    pub method: WebdavMethod,
    /// Header names are stored as given; the only ones ever sent are
    /// `Authorization`, `Depth` and `Content-Type`.
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
    /// The per-request budget. A transport is expected to abandon the request
    /// when it elapses, and to fail the call rather than hang.
    pub timeout: Duration,
}

impl DavRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// What a server answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DavResponse {
    pub status: u16,
    pub status_text: String,
    pub body: Vec<u8>,
}

impl DavResponse {
    pub fn new(status: u16, status_text: &str, body: Vec<u8>) -> Self {
        Self {
            status,
            status_text: status_text.to_string(),
            body,
        }
    }

    /// The body as text, lossily.
    ///
    /// Lossy because that is what a `PROPFIND` body and a note body both are
    /// everywhere else in this project: the filesystem backend reads with
    /// Node's lossy `"utf8"` and the write read-back decodes the same way, so a
    /// decoding policy that differed here would make the equivalence suite
    /// compare policies.
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// The HTTP call this backend makes, and the only one it may depend on.
///
/// Deliberately narrower than `reqwest::Client`: the shape here is the whole
/// vocabulary of the backend — a URL, a method, headers, an optional body and a
/// budget — so a change to it is a change to what this client is able to do, and
/// every test double in the suite is a `Map` rather than an HTTP mock library.
#[async_trait(?Send)]
pub trait WebdavTransport {
    async fn send(&self, request: DavRequest) -> Result<DavResponse>;
}

/// The transport that actually speaks HTTP, via `reqwest`.
pub struct HttpTransport {
    client: reqwest::Client,
}

impl HttpTransport {
    pub fn new() -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder().build().map_err(|e| {
                BasesError::new(format!("WebDAV: could not build an HTTP client: {e}"))
            })?,
        })
    }
}

#[async_trait(?Send)]
impl WebdavTransport for HttpTransport {
    async fn send(&self, request: DavRequest) -> Result<DavResponse> {
        let method = reqwest::Method::from_bytes(request.method.as_str().as_bytes())
            .map_err(|e| BasesError::new(format!("WebDAV: {e}")))?;
        let mut builder = self
            .client
            .request(method, &request.url)
            .timeout(request.timeout);
        for (name, value) in &request.headers {
            builder = builder.header(name.as_str(), value.as_str());
        }
        if let Some(body) = request.body {
            builder = builder.body(body);
        }
        let response = builder
            .send()
            .await
            .map_err(|e| BasesError::new(e.to_string()))?;
        let status = response.status();
        let status_text = status.canonical_reason().unwrap_or_default().to_string();
        let body = response
            .bytes()
            .await
            .map_err(|e| BasesError::new(e.to_string()))?
            .to_vec();
        Ok(DavResponse {
            status: status.as_u16(),
            status_text,
            body,
        })
    }
}

// ---------------------------------------------------------------------------
// Refusals
// ---------------------------------------------------------------------------

/// Statuses an operation treats as success anyway.
///
/// Every entry is a status RFC 4918 requires, not one found inconvenient:
/// `MKCOL` answers `405` for a collection that is already there, and `DELETE`
/// answers `404` for a resource that is not. Both are the server confirming that
/// the state the caller was trying to reach already holds, which is the whole
/// intent of the call. `list`, `read`, `stat` and `write` tolerate nothing, so a
/// `PROPFIND` that is refused is a refusal rather than an empty directory.
///
/// `exists` is deliberately NOT here, though it also finds a `404` meaningful:
/// for `MKCOL` and `DELETE` a `404` is a success wearing a refusal's clothes,
/// where for `exists` it is the ANSWER. Listing it would hand `exists` a `404` as
/// `Ok`, and the operation would then have to read the status back out of a
/// successful response to tell present from absent -- so the one status it treats
/// as an answer is matched where it is used instead.
const fn tolerated(operation: DavOperation, status: u16) -> bool {
    matches!(
        (operation, status),
        (DavOperation::EnsureDir, 405) | (DavOperation::Delete, 404)
    )
}

/// A refusal from the WebDAV server, carrying its status structurally.
///
/// A `BasesError` underneath, so the MCP layer reports it as a structured tool
/// failure naming the operation rather than as an internal bug in this server.
/// The status is a field and not only prose because the two reactions differ: a
/// `503` is worth a retry and a `404` is not, and a caller forced to scrape
/// strings gets one of those right by luck.
///
/// Carries the vault-relative path rather than a URL. The configured base URL
/// may be reachable from outside, may be a hostname worth not broadcasting, and
/// is in any case not what the caller asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebdavError {
    /// The status the server answered with.
    ///
    /// `None` when nothing answered at all: a transport failure has no status,
    /// and inventing one would make a network problem look like a `503` worth
    /// retrying. The absence IS the distinction the prose drew.
    pub status: Option<u16>,
    pub operation: DavOperation,
    pub method: WebdavMethod,
    pub path: String,
    pub error: BasesError,
}

impl WebdavError {
    pub fn message(&self) -> &str {
        self.error.message()
    }
}

impl From<WebdavError> for BasesError {
    fn from(error: WebdavError) -> Self {
        error.error
    }
}

impl fmt::Display for WebdavError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.error, f)
    }
}

impl std::error::Error for WebdavError {}

/// The refusal a status implies, or `None` when the call did what it asked.
///
/// `None` rather than a boolean so the call site reads as what it is — throw
/// this or do not — and so a tolerated status cannot be confused with a
/// successful one by a caller that only wants to know whether to continue. Any
/// `2xx` passes, including the `207 Multi-Status` a `PROPFIND` answers with, and
/// the tolerated statuses for the operation pass. Everything else refuses, `3xx`
/// included: a redirect this client did not follow is a server pointing
/// somewhere else, and reading a vault from there without being told would be
/// worse than refusing.
pub fn dav_refusal(
    operation: DavOperation,
    method: WebdavMethod,
    path: &str,
    status: u16,
    status_text: &str,
) -> Option<WebdavError> {
    if (200..300).contains(&status) || tolerated(operation, status) {
        return None;
    }
    Some(WebdavError {
        status: Some(status),
        operation,
        method,
        path: path.to_string(),
        error: BasesError::new(format!(
            "WebDAV {operation}: {method} \"{}\": {status} {status_text}",
            at_root(path)
        ))
        .with_construct("webdav"),
    })
}

// ---------------------------------------------------------------------------
// Multistatus
// ---------------------------------------------------------------------------

/// One resource as a `207 Multi-Status` body describes it.
///
/// The properties are optional because the server decides that: `propstat`
/// carries a status per group, and a server may decline to report
/// `getcontentlength` for a resource it has no length for. Extraction is total
/// for that reason — it takes what the XML says and passes no judgement — while
/// [`stat_of`] is where a missing property becomes a refusal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DavResource {
    /// Vault-relative POSIX path. `""` is the vault root.
    pub path: String,
    pub is_collection: bool,
    pub size: Option<u64>,
    /// The `getlastmodified` string verbatim, never parsed here.
    pub last_modified: Option<String>,
}

/// Every resource in a `207 Multi-Status` body, in the order the server listed
/// them.
///
/// `base_path` is the server-side path the configured URL points at, e.g.
/// `/dav/vault` for `https://host/dav/vault/`. It is stripped from every href,
/// because an href is server-root-absolute and a vault-relative path is not: the
/// same note is `/dav/vault/Root Ticket.md` on the wire and `Root Ticket.md` in
/// every `VaultSource` contract in this project.
///
/// A href outside `base_path` is refused rather than resolving to something. It
/// means the server is rooted somewhere other than where it was addressed, and
/// quietly indexing that tree would answer queries from a vault nobody named.
pub fn parse_multistatus(xml: &str, base_path: &str) -> Result<Vec<DavResource>> {
    let walk = Multistatus::read(xml)?;
    if !walk.saw_multistatus {
        return Err(BasesError::new(
            "WebDAV PROPFIND did not return a multistatus document. The server answered with \
             something else, which means it is not serving WebDAV at this URL.",
        )
        .with_construct("webdav"));
    }

    let mut out: Vec<DavResource> = Vec::new();
    for response in &walk.responses {
        // Skipped when every propstat is a non-2xx, which is how a server says
        // this resource is gone: a listing can name a file deleted between the
        // request and the response, and indexing it would put a path in `list()`
        // that a later `read_note` cannot serve. A href outside the base does
        // NOT land here — that refuses, in `vault_path_from_href`.
        let Some(properties) = found_properties(response) else {
            continue;
        };
        let Some(href) = &response.href else {
            continue;
        };
        out.push(DavResource {
            path: vault_path_from_href(href, base_path)?,
            is_collection: properties.contains_key("resourcetype"),
            size: content_length(properties.get("getcontentlength")),
            last_modified: trimmed(properties.get("getlastmodified")),
        });
    }
    Ok(out)
}

/// The properties the server actually has for one resource.
///
/// A `response` may carry several `propstat` blocks, each with its own status:
/// one for the properties that exist and one for those that do not. Only the
/// `2xx` ones describe the resource, and merging them is the point — reading
/// only the first would drop the size whenever a server puts `getetag` in a
/// second block, and reading all of them would let a `404` blank a value the
/// server did report.
fn found_properties(response: &DavResponseNode) -> Option<HashMap<String, String>> {
    let mut merged = HashMap::new();
    let mut saw_success = false;
    for block in &response.propstats {
        if !is_success(block.status.as_deref()) {
            continue;
        }
        saw_success = true;
        for (name, value) in &block.properties {
            merged.insert(name.clone(), value.clone());
        }
    }
    saw_success.then_some(merged)
}

/// `"HTTP/1.1 200 OK"` is a success; a missing or unparsable status line is not.
///
/// A whitespace, three digits, then a whitespace or the end — which is what the
/// original's regex says, and it is deliberately stricter than "the first three
/// digits anywhere": a status line with four digits in it is not one a server
/// sent, and reading it would invent a code.
fn is_success(status: Option<&str>) -> bool {
    let Some(text) = status else { return false };
    let chars: Vec<char> = text.chars().collect();
    for index in 0..chars.len() {
        if !chars[index].is_whitespace() {
            continue;
        }
        let Some(code) = chars.get(index + 1..index + 4).and_then(code_of) else {
            continue;
        };
        // `map_or(true, ..)` rather than `is_none_or(..)`: the latter is newer than the
        // crate's MSRV.
        if chars.get(index + 4).map_or(true, |c| c.is_whitespace()) {
            return (200..300).contains(&code);
        }
    }
    false
}

fn code_of(digits: &[char]) -> Option<u16> {
    digits.iter().all(char::is_ascii_digit).then(|| {
        digits.iter().fold(0u16, |code, d| {
            code * 10 + d.to_digit(10).unwrap_or(0) as u16
        })
    })
}

/// A `getcontentlength` as a byte count, or `None` if it is not one.
fn content_length(value: Option<&String>) -> Option<u64> {
    let raw = trimmed(value)?;
    raw.parse::<f64>()
        .ok()
        .filter(|size| size.is_finite())
        .map(|size| size as u64)
}

fn trimmed(value: Option<&String>) -> Option<String> {
    let text = value?.trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

/// A vault-relative POSIX path as it appears in an error message.
///
/// The vault root is `""` everywhere in this project, because that is what the
/// filesystem walk and the in-memory source both call it, and an empty string in
/// prose reads as a missing value rather than as `/`.
fn at_root(path: &str) -> &str {
    if path.is_empty() {
        "/"
    } else {
        path
    }
}

/// The vault-relative path a `DAV:href` names.
///
/// Hrefs are URL-encoded, percent-escaped and server-root-absolute, and every
/// path in this project is decoded, literal and vault-relative, so the mapping
/// is three steps in this order and the order is the whole trick:
///
///   1. Reduce to a pathname, so an href sent as a full URL and one sent as a
///      path — both allowed by RFC 4918, and servers do both — differ only
///      before the first segment.
///   2. Split on `/` and decode each segment SEPARATELY, rather than decoding
///      the whole path and then splitting. A note called `Root Project.md`
///      arrives as `Root%20Project.md` and must come back with its space, while
///      a note called `50%.md` arrives as `50%25.md` and must not be mangled by
///      a decode that ran over the separators too.
///   3. Strip `base_path`, compared segment by segment for the same reason, and
///      fold `.` and `..` out of what is left. Only then is what remained a
///      vault-relative path, and a trailing slash — which every collection href
///      carries and no vault-relative path ever does — is already gone with the
///      empty final segment.
pub fn vault_path_from_href(href: &str, base_path: &str) -> Result<String> {
    let segments = decode_segments(&href_pathname(href), href)?;
    let base = decode_segments(base_path, base_path)?;
    if !starts_with(&segments, &base) {
        return Err(BasesError::new(format!(
            "WebDAV href is outside the configured vault base \"{}\": the server rooted its \
             listing somewhere else, so its files cannot be mapped to vault-relative paths.",
            if base_path.is_empty() { "/" } else { base_path }
        ))
        .with_construct("webdav"));
    }
    Ok(fold_segments(&segments[base.len()..], "WebDAV href", href)?.join("/"))
}

/// A vault-relative path from whatever the caller passed.
///
/// Folds `.` and `..` exactly as `path.resolve` does, so a path that is legal
/// for the filesystem backend is legal here and one that is not is refused here
/// too. A leading `/` is a refusal rather than something to strip: `/etc/passwd`
/// is not a note in this vault, it is a different file, and reading it because
/// the leading slash could be dropped would be the worst bug this backend could
/// have.
pub fn vault_relative_path(rel: &str) -> Result<String> {
    if rel.starts_with('/') {
        return Err(escapes_vault("Path", rel));
    }
    Ok(fold_segments(&rel.split('/').collect::<Vec<_>>(), "Path", rel)?.join("/"))
}

/// Resolve a vault-relative path against the configured base and encode the
/// result.
///
/// Each segment is encoded on its own so a note called `Q1 #2.md` produces a URL
/// whose `#` cannot be read as a fragment, and whose space is `%20` rather than
/// the server having to tolerate a literal one.
///
/// A collection keeps its trailing slash, because that is how WebDAV says a URL
/// names one. A server that receives `/vault/Projects` may answer `404`, because
/// as far as it is concerned that names a resource called `Projects`; only
/// `/vault/Projects/` is the collection. The vault-relative path has no trailing
/// slash, so the slash has to be put back here, and only here, where the caller
/// has said which kind of resource it is asking about.
fn resource_url(base_url: &str, path: &str, collection: bool) -> String {
    if path.is_empty() {
        return format!("{base_url}/");
    }
    let encoded = path
        .split('/')
        .map(encode_segment)
        .collect::<Vec<_>>()
        .join("/");
    if collection {
        format!("{base_url}/{encoded}/")
    } else {
        format!("{base_url}/{encoded}")
    }
}

/// The pathname of an href that may or may not be a full URL.
fn href_pathname(href: &str) -> String {
    match reqwest::Url::parse(href) {
        Ok(url) => url.path().to_string(),
        // A path with no scheme makes `Url::parse` fail, which is the common
        // case. Deliberately NOT resolved against anything: the original's `URL`
        // throws here too, so a `..` in a relative href survives to be folded by
        // `fold_segments` rather than being normalised away before the base path
        // has been stripped.
        Err(_) => href.to_string(),
    }
}

/// Path segments, percent-decoded one at a time, with no empty segments.
///
/// Strict where the original is strict: a `%` that does not introduce two hex
/// digits refuses the document rather than decoding to itself, because a
/// silently-mangled path is a note the vault cannot read later.
fn decode_segments(pathname: &str, origin: &str) -> Result<Vec<String>> {
    pathname
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(|segment| decode_segment(segment, origin))
        .collect()
}

fn decode_segment(segment: &str, origin: &str) -> Result<String> {
    let bytes = segment.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let escape = bytes
                .get(i + 1..i + 3)
                .and_then(|pair| Some(hex_digit(pair[0])? * 16 + hex_digit(pair[1])?));
            match escape {
                Some(byte) => {
                    out.push(byte);
                    i += 3;
                }
                None => return Err(malformed_encoding(origin)),
            }
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).map_err(|_| malformed_encoding(origin))
}

fn hex_digit(byte: u8) -> Option<u8> {
    char::from(byte).to_digit(16).map(|digit| digit as u8)
}

fn malformed_encoding(origin: &str) -> BasesError {
    BasesError::new(format!(
        "WebDAV path is not valid percent-encoding, so it cannot be read as a path: {origin}"
    ))
    .with_construct("webdav")
}

/// Everything `encodeURIComponent` leaves alone. `percent-encoding`'s
/// `NON_ALPHANUMERIC` is the right base for "encode every byte that is not
/// unreserved", and these are the eight JavaScript treats as unreserved too.
fn encode_segment(segment: &str) -> String {
    const RESERVED: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'.')
        .remove(b'_')
        .remove(b'!')
        .remove(b'~')
        .remove(b'*')
        .remove(b'\'')
        .remove(b'(')
        .remove(b')');
    percent_encoding::utf8_percent_encode(segment, RESERVED).to_string()
}

/// Whether `segments` begins with `prefix`, compared element by element.
fn starts_with(segments: &[String], prefix: &[String]) -> bool {
    prefix.len() <= segments.len() && prefix.iter().enumerate().all(|(i, s)| &segments[i] == s)
}

/// Fold `.` and `..` out of path segments.
///
/// `what` names the origin in the refusal, because the two callers are different
/// trust boundaries — a path an agent supplied and a path a server supplied — and
/// a message that cannot say which one was wrong is a message that gets guessed
/// at.
fn fold_segments<S: AsRef<str>>(segments: &[S], what: &str, origin: &str) -> Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for segment in segments {
        let segment = segment.as_ref();
        if segment.is_empty() || segment == "." {
            continue;
        }
        if segment != ".." {
            out.push(segment.to_string());
            continue;
        }
        // The base path has already been stripped, so a `..` with nothing left
        // to pop is an escape from the vault rather than a walk up towards it.
        if out.pop().is_none() {
            return Err(escapes_vault(what, origin));
        }
    }
    Ok(out)
}

/// A path that must name a file.
///
/// `PUT` and `DELETE` with no name address the collection itself, which is a
/// request with no honest reading: the server either refuses it or, worse,
/// treats the collection as a resource. The filesystem backend fails on the same
/// call with a raw `EISDIR`, so this makes the refusal say what happened.
fn named_file(path: &str) -> Result<&str> {
    if !path.is_empty() {
        return Ok(path);
    }
    Err(BasesError::new(
        "WebDAV: the vault root is a collection, not a file, so it cannot be written or deleted.",
    )
    .with_construct("webdav"))
}

fn escapes_vault(what: &str, origin: &str) -> BasesError {
    BasesError::new(format!("{what} escapes the vault root: {origin}")).with_construct("webdav")
}

fn cyclic(path: &str) -> BasesError {
    BasesError::new(format!(
        "WebDAV list: a listing named \"{}\" as a collection inside itself, so the vault has no \
         finite tree. Refusing rather than following the cycle.",
        at_root(path)
    ))
    .with_construct("webdav")
}

// ---------------------------------------------------------------------------
// The multistatus reader
// ---------------------------------------------------------------------------

/// One `propstat` block: the status it reported under and the properties it
/// carried. Both are optional because a server may send either half alone.
#[derive(Debug, Default)]
struct DavPropstat {
    status: Option<String>,
    properties: Vec<(String, String)>,
}

/// One `response` block: an href and the `propstat` blocks that described it.
#[derive(Debug, Default)]
struct DavResponseNode {
    href: Option<String>,
    propstats: Vec<DavPropstat>,
}

/// The parsed shape of a `207 Multi-Status` body.
///
/// A purpose-built reader rather than a generic XML-to-tree binding, because
/// three things here are not "parse the document" questions: a `prop` child that
/// is EMPTY is a property the server reported with no value, `resourcetype` names
/// a collection exactly when it has a `collection` child, and only the `2xx`
/// `propstat` blocks count. A tree binding that got any of the three wrong would
/// answer with a vault that looks empty rather than refuse, which is the one
/// confusion this server exists to avoid.
#[derive(Debug, Default)]
struct Multistatus {
    saw_multistatus: bool,
    responses: Vec<DavResponseNode>,
}

/// A `prop` child being read, and the text inside it so far.
#[derive(Debug)]
struct OpenProp {
    name: String,
    text: String,
}

impl Multistatus {
    fn read(xml: &str) -> Result<Self> {
        use quick_xml::events::Event;

        let mut reader = quick_xml::Reader::from_str(xml);
        let mut walk = DavReader::default();
        let mut buffer = Vec::new();
        loop {
            let event = reader.read_event_into(&mut buffer).map_err(|error| {
                BasesError::new(format!(
                    "WebDAV PROPFIND returned a body that is not well-formed XML: {error}"
                ))
                .with_construct("webdav")
            })?;
            let done = match event {
                Event::Start(element) => {
                    walk.open(&local_name(element.name().as_ref()));
                    false
                }
                Event::Empty(element) => {
                    walk.empty(&local_name(element.name().as_ref()));
                    false
                }
                Event::End(element) => {
                    walk.close(&local_name(element.name().as_ref()));
                    false
                }
                Event::Text(text) => {
                    let decoded = text.decode().map_err(|_| {
                        BasesError::new("WebDAV PROPFIND returned XML that is not valid UTF-8.")
                            .with_construct("webdav")
                    })?;
                    walk.text(&decoded);
                    false
                }
                Event::CData(text) => {
                    walk.text(&String::from_utf8_lossy(text.as_ref()));
                    false
                }
                Event::Eof => true,
                _ => false,
            };
            buffer.clear();
            if done {
                return Ok(walk.walk);
            }
        }
    }
}

/// The reader's mutable state, bundled into one value so the four event handlers
/// take an argument rather than seven.
#[derive(Debug, Default)]
struct DavReader {
    walk: Multistatus,
    /// Local names of the elements currently open, outermost first.
    stack: Vec<String>,
    response: DavResponseNode,
    propstat: Option<DavPropstat>,
    prop: Option<OpenProp>,
    href_text: Option<String>,
    status_text: Option<String>,
    /// Whether the `resourcetype` being read has a `collection` child. A
    /// collection is `<resourcetype><collection/></resourcetype>` and a file is
    /// an empty one, so the answer is the presence of the child rather than any
    /// value inside it. A server that writes `<collection></collection>` is
    /// identical here.
    resourcetype_is_collection: bool,
}

/// The kind of element a new child sits inside.
///
/// `Copy`, so the handlers can match on it and then mutate the reader. An
/// `Option<&str>` borrowed from the reader would keep that borrow alive across
/// the mutation, and a `String` clone per element would allocate once per tag in
/// a body that has ten tags per resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Parent {
    Prop,
    Resourcetype,
    Response,
    Propstat,
    Other,
}

impl DavReader {
    fn parent(&self) -> Parent {
        match self.stack.last().map(String::as_str) {
            Some("prop") => Parent::Prop,
            Some("resourcetype") => Parent::Resourcetype,
            Some("response") => Parent::Response,
            Some("propstat") => Parent::Propstat,
            _ => Parent::Other,
        }
    }

    fn open(&mut self, name: &str) {
        if name == "multistatus" {
            self.walk.saw_multistatus = true;
        }
        match self.parent() {
            Parent::Prop => {
                self.resourcetype_is_collection = false;
                self.prop = Some(OpenProp {
                    name: name.to_string(),
                    text: String::new(),
                });
            }
            Parent::Resourcetype if name == "collection" => self.resourcetype_is_collection = true,
            Parent::Response if name == "href" => self.href_text = Some(String::new()),
            Parent::Propstat if name == "prop" => self.propstat = Some(DavPropstat::default()),
            Parent::Propstat if name == "status" => self.status_text = Some(String::new()),
            _ => {}
        }
        self.stack.push(name.to_string());
    }

    /// A self-closing element: present, with no value and no closing event.
    fn empty(&mut self, name: &str) {
        if name == "multistatus" {
            self.walk.saw_multistatus = true;
        }
        match self.parent() {
            Parent::Prop => self.record(name, ""),
            Parent::Resourcetype if name == "collection" => self.resourcetype_is_collection = true,
            _ => {}
        }
    }

    /// Text inside the element currently open, and nowhere else.
    ///
    /// Dispatched on the innermost OPEN element rather than on its parent,
    /// because that is the element the text is inside. The `prop` case is gated
    /// on the open property's own name, so a `prop` child that itself contains
    /// markup does not fold its inner text into the outer value.
    fn text(&mut self, text: &str) {
        if self
            .prop
            .as_ref()
            .is_some_and(|open| self.stack.last() == Some(&open.name))
        {
            if let Some(open) = self.prop.as_mut() {
                open.text.push_str(text);
            }
            return;
        }
        if self.stack.last().is_some_and(|name| name == "href") {
            if let Some(buffer) = self.href_text.as_mut() {
                buffer.push_str(text);
            }
            return;
        }
        if self.stack.last().is_some_and(|name| name == "status") {
            if let Some(buffer) = self.status_text.as_mut() {
                buffer.push_str(text);
            }
        }
    }

    fn close(&mut self, name: &str) {
        if self.prop.as_ref().is_some_and(|open| open.name == name) {
            let open = self.prop.take().expect("checked above");
            let value = if open.name == "resourcetype" {
                ""
            } else {
                open.text.as_str()
            };
            self.record(&open.name, value);
        }
        if name == "status" && self.propstat.is_some() {
            if let Some(block) = self.propstat.as_mut() {
                block.status = self.status_text.take();
            }
        }
        if name == "href" {
            self.response.href = self.href_text.take();
        }
        if name == "propstat" && self.propstat.is_some() {
            if let Some(block) = self.propstat.take() {
                self.response.propstats.push(block);
            }
        }
        if name == "response" {
            self.walk.responses.push(std::mem::take(&mut self.response));
        }
        self.stack.pop();
    }

    /// Store one `prop` child in the `propstat` block that reported it.
    ///
    /// An empty `resourcetype` records NOTHING, because a file's `resourcetype`
    /// is empty and a collection's is not — recording both as present would make
    /// every file in the vault look like a collection, and a vault of
    /// collections is a vault with no notes.
    fn record(&mut self, name: &str, value: &str) {
        let is_collection = name == "resourcetype" && self.resourcetype_is_collection;
        if name == "resourcetype" {
            self.resourcetype_is_collection = false;
        }
        if !is_collection && name == "resourcetype" {
            return;
        }
        if let Some(block) = self.propstat.as_mut() {
            block.properties.push((name.to_string(), value.to_string()));
        }
    }
}

/// The local name of a qualified XML name, with the namespace prefix dropped.
///
/// Case-SENSITIVE, deliberately: XML names are, and every `DAV:` name is
/// lowercase, so folding case would accept documents the original refuses.
fn local_name(qualified: &[u8]) -> String {
    let text = String::from_utf8_lossy(qualified);
    match text.split_once(':') {
        Some((_, local)) => local.to_string(),
        None => text.into_owned(),
    }
}

// ---------------------------------------------------------------------------
// Dates
// ---------------------------------------------------------------------------

/// The instant a `getlastmodified` names.
///
/// RFC 9110 allows three date formats in this header and a server may send any of
/// them; all three are read here, and a value none of them reads is refused
/// rather than turned into an epoch that would then flow into a rendered table
/// as a plausible-looking date. The offset is reported as received, because
/// converting it would be inventing a precision the server did not claim.
pub fn parse_dav_date(value: &str, path: &str) -> Result<DateTime<FixedOffset>> {
    DateTime::parse_from_rfc2822(value)
        .or_else(|_| parse_rfc850(value))
        .or_else(|_| parse_asctime(value))
        .map_err(|_| {
            BasesError::new(format!(
                "WebDAV stat: getlastmodified for \"{}\" is not a date this client can read: {}.",
                at_root(path),
                json_quote(value)
            ))
            .with_construct("webdav")
        })
}

/// The obsolete RFC 850 format, `Tuesday, 01-Oct-24 10:11:12 GMT`.
///
/// Parsed by hand because the RFC 2822 reader wants the day and month in the
/// other order with no dashes between them, and a string being reassembled only
/// to be taken apart again is two chances to be wrong. The weekday is ignored —
/// a date with the wrong weekday in it is still the date the server sent, and
/// refusing it would be refusing a header over a field nothing reads.
fn parse_rfc850(value: &str) -> std::result::Result<DateTime<FixedOffset>, ()> {
    let (_, rest) = value.split_once(", ").ok_or(())?;
    let (date, rest) = rest.split_once(' ').ok_or(())?;
    let (clock, zone) = rest.split_once(' ').ok_or(())?;
    let (day, rest) = date.split_once('-').ok_or(())?;
    let (month, year) = rest.split_once('-').ok_or(())?;

    let day: u32 = day.parse().map_err(|_| ())?;
    let month = month_number(month).ok_or(())?;
    let year: i32 = year.parse().map_err(|_| ())?;
    // The two-digit year pivots the same way the RFC 2822 reader pivots it: a
    // header sent in 1994 is not a header sent in 2094.
    let year = if year < 50 { 2000 + year } else { 1900 + year };
    assemble(year, month, day, clock, zone)
}

/// The obsolete `asctime` format, `Tue Oct  1 10:11:12 2024`.
///
/// Built by hand rather than reformatted for the RFC 2822 reader: that reader
/// wants a weekday and a spelled month in a different order, and `asctime` puts
/// a double space where the day is one digit.
fn parse_asctime(value: &str) -> std::result::Result<DateTime<FixedOffset>, ()> {
    let tokens: Vec<&str> = value.split_whitespace().collect();
    if tokens.len() != 5 {
        return Err(());
    }
    let month = month_number(tokens[1]).ok_or(())?;
    let day: u32 = tokens[2].parse().map_err(|_| ())?;
    let year: i32 = tokens[4].parse().map_err(|_| ())?;
    // `asctime` carries no zone; RFC 9110 says it is GMT, and anything else would
    // be a guess about a clock this client cannot see.
    assemble(year, month, day, tokens[3], "GMT")
}

/// A date, a `HH:MM:SS` clock and a zone name, as one instant.
fn assemble(
    year: i32,
    month: u32,
    day: u32,
    clock: &str,
    zone: &str,
) -> std::result::Result<DateTime<FixedOffset>, ()> {
    let mut parts = clock.split(':');
    let hour: u32 = parts.next().ok_or(())?.parse().map_err(|_| ())?;
    let minute: u32 = parts.next().ok_or(())?.parse().map_err(|_| ())?;
    let second: u32 = parts.next().ok_or(())?.parse().map_err(|_| ())?;
    if parts.next().is_some() {
        return Err(());
    }
    let offset = zone_offset(zone)?;
    let date = NaiveDate::from_ymd_opt(year, month, day).ok_or(())?;
    let naive = date.and_hms_opt(hour, minute, second).ok_or(())?;
    offset.from_local_datetime(&naive).single().ok_or(())
}

/// The offset a zone name names. Only the two names RFC 9110 still allows.
fn zone_offset(zone: &str) -> std::result::Result<FixedOffset, ()> {
    match zone {
        "GMT" | "UT" | "UTC" | "Z" => FixedOffset::east_opt(0).ok_or(()),
        other => {
            let sign = match other.as_bytes().first() {
                Some(b'+') => 1,
                Some(b'-') => -1,
                _ => return Err(()),
            };
            let digits = &other[1..];
            if digits.len() != 4 {
                return Err(());
            }
            let hours: i32 = digits[..2].parse().map_err(|_| ())?;
            let minutes: i32 = digits[2..].parse().map_err(|_| ())?;
            if hours > 23 || minutes > 59 {
                return Err(());
            }
            FixedOffset::east_opt(sign * (hours * 3_600 + minutes * 60)).ok_or(())
        }
    }
}

fn month_number(name: &str) -> Option<u32> {
    Some(match name {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    })
}

/// A short JSON string literal, so a date that is not a date is quoted in the
/// refusal the way `JSON.stringify` would quote it.
fn json_quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// `FsVaultSource.walk` skips a dot-directory rather than descending into it.
///
/// The basename is the only segment that can start with a dot here, because the
/// walk never descends into one — so a per-segment check would be a second rule
/// for a question the recursion has already answered.
fn is_hidden_collection(path: &str) -> bool {
    path.rsplit('/').next().unwrap_or(path).starts_with('.')
}

/// The `FileStat` one resource describes, or a refusal saying which part is
/// missing.
///
/// Fail-fast on a missing `getlastmodified` because there is no honest default.
/// `FileStat::mtime` feeds `file.ctime` and `file.mtime`, so substituting the
/// epoch for a value the server declined to send would put a real-looking date
/// in a query result, and this server's whole reason to exist is that a wrong
/// answer must be visibly wrong rather than plausible.
fn stat_of(resource: Option<&DavResource>, path: &str) -> Result<FileStat> {
    let resource = resource.ok_or_else(|| {
        BasesError::new(format!(
            "WebDAV stat: PROPFIND \"{}\" returned no resource for it.",
            at_root(path)
        ))
        .with_construct("webdav")
    })?;
    if resource.is_collection {
        return Err(BasesError::new(format!(
            "WebDAV stat: PROPFIND \"{}\" reported a collection.",
            at_root(path)
        ))
        .with_construct("webdav"));
    }
    let size = resource.size.ok_or_else(|| {
        BasesError::new(format!(
            "WebDAV stat: the server did not report getcontentlength for \"{}\", so its size is \
             unknown rather than zero.",
            at_root(path)
        ))
        .with_construct("webdav")
    })?;
    let mtime = parse_dav_date(
        resource.last_modified.as_deref().ok_or_else(|| {
            BasesError::new(format!(
                "WebDAV stat: the server did not report getlastmodified for \"{}\", so its mtime \
                 is unknown rather than the epoch.",
                at_root(path)
            ))
            .with_construct("webdav")
        })?,
        path,
    )?;
    Ok(FileStat { size, mtime })
}

// ---------------------------------------------------------------------------
// The source
// ---------------------------------------------------------------------------

/// What a request needs beyond its method and path.
#[derive(Debug, Default, Clone)]
pub struct DavRequestOptions {
    pub body: Option<String>,
    /// Set only for `PROPFIND`, and only to one of the two depths the type
    /// allows. A `GET`, a `PUT`, an `MKCOL` and a `DELETE` carry no `Depth` at
    /// all, which is what the original sends and what servers expect.
    pub depth: Option<Depth>,
    pub content_type: Option<&'static str>,
    /// Whether the URL names a collection, which is what decides the trailing
    /// slash.
    pub collection: bool,
}

pub struct WebdavVaultOptions {
    /// The WebDAV collection that IS the vault, e.g. `https://host/dav/vault/`.
    pub url: String,
    pub user: Option<String>,
    pub password: Option<String>,
    /// Per-request budget in milliseconds. Defaults to [`DEFAULT_TIMEOUT_MS`].
    pub timeout_ms: Option<u64>,
    /// The HTTP seam, defaulting to a `reqwest` client.
    pub transport: Option<Box<dyn WebdavTransport>>,
}

impl WebdavVaultOptions {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            user: None,
            password: None,
            timeout_ms: None,
            transport: None,
        }
    }

    pub fn with_user(mut self, user: impl Into<String>) -> Self {
        self.user = Some(user.into());
        self
    }

    pub fn with_password(mut self, password: impl Into<String>) -> Self {
        self.password = Some(password.into());
        self
    }

    pub fn with_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.timeout_ms = Some(timeout_ms);
        self
    }

    /// Replace the HTTP seam.
    ///
    /// Present because the two hard constraints of this backend are otherwise
    /// untestable without a server: that recursion sends `Depth: 1` and never
    /// `infinity`, and that it reads a listing rather than a file per note. Both
    /// are claims about the requests this source makes, so the tests assert on the
    /// requests.
    pub fn with_transport(mut self, transport: Box<dyn WebdavTransport>) -> Self {
        self.transport = Some(transport);
        self
    }
}

pub struct WebdavVaultSource {
    /// The base URL with no trailing slash, so paths can be appended verbatim.
    base_url: String,
    /// The same base as a server-side path, for stripping from hrefs.
    base_path: String,
    authorization: Option<String>,
    timeout: Duration,
    transport: Box<dyn WebdavTransport>,
    files: RefCell<Option<Vec<String>>>,
    /// Note text, NORMALISED — the same string [`VaultSource::read_note`] returns.
    /// The equivalence suite reads this backend and the filesystem one through the
    /// same calls, so a cache that held raw bytes here and normalised bytes there
    /// would show up as a phantom backend difference.
    text_cache: RefCell<HashMap<String, String>>,
    hash_cache: RefCell<HashMap<String, String>>,
}

impl std::fmt::Debug for WebdavVaultSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The base URL is not printed: it may name a host worth not broadcasting
        // in a log, and nothing here builds a credential into it, which is the
        // property worth keeping visible.
        f.debug_struct("WebdavVaultSource")
            .field("base_path", &self.base_path)
            .field("authenticated", &self.authorization.is_some())
            .finish()
    }
}

impl WebdavVaultSource {
    /// Build a source, refusing a configuration that cannot be honoured.
    ///
    /// A credential in the URL is refused rather than honoured: honouring it
    /// would put a password in every string built from that URL, and this file
    /// builds error messages from strings. Keeping the credential in exactly one
    /// place is what makes "never log it" a property of the code rather than of
    /// everyone's care.
    pub fn new(options: WebdavVaultOptions) -> Result<Self> {
        let url = reqwest::Url::parse(&options.url).map_err(|e| {
            BasesError::new(format!(
                "WebDAV vault URL is not a URL: {}. Set BASES_MCP_WEBDAV_URL to the WebDAV \
                 collection that is the vault.",
                e
            ))
            .with_construct("webdav")
        })?;
        if url.scheme() != "http" && url.scheme() != "https" {
            return Err(BasesError::new(format!(
                "WebDAV vault URL must be http or https, not \"{}\". Set BASES_MCP_WEBDAV_URL to \
                 the WebDAV collection that is the vault.",
                url.scheme()
            ))
            .with_construct("webdav"));
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(BasesError::new(
                "WebDAV vault URL carries credentials, which this client will not embed in a URL. \
                 Set BASES_MCP_WEBDAV_USER and BASES_MCP_WEBDAV_PASSWORD instead.",
            )
            .with_construct("webdav"));
        }
        if options.user.is_some() != options.password.is_some() {
            return Err(BasesError::new(
                "WebDAV needs both BASES_MCP_WEBDAV_USER and BASES_MCP_WEBDAV_PASSWORD, or \
                 neither. One without the other is a misconfiguration, not anonymous access.",
            )
            .with_construct("webdav"));
        }

        let base_path = url.path().to_string();
        let base_url = format!(
            "{}://{}{}",
            url.scheme(),
            url.authority(),
            base_path.trim_end_matches('/')
        );
        let authorization = options.user.as_ref().map(|user| {
            format!(
                "Basic {}",
                base64_encode(format!(
                    "{}:{}",
                    user,
                    options.password.as_deref().unwrap_or("")
                ))
            )
        });
        let transport = match options.transport {
            Some(transport) => transport,
            None => Box::new(HttpTransport::new()?),
        };

        Ok(Self {
            base_url,
            base_path,
            authorization,
            timeout: Duration::from_millis(options.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS)),
            transport,
            files: RefCell::new(None),
            text_cache: RefCell::new(HashMap::new()),
            hash_cache: RefCell::new(HashMap::new()),
        })
    }

    /// The configured base URL, with no trailing slash. Never carries a
    /// credential, because the constructor refuses a URL that had one.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// The server-side path the base URL points at, which is what hrefs are
    /// rooted at.
    pub fn base_path(&self) -> &str {
        &self.base_path
    }

    /// Drop the snapshot so the next read observes the current vault.
    ///
    /// The same escape hatch the filesystem backend offers, for the same reason:
    /// this server holds one vault for its whole life, and re-reading it is an
    /// explicit decision rather than a side effect of asking.
    pub fn refresh(&self) {
        *self.files.borrow_mut() = None;
        self.text_cache.borrow_mut().clear();
        self.hash_cache.borrow_mut().clear();
    }

    /// Depth-first, one `PROPFIND` per collection.
    ///
    /// Depth-first because a `PROPFIND` at `Depth: 1` answers with a collection's
    /// own members and nothing below them, so a child collection is only
    /// reachable by asking about it. `is_indexable` is applied to files and
    /// never to collections, exactly as the filesystem walk does: it is the
    /// definition of what a note is, and a second rule for the same question is a
    /// second rule to disagree.
    ///
    /// Boxed at the recursive call because an `async fn` that calls itself has an
    /// infinitely sized future. A vault is a handful of collections deep and this
    /// is once per `list()`, so the box costs one allocation per level.
    ///
    /// `ancestors` exists because the alternative to noticing a cycle is walking
    /// it. `children_of` already drops a listing's self-entry, so a loop needs a
    /// server that names a collection as its own descendant — and one that does
    /// would send this process round the same URL until it died, with a tool
    /// call hanging and nothing to report. Refusing the cycle costs one
    /// membership test and turns the worst outcome into a message naming the
    /// path.
    async fn walk(&self, rel: &str, out: &mut Vec<String>, ancestors: &[String]) -> Result<()> {
        for resource in self.children_of(rel).await? {
            if resource.is_collection {
                if is_hidden_collection(&resource.path) {
                    continue;
                }
                if ancestors.contains(&resource.path) {
                    return Err(cyclic(&resource.path));
                }
                let mut deeper = ancestors.to_vec();
                deeper.push(resource.path.clone());
                Box::pin(self.walk(&resource.path, out, &deeper)).await?;
                continue;
            }
            if is_indexable(&resource.path) {
                out.push(resource.path);
            }
        }
        Ok(())
    }

    /// The members of one collection, excluding the collection itself.
    ///
    /// The self-entry is dropped because a `Depth: 1` listing includes the
    /// collection that was asked about, and recursing into it would walk the
    /// vault forever.
    ///
    /// A REFUSED `PROPFIND` REFUSES here, and that is the one place this backend
    /// deliberately disagrees with the filesystem one, which swallows a failed
    /// `readdir` and indexes whatever else it could reach. The filesystem has a
    /// reason — it is a development and test path, and the entrypoint compensates
    /// by stat-ing the directory before opening it — but over HTTP there is no
    /// such check, and the result of swallowing is the failure this server exists
    /// to avoid: a vault that answers every query with zero rows and no reason.
    /// Surfacing the `403` is more correct than a smaller vault, and the
    /// equivalence suite reports the difference rather than hiding it.
    async fn children_of(&self, rel: &str) -> Result<Vec<DavResource>> {
        let response = self
            .send(
                DavOperation::List,
                WebdavMethod::Propfind,
                rel,
                DavRequestOptions {
                    body: Some(DAV_PROPFIND_BODY.to_string()),
                    depth: Some(Depth::One),
                    collection: true,
                    ..Default::default()
                },
            )
            .await?;
        let resources = parse_multistatus(&response.text(), &self.base_path)?;
        Ok(resources
            .into_iter()
            .filter(|resource| resource.path != rel)
            .collect())
    }

    /// The bytes at `path`, read without consulting or filling the cache.
    ///
    /// Decoded from the raw bytes rather than through a BOM-stripping helper: the
    /// filesystem backend reading with `"utf8"` does not remove a byte-order
    /// mark, so a note that starts with one has to read the same on both
    /// backends.
    async fn fetch_text(&self, operation: DavOperation, path: &str) -> Result<String> {
        let response = self
            .send(
                operation,
                WebdavMethod::Get,
                path,
                DavRequestOptions::default(),
            )
            .await?;
        Ok(response.text())
    }

    /// Send one request and refuse the statuses this operation refuses.
    ///
    /// Public because it is the whole contract of this backend: a caller that
    /// wants to know what it would put on the wire, or what a refusal looks like
    /// with its status still attached, asks here rather than guessing.
    pub async fn send(
        &self,
        operation: DavOperation,
        method: WebdavMethod,
        path: &str,
        options: DavRequestOptions,
    ) -> std::result::Result<DavResponse, WebdavError> {
        let mut headers: Vec<(String, String)> = Vec::new();
        if let Some(authorization) = &self.authorization {
            headers.push(("Authorization".to_string(), authorization.clone()));
        }
        if let Some(depth) = options.depth {
            headers.push(("Depth".to_string(), depth.to_string()));
        }
        // A body with no declared type is a PROPFIND request, and the only body
        // that arrives without one.
        let content_type = options.content_type.or(match &options.body {
            Some(_) => Some(XML_CONTENT_TYPE),
            None => None,
        });
        if let Some(content_type) = content_type {
            headers.push(("Content-Type".to_string(), content_type.to_string()));
        }

        let request = DavRequest {
            url: resource_url(&self.base_url, path, options.collection),
            method,
            headers,
            body: options.body,
            timeout: self.timeout,
        };

        let response = self
            .transport
            .send(request)
            .await
            .map_err(|error| WebdavError {
                // No status, because nothing answered. Reported as a refusal rather
                // than left to escape as a transport error, which the MCP layer
                // would call an internal bug in this server rather than an
                // unreachable one.
                status: None,
                operation,
                method,
                path: path.to_string(),
                error: BasesError::new(format!(
                    "WebDAV {operation}: {method} \"{}\" got no response: {error}",
                    at_root(path)
                ))
                .with_construct("webdav"),
            })?;

        match dav_refusal(
            operation,
            method,
            path,
            response.status,
            &response.status_text,
        ) {
            Some(refusal) => Err(refusal),
            None => Ok(response),
        }
    }
}

/// The bytes a `Basic` credential is, in the one encoding that header allows.
fn base64_encode(plain: String) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(plain)
}

#[async_trait(?Send)]
impl VaultSource for WebdavVaultSource {
    fn kind(&self) -> SourceKind {
        SourceKind::Webdav
    }

    async fn list(&self) -> Result<Vec<String>> {
        if let Some(files) = self.files.borrow().clone() {
            return Ok(files);
        }
        let mut out: Vec<String> = Vec::new();
        self.walk("", &mut out, &[]).await?;
        out.sort();
        *self.files.borrow_mut() = Some(out.clone());
        Ok(out)
    }

    async fn read_note(&self, rel: &str) -> Result<String> {
        let path = vault_relative_path(rel)?;
        if let Some(text) = self.text_cache.borrow().get(&path) {
            return Ok(text.clone());
        }
        let text = normalise_line_endings(&self.fetch_text(DavOperation::Read, &path).await?);
        self.text_cache.borrow_mut().insert(path, text.clone());
        Ok(text)
    }

    /// Bypass the cache and go to the server.
    ///
    /// Same reasoning as the filesystem backend: the write path needs the note as
    /// it is now, not as it was when this process last read it. Obsidian may be
    /// syncing the same vault concurrently, so a cached copy can be arbitrarily
    /// old.
    async fn read_fresh(&self, rel: &str) -> Result<String> {
        let path = vault_relative_path(rel)?;
        let text = normalise_line_endings(&self.fetch_text(DavOperation::Read, &path).await?);
        self.text_cache.borrow_mut().insert(path, text.clone());
        Ok(text)
    }

    /// Size and mtime as the SERVER reports them.
    ///
    /// `getlastmodified` is passed through untouched. It is the only mtime that
    /// exists over WebDAV, and it is a real divergence: the filesystem reports
    /// the local clock at the filesystem's resolution, a server reports its own
    /// clock at its own resolution and in its own timezone. Massaging it towards
    /// the local clock would make the two backends agree on a value that never
    /// existed, so the instant is reported as sent and the equivalence suite
    /// avoids `file.mtime` and `file.ctime` because of it.
    async fn stat(&self, rel: &str) -> Result<FileStat> {
        let path = vault_relative_path(rel)?;
        let response = self
            .send(
                DavOperation::Stat,
                WebdavMethod::Propfind,
                &path,
                DavRequestOptions {
                    body: Some(DAV_PROPFIND_BODY.to_string()),
                    depth: Some(Depth::Zero),
                    ..Default::default()
                },
            )
            .await?;
        let resources = parse_multistatus(&response.text(), &self.base_path)?;
        stat_of(resources.first(), &path)
    }

    /// The shared content hash, over the bytes this backend read.
    ///
    /// [`content_hash`] is the same function the filesystem backend uses, so a
    /// hash taken here means what a hash taken there means. Nothing here reads an
    /// ETag: a server is free to invent one, and the one this targets does not
    /// emit `getetag` at all, so an ETag comparison would be a check that either
    /// always passes or always fails.
    async fn hash(&self, rel: &str) -> Result<String> {
        let path = vault_relative_path(rel)?;
        if let Some(hash) = self.hash_cache.borrow().get(&path) {
            return Ok(hash.clone());
        }
        let hash = content_hash(&self.read_note(&path).await?);
        self.hash_cache.borrow_mut().insert(path, hash.clone());
        Ok(hash)
    }

    /// Ask the server whether the path is taken, bypassing every cache.
    ///
    /// A `PROPFIND` at `Depth: 0` rather than a `HEAD`: this is the same request
    /// [`stat`](Self::stat) already issues for one resource, so a server that
    /// cannot answer it cannot serve this vault either, and no new HTTP verb
    /// enters the vocabulary.
    ///
    /// **Only `404` means absent, and everything else is an error.** The refusal
    /// this guards against is a clobber, so the failure mode that matters is the
    /// one where the server did NOT say the path is free: a `401`, a `503`, a
    /// transport failure, a redirect this client did not follow. Answering any of
    /// them with `false` would report a note as absent precisely when the server
    /// could not be asked, and the caller would then overwrite it -- turning a
    /// connectivity problem into silent data loss, which is the whole thing
    /// `VaultSource::exists` is written to prevent.
    ///
    /// The distinction is structural rather than stringly, and it is made in three
    /// places rather than two: `exists` is not in [`tolerated`], so `send` hands
    /// the `404` back as a refusal; the refusal carries its status as a field, so
    /// it is matched as a number; and `Ok` therefore means a `2xx` and nothing
    /// else. Both halves matter -- tolerating the `404` inside `send` would make
    /// every answer below the `Ok` arm, and the method would report a missing note
    /// as present.
    async fn exists(&self, rel: &str) -> Result<bool> {
        let path = vault_relative_path(rel)?;
        match self
            .send(
                DavOperation::Exists,
                WebdavMethod::Propfind,
                &path,
                DavRequestOptions {
                    body: Some(DAV_PROPFIND_BODY.to_string()),
                    depth: Some(Depth::Zero),
                    ..Default::default()
                },
            )
            .await
        {
            Ok(_) => Ok(true),
            Err(refusal) if refusal.status == Some(404) => Ok(false),
            Err(refusal) => Err(refusal.into()),
        }
    }

    /// Store text, then read it back and refuse anything but the bytes that were
    /// sent.
    ///
    /// The read-back is the whole point of this method over HTTP. A `PUT`
    /// answered `201 Created` is a claim about what the server will do, not a
    /// report of what it did, and a server that accepts a write and stores
    /// something else — a transcoding filter, a `Content-Encoding` it disagreed
    /// with, a bug — is exactly the failure a client cannot detect from the
    /// response. `content_hash` decides it, and no ETag is consulted.
    ///
    /// The collection has to exist already. `MKCOL` is a separate call for a
    /// reason — a `PUT` into a missing collection answers `409`, and the caller
    /// that wanted the collection made said so by calling `ensure_dir`.
    async fn write_text(&self, rel: &str, data: &str) -> Result<()> {
        let path = named_file(&vault_relative_path(rel)?)?.to_string();
        self.send(
            DavOperation::Write,
            WebdavMethod::Put,
            &path,
            DavRequestOptions {
                body: Some(data.to_string()),
                content_type: Some(TEXT_CONTENT_TYPE),
                ..Default::default()
            },
        )
        .await?;

        let sent = content_hash(data);
        // NOT normalised on either side, deliberately: this comparison is about
        // what the server stored on the wire, which is a different question from
        // what a read of this resource will return. Normalising here would accept a
        // server that rewrote line endings on the way through.
        let stored = content_hash(&self.fetch_text(DavOperation::Write, &path).await?);
        if stored != sent {
            return Err(BasesError::new(format!(
                "WebDAV write: PUT \"{path}\" was accepted but the bytes read back are not the \
                 bytes sent (wrote {sent}, read back {stored}). The server stored something else, \
                 so the write is refused rather than reported as done."
            ))
            .with_construct("webdav"));
        }
        // Only now, after the read-back agreed: a cache updated from an
        // unverified write would report the requested bytes for a resource
        // holding others. Normalised, because that is what the next
        // [`VaultSource::read_note`] of this resource will hand back.
        self.text_cache
            .borrow_mut()
            .insert(path.clone(), normalise_line_endings(data));
        self.hash_cache.borrow_mut().remove(&path);
        *self.files.borrow_mut() = None;
        Ok(())
    }

    /// Create intermediate collections, tolerating the ones already there.
    ///
    /// `MKCOL` answers `405 Method Not Allowed` for a collection that exists,
    /// which is the one success the protocol defines as a non-2xx, so `ensure_dir`
    /// reports nothing about whether it did anything — exactly as the
    /// filesystem's `mkdir -p` reports nothing. The listing snapshot is left
    /// alone for the same reason the filesystem one leaves its own: an empty
    /// collection adds no indexable file, and the `write_text` that follows
    /// invalidates it.
    async fn ensure_dir(&self, rel: &str) -> Result<()> {
        let path = vault_relative_path(rel)?;
        if path.is_empty() {
            return Ok(());
        }
        let segments: Vec<&str> = path.split('/').collect();
        for depth in 1..=segments.len() {
            self.send(
                DavOperation::EnsureDir,
                WebdavMethod::Mkcol,
                &segments[..depth].join("/"),
                DavRequestOptions::default(),
            )
            .await?;
        }
        Ok(())
    }

    /// Remove a file, tolerating one that is not there.
    ///
    /// This is the second deliberate divergence from the filesystem backend, and
    /// it is a divergence in the WORDING rather than in the outcome: a WebDAV
    /// `DELETE` does answer `404` for a missing resource, and this source treats
    /// that `404` as the state the caller asked for. The filesystem runs
    /// `rm --force` and reports success for the same call. Both backends say
    /// "gone", and both say it because absence IS the requested state — but the
    /// filesystem never learns that the resource was absent, and a caller that
    /// wanted to know the difference has to ask. Recorded here because the first
    /// caller needs to know which of the two it got.
    async fn delete(&self, rel: &str) -> Result<()> {
        let path = named_file(&vault_relative_path(rel)?)?.to_string();
        self.send(
            DavOperation::Delete,
            WebdavMethod::Delete,
            &path,
            DavRequestOptions::default(),
        )
        .await?;
        self.text_cache.borrow_mut().remove(&path);
        self.hash_cache.borrow_mut().remove(&path);
        *self.files.borrow_mut() = None;
        Ok(())
    }
}
