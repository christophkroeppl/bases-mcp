//! A WebDAV server, in enough of it to be a real client.
//!
//! It serves `PROPFIND` at `Depth: 0` and `Depth: 1`, `GET`, `PUT`, `MKCOL` and
//! `DELETE`, generates hrefs the way a server does (percent-encoded, trailing
//! slash on every collection) and refuses what a server refuses: `405` for a
//! `MKCOL` onto an existing collection, `409` for one whose parent is missing,
//! `404` for a `DELETE` of nothing.
//!
//! The hrefs it generates are the only thing it shares with the backend's
//! mapping, and it keeps them ENCODED, so a request arrives as the raw string the
//! backend built. A decoding mistake in the backend therefore shows up as a file
//! the fake does not recognise, rather than being cancelled out by a decoder on
//! both sides.
//!
//! It is a [`WebdavTransport`] rather than a real socket, so it needs no server,
//! no network and no port. The two hard constraints of the backend are claims
//! about the REQUESTS it makes, and a transport seam is the only place those
//! claims are observable.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::RwLock;

use async_trait::async_trait;
use bases_mcp::error::{BasesError, Result};
use bases_mcp::vault::webdav::{DavRequest, DavResponse, WebdavTransport};

use super::VaultFile;

/// The base URL the whole suite addresses.
pub const BASE_URL: &str = "https://dav.example/dav/vault";
/// The server-side path [`BASE_URL`] points at. Hrefs are rooted here.
pub const BASE_PATH: &str = "/dav/vault";

/// One request the source made, as the server saw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recorded {
    pub method: String,
    pub url: String,
    pub depth: Option<String>,
    pub authorization: Option<String>,
    pub body: Option<String>,
}

impl Recorded {
    pub fn verb_depth(&self) -> (String, Option<String>) {
        (self.method.clone(), self.depth.clone())
    }

    pub fn verb_url(&self) -> (String, String) {
        (self.method.clone(), self.url.clone())
    }
}

/// A status the fake will answer with, wherever a path is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Status {
    pub status: u16,
    pub status_text: &'static str,
}

pub struct FakeDav {
    pub requests: RwLock<Vec<Recorded>>,
    files: RwLock<BTreeMap<String, String>>,
    dirs: RwLock<BTreeSet<String>>,
    /// Per-resource refusals, keyed by vault path, for the failure tests.
    refuse: RwLock<BTreeMap<String, Status>>,
    mtime: RwLock<String>,
    /// When set, every request is refused with this status whatever it is.
    timeout: RwLock<bool>,
}

impl FakeDav {
    /// A server seeded with `files`, with their parent collections created.
    pub fn new(seed: impl IntoIterator<Item = VaultFile>) -> Self {
        let server = Self {
            requests: RwLock::new(Vec::new()),
            files: RwLock::new(BTreeMap::new()),
            dirs: RwLock::new(BTreeSet::from(["".to_string()])),
            refuse: RwLock::new(BTreeMap::new()),
            mtime: RwLock::new("Tue, 01 Oct 2024 10:11:12 GMT".to_string()),
            timeout: RwLock::new(false),
        };
        for file in seed {
            server.store(&file.path, file.content);
        }
        server
    }

    /// Refuse every request on a vault path with this status.
    ///
    /// Takes `&self` and returns nothing rather than returning a builder: the
    /// server is shared behind an `Rc` (the transport trait object is
    /// `'static`, so it cannot borrow a local), and a builder that returned a
    /// borrow would be lending a reference to something the caller cannot keep.
    pub fn refusing(&self, path: &str, status: Status) {
        self.refuse
            .write()
            .unwrap()
            .insert(path.to_string(), status);
    }

    /// The mtime every `getlastmodified` reports, so no assertion is against a
    /// clock.
    pub fn with_mtime(&self, value: &str) {
        *self.mtime.write().unwrap() = value.to_string();
    }

    /// A server that never answers, so the request budget is what ends the call.
    pub fn wedged(&self) {
        *self.timeout.write().unwrap() = true;
    }

    /// The bytes at a vault path, as the server holds them.
    pub fn stored(&self, path: &str) -> Option<String> {
        self.files.read().unwrap().get(path).cloned()
    }

    pub fn has_dir(&self, path: &str) -> bool {
        self.dirs.read().unwrap().contains(path)
    }

    pub fn requests(&self) -> Vec<Recorded> {
        self.requests.read().unwrap().clone()
    }

    fn store(&self, path: &str, content: String) {
        let segments: Vec<&str> = path.split('/').collect();
        let mut dirs = self.dirs.write().unwrap();
        // Every ancestor becomes a collection; the path itself does not, because
        // a file is not a collection and the fake answers `Depth: 1` for each.
        for depth in 1..segments.len() {
            dirs.insert(segments[..depth].join("/"));
        }
        drop(dirs);
        self.files
            .write()
            .unwrap()
            .insert(path.to_string(), content);
    }

    /// The href for a vault path, encoded the way a server encodes one.
    fn href(&self, path: &str) -> String {
        if path.is_empty() {
            return format!("{BASE_URL}/");
        }
        format!(
            "{BASE_URL}/{}",
            path.split('/')
                .map(encode_uri_component)
                .collect::<Vec<_>>()
                .join("/")
        )
    }

    /// The vault path a request URL names, as this server stores it.
    ///
    /// Decoding here is the server's own business and shares no code with the
    /// backend's href mapping: the two meet only at the wire, so a mistake in the
    /// backend's outbound encoding reaches the fake as a path it does not hold
    /// rather than being cancelled out by a shared decoder.
    fn path_of(&self, url: &reqwest::Url) -> Option<String> {
        let pathname = url.path();
        let base = BASE_PATH;
        let rest = pathname.strip_prefix(base)?;
        Some(
            rest.split('/')
                .filter(|segment| !segment.is_empty())
                .map(|segment| {
                    percent_encoding::percent_decode_str(segment)
                        .decode_utf8_lossy()
                        .into_owned()
                })
                .collect::<Vec<_>>()
                .join("/"),
        )
    }

    /// Direct children of a vault path, plus the path itself, in sorted order.
    fn members_of(&self, path: &str) -> Vec<String> {
        let prefix = if path.is_empty() {
            String::new()
        } else {
            format!("{path}/")
        };
        let mut all: BTreeSet<String> = BTreeSet::new();
        for file in self.files.read().unwrap().keys() {
            if file.starts_with(&prefix) {
                all.insert(format!("{prefix}{}", first_segment(file, &prefix)));
            }
        }
        for dir in self.dirs.read().unwrap().iter() {
            if dir != path && dir.starts_with(&prefix) {
                all.insert(format!("{prefix}{}", first_segment(dir, &prefix)));
            }
        }
        let mut members = vec![path.to_string()];
        members.extend(all);
        members
    }

    fn propfind(&self, path: &str, depth: Option<&str>) -> DavResponse {
        let known = self.dirs.read().unwrap().contains(path)
            || self.files.read().unwrap().contains_key(path);
        if !known {
            return refusal(404, "Not Found");
        }
        if self.dirs.read().unwrap().contains(path) && depth == Some("1") {
            let members = self.members_of(path);
            return multistatus(&members.iter().map(|m| self.resource(m)).collect::<Vec<_>>());
        }
        multistatus(&[self.resource(path)])
    }

    fn resource(&self, path: &str) -> String {
        let collection = self.dirs.read().unwrap().contains(path);
        let properties = if collection {
            "<D:resourcetype><D:collection/></D:resourcetype>".to_string()
        } else {
            let size = self
                .files
                .read()
                .unwrap()
                .get(path)
                .map(String::len)
                .unwrap_or(0);
            format!("<D:resourcetype/><D:getcontentlength>{size}</D:getcontentlength>")
        };
        format!(
            "<D:response><D:href>{}</D:href><D:propstat><D:prop>{properties}<D:getlastmodified>{}\
             </D:getlastmodified></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat>\
             </D:response>",
            escape_xml(&self.href(path)),
            self.mtime.read().unwrap()
        )
    }

    fn mkcol(&self, path: &str) -> DavResponse {
        if self.dirs.read().unwrap().contains(path) || self.files.read().unwrap().contains_key(path)
        {
            return refusal(405, "Method Not Allowed");
        }
        if !self.dirs.read().unwrap().contains(parent_of(path).as_str()) {
            return refusal(409, "Conflict");
        }
        self.dirs.write().unwrap().insert(path.to_string());
        refusal(201, "Created")
    }
}

#[async_trait]
impl WebdavTransport for FakeDav {
    async fn send(&self, request: DavRequest) -> Result<DavResponse> {
        let url = reqwest::Url::parse(&request.url).map_err(|e| BasesError::new(e.to_string()))?;
        self.requests.write().unwrap().push(Recorded {
            method: request.method.as_str().to_string(),
            url: url.to_string(),
            depth: request.header("Depth").map(str::to_string),
            authorization: request.header("Authorization").map(str::to_string),
            body: request.body.clone(),
        });

        if *self.timeout.read().unwrap() {
            // The transport budget elapsed. A real client would see a timeout
            // here, and the backend has to turn that into a refusal naming the
            // operation rather than hang.
            return Err(BasesError::new("The operation timed out"));
        }

        let Some(path) = self.path_of(&url) else {
            return Ok(refusal(404, "Not Found"));
        };
        if let Some(blocked) = self.refuse.read().unwrap().get(&path) {
            return Ok(refusal(blocked.status, blocked.status_text));
        }

        // A collection URL ends in `/` and a file URL does not, and a server
        // that is handed the wrong one answers 404. Stated as a rule here so a
        // backend that dropped the slash is refused the way a real server refuses
        // it, rather than quietly served -- this is the one part of the fake a
        // backend can be wrong about without the wire noticing.
        let is_collection = self.dirs.read().unwrap().contains(&path);
        let trailing = url.path().ends_with('/');
        // `Depth: 0` is exempt: a properties request for a collection is
        // answered by plenty of servers whichever way the URL is written, and
        // refusing it here would mean the backend never got to say "that is a
        // collection, not a note".
        let properties =
            request.method.as_str() == "PROPFIND" && request.header("Depth") == Some("0");
        let names_a_collection = matches!(request.method.as_str(), "PUT" | "MKCOL");
        if !names_a_collection && !properties && trailing != is_collection {
            return Ok(refusal(404, "Not Found"));
        }

        Ok(match request.method.as_str() {
            "PROPFIND" => self.propfind(&path, request.header("Depth")),
            "GET" => match self.files.read().unwrap().get(&path) {
                Some(text) => DavResponse::new(200, "OK", text.clone().into_bytes()),
                None => refusal(404, "Not Found"),
            },
            "PUT" => {
                if !self
                    .dirs
                    .read()
                    .unwrap()
                    .contains(parent_of(&path).as_str())
                {
                    refusal(409, "Conflict")
                } else {
                    self.store(&path, request.body.unwrap_or_default());
                    refusal(201, "Created")
                }
            }
            "MKCOL" => self.mkcol(&path),
            "DELETE" => {
                if self.files.write().unwrap().remove(&path).is_none() {
                    refusal(404, "Not Found")
                } else {
                    refusal(204, "No Content")
                }
            }
            _ => refusal(405, "Method Not Allowed"),
        })
    }
}

/// The vault path of a path's parent collection. A root-level file's is the
/// root.
fn parent_of(path: &str) -> String {
    match path.rfind('/') {
        None => String::new(),
        Some(cut) => path[..cut].to_string(),
    }
}

fn first_segment<'a>(path: &'a str, prefix: &'a str) -> &'a str {
    path[prefix.len()..].split('/').next().unwrap_or(path)
}

fn escape_xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// A `207 Multi-Status` body for the given `response` blocks.
fn multistatus(responses: &[String]) -> DavResponse {
    DavResponse::new(
        207,
        "Multi-Status",
        format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?><D:multistatus xmlns:D=\"DAV:\">{}\
             </D:multistatus>",
            responses.join("")
        )
        .into_bytes(),
    )
}

pub fn refusal(status: u16, status_text: &str) -> DavResponse {
    DavResponse::new(status, status_text, Vec::new())
}

/// The one `encodeURIComponent` a server's href generator shares with the
/// backend's outbound encoder, written out twice on purpose: the fake is the
/// wire, and a shared function would hide a mistake in either.
fn encode_uri_component(segment: &str) -> String {
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
