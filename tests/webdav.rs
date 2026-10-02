//! The WebDAV backend, tier 1.
//!
//! Tier 1 needs no server and runs in the ordinary unit suite: a fake WebDAV
//! transport stands in for the network, and the two hard constraints of this
//! backend are asserted as properties of the requests it makes. That is the only
//! way to pin them. "Recurse with repeated `Depth: 1`, never `Depth: infinity`"
//! and "read a listing rather than a file per note" are claims about headers and
//! about which requests happen at all, so the test records both and reads them
//! back.
//!
//! Tier 2 of `test/webdav/webdav.test.ts` runs against a live server and is gated
//! on `BASES_MCP_WEBDAV_URL`. It is not ported: a skipped test that needs a
//! server to exist is a test that rots, and the project already had to fix that
//! once in the parity suite. The gate stays in the TypeScript tree.
//!
//! What the equivalence suite already covers is not repeated here: that a
//! `VaultSource` produces the same answers as the filesystem one. What it could
//! not cover, because the backend under test there is an in-memory map with no
//! HTTP in it, is whether THIS module's href mapping, XML parsing and request
//! discipline are right. That is tier 1's job, and it is compared against the
//! same oracle the equivalence suite uses: `test/vault`, read by
//! `FsVaultSource`.
//!
//! ## What is not here, and why
//!
//! The last three tests of the original file — "the WebDAV backend behind the
//! Resolver" — need `src/base.rs`, `src/render/markdown.rs` and
//! `src/service.rs`, which a later task owns. They are named in the porting
//! report rather than approximated with a stand-in resolver.

// Each integration test is its own crate, and no two of them need every helper here.
// Each integration test is its own crate, and no suite needs every helper here.
#[allow(dead_code)]
mod common;

use bases_mcp::error::BasesError;
use bases_mcp::vault::fs::content_hash;
use bases_mcp::vault::webdav::{
    dav_refusal, parse_dav_date, parse_multistatus, vault_path_from_href, vault_relative_path,
    DavOperation, DavRequest, DavRequestOptions, DavResponse, Depth, WebdavError, WebdavMethod,
    WebdavTransport, WebdavVaultOptions, WebdavVaultSource, DAV_PROPFIND_BODY,
};
use bases_mcp::vault::{is_indexable, FsVaultSource, VaultSource};
use std::rc::Rc;

use common::fake_dav::{FakeDav, Recorded, Status, BASE_PATH, BASE_URL};
use common::{futures_block_on, load_corpus, vault_dir, VaultFile, CORPUS_SIZE};

/// The mtime every fake server hands out, so no assertion is against a clock.
const SERVER_MTIME: &str = "Tue, 01 Oct 2024 10:11:12 GMT";
/// `Date.UTC(2024, 9, 1, 10, 11, 12)` in the original.
const SERVER_MTIME_MS: i64 = 1_727_777_472_000;

const USER: &str = "agent";
const PASSWORD: &str = "correct horse battery staple";

/// A `207 Multi-Status` body as a server actually sends one.
///
/// Written out rather than generated, because the point of the parsing tests is
/// that the shapes a real server emits are understood: a namespace prefix, a
/// collection self-entry, a percent-encoded href, an empty `<resourcetype/>` for a
/// file, a `404` propstat sitting beside a `200` one, and properties split across
/// two propstats. Every value here is asserted by name below, so a change in what
/// is extracted cannot pass as a change in a fixture.
const CANNED: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<D:multistatus xmlns:D="DAV:">
  <D:response>
    <D:href>/dav/vault/</D:href>
    <D:propstat>
      <D:prop>
        <D:resourcetype><D:collection/></D:resourcetype>
        <D:getlastmodified>Tue, 01 Oct 2024 09:00:00 GMT</D:getlastmodified>
      </D:prop>
      <D:status>HTTP/1.1 200 OK</D:status>
    </D:propstat>
  </D:response>
  <D:response>
    <D:href>/dav/vault/Root%20Project.md</D:href>
    <D:propstat>
      <D:prop>
        <D:resourcetype/>
        <D:getcontentlength>512</D:getcontentlength>
        <D:getlastmodified>Tue, 01 Oct 2024 10:11:12 GMT</D:getlastmodified>
      </D:prop>
      <D:status>HTTP/1.1 200 OK</D:status>
    </D:propstat>
  </D:response>
  <D:response>
    <D:href>/dav/vault/Projects/</D:href>
    <D:propstat>
      <D:prop>
        <D:resourcetype><D:collection/></D:resourcetype>
        <D:getlastmodified>Tue, 01 Oct 2024 12:00:00 +0200</D:getlastmodified>
      </D:prop>
      <D:status>HTTP/1.1 200 OK</D:status>
    </D:propstat>
  </D:response>
  <D:response>
    <D:href>/dav/vault/Projects/SomeProject.md</D:href>
    <D:propstat>
      <D:prop>
        <D:resourcetype/>
        <D:getcontentlength>8</D:getcontentlength>
        <D:getlastmodified>Tue, 01 Oct 2024 12:00:00 +0200</D:getlastmodified>
      </D:prop>
      <D:status>HTTP/1.1 200 OK</D:status>
    </D:propstat>
    <D:propstat>
      <D:prop><D:getetag>"unproven"</D:getetag></D:prop>
      <D:status>HTTP/1.1 404 Not Found</D:status>
    </D:propstat>
  </D:response>
  <D:response>
    <D:href>/dav/vault/Tickets/Fix%20login%20redirect.md</D:href>
    <D:propstat>
      <D:prop>
        <D:resourcetype/>
        <D:getcontentlength>0</D:getcontentlength>
        <D:getlastmodified>Tue, 01 Oct 2024 09:30:00 GMT</D:getlastmodified>
      </D:prop>
      <D:status>HTTP/1.1 200 OK</D:status>
    </D:propstat>
  </D:response>
  <D:response>
    <D:href>/dav/vault/Caf%C3%A9/50%25.md</D:href>
    <D:propstat>
      <D:prop>
        <D:resourcetype/>
        <D:getcontentlength>17</D:getcontentlength>
        <D:getlastmodified>Tue, 01 Oct 2024 09:45:00 GMT</D:getlastmodified>
      </D:prop>
      <D:status>HTTP/1.1 200 OK</D:status>
    </D:propstat>
  </D:response>
  <D:response>
    <D:href>/dav/vault/Deleted%20While%20Listing.md</D:href>
    <D:propstat>
      <D:prop><D:getcontentlength>10</D:getcontentlength></D:prop>
      <D:status>HTTP/1.1 404 Not Found</D:status>
    </D:propstat>
  </D:response>
</D:multistatus>"#;

/// A `PROPFIND` answer carrying a size but no `getlastmodified`.
const NO_MTIME_PROPFIND: &str = r#"<?xml version="1.0"?><D:multistatus xmlns:D="DAV:"><D:response>
  <D:href>/dav/vault/Note.md</D:href><D:propstat><D:prop><D:resourcetype/>
  <D:getcontentlength>4</D:getcontentlength></D:prop>
  <D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>"#;

/// A `PROPFIND` answer carrying an mtime but no `getcontentlength`.
const NO_SIZE_PROPFIND: &str = r#"<?xml version="1.0"?><D:multistatus xmlns:D="DAV:"><D:response>
  <D:href>/dav/vault/Note.md</D:href><D:propstat><D:prop><D:resourcetype/>
  <D:getlastmodified>Tue, 01 Oct 2024 10:11:12 GMT</D:getlastmodified></D:prop>
  <D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>"#;

/// A `207 Multi-Status` body listing nothing.
const EMPTY_LISTING: &str = r#"<?xml version="1.0"?><D:multistatus xmlns:D="DAV:"/>"#;

// ---------------------------------------------------------------------------
// A source over a fake server
// ---------------------------------------------------------------------------

/// A fake server, shared.
///
/// `Rc` because the transport a source owns is `'static`: it cannot borrow a
/// local, so the fake has to outlive the constructor that hands it over. Every
/// test keeps its own handle so it can read the request log afterwards.
fn fake(seed: impl IntoIterator<Item = VaultFile>) -> Rc<FakeDav> {
    Rc::new(FakeDav::new(seed))
}

/// A source over a fake server, with Basic auth, at the fake's base URL.
fn source_over(server: &Rc<FakeDav>) -> WebdavVaultSource {
    source_over_with(server, Some(USER), Some(PASSWORD), None)
}

fn source_over_with(
    server: &Rc<FakeDav>,
    user: Option<&str>,
    password: Option<&str>,
    timeout_ms: Option<u64>,
) -> WebdavVaultSource {
    let mut options = WebdavVaultOptions::new(BASE_URL)
        .with_transport(Box::new(SharedTransport(Rc::clone(server))));
    if let Some(user) = user {
        options.user = Some(user.to_string());
    }
    if let Some(password) = password {
        options.password = Some(password.to_string());
    }
    options.timeout_ms = timeout_ms;
    WebdavVaultSource::new(options).expect("the fake's base URL is a valid one")
}

/// A source that answers with a fixed body whatever it is asked.
fn source_answering(response: DavResponse) -> WebdavVaultSource {
    let transport = move |_request: DavRequest| {
        let response = response.clone();
        async move { Ok(response) }
    };
    WebdavVaultSource::new(
        WebdavVaultOptions::new(BASE_URL).with_transport(Box::new(ClosureTransport(transport))),
    )
    .expect("the base URL is valid")
}

/// A transport that answers with whatever the closure decides.
pub struct ClosureTransport<F>(pub F);

#[async_trait::async_trait(?Send)]
impl<F, Fut> WebdavTransport for ClosureTransport<F>
where
    F: Fn(DavRequest) -> Fut,
    Fut: std::future::Future<Output = Result<DavResponse, BasesError>>,
{
    async fn send(&self, request: DavRequest) -> Result<DavResponse, BasesError> {
        (self.0)(request).await
    }
}

/// A `WebdavVaultSource` owns its transport for `'static`, so a fake held in a
/// local cannot be borrowed by one. `Rc` rather than `Arc` because the transport
/// futures are `?Send` for the same reason the source's are, and the test that
/// made the request has to be able to read the log afterwards.
pub struct SharedTransport(pub Rc<FakeDav>);

#[async_trait::async_trait(?Send)]
impl WebdavTransport for SharedTransport {
    async fn send(&self, request: DavRequest) -> Result<DavResponse, BasesError> {
        self.0.send(request).await
    }
}

fn run<F: std::future::Future>(future: F) -> F::Output {
    futures_block_on(future)
}

/// The canned listing, keyed by the vault path it maps to.
fn canned_by_path() -> std::collections::BTreeMap<String, bases_mcp::vault::webdav::DavResource> {
    parse_multistatus(CANNED, BASE_PATH)
        .expect("the canned body is a multistatus")
        .into_iter()
        .map(|resource| (resource.path.clone(), resource))
        .collect()
}

fn corpus_source(server: &Rc<FakeDav>) -> WebdavVaultSource {
    source_over(server)
}

fn corpus_files() -> Vec<VaultFile> {
    load_corpus()
}

/// The failure a call produced.
fn failure_of<F, T>(future: F) -> BasesError
where
    F: std::future::Future<Output = Result<T, BasesError>>,
    T: std::fmt::Debug,
{
    match run(future) {
        Err(error) => error,
        Ok(_) => panic!("expected a refusal, but the call succeeded"),
    }
}

// ---------------------------------------------------------------------------
// href to vault-relative path
// ---------------------------------------------------------------------------

#[test]
fn strips_the_server_base_path_and_percent_decodes_what_is_left() {
    assert_eq!(
        vault_path_from_href("/dav/vault/Root%20Project.md", BASE_PATH).expect("inside the base"),
        "Root Project.md"
    );
    assert_eq!(
        vault_path_from_href("/dav/vault/Tickets/Fix%20login%20redirect.md", BASE_PATH)
            .expect("inside the base"),
        "Tickets/Fix login redirect.md"
    );
}

#[test]
fn a_collection_href_loses_its_trailing_slash_so_it_is_a_path_and_not_a_name() {
    // The filesystem walk passes `Projects`, never `Projects/`, and a listing
    // compared against it would be off by a slash on every directory.
    assert_eq!(
        vault_path_from_href("/dav/vault/Projects/", BASE_PATH).expect("a collection"),
        "Projects"
    );
    assert_eq!(
        vault_path_from_href("/dav/vault/", BASE_PATH).expect("the root"),
        ""
    );
}

#[test]
fn the_collection_self_reference_maps_to_the_collections_own_vault_path() {
    for (href, path) in [("/dav/vault/", ""), ("/dav/vault/Projects/", "Projects")] {
        assert_eq!(
            vault_path_from_href(href, BASE_PATH).expect("a collection"),
            path,
            "{href}"
        );
    }
}

#[test]
fn decodes_a_percent_escape_not_the_escape_character() {
    // The two traps in one name: a non-ASCII byte, and a literal `%` that a naive
    // decode would treat as the start of an escape.
    assert_eq!(
        vault_path_from_href("/dav/vault/Caf%C3%A9/50%25.md", BASE_PATH).expect("a note"),
        "Café/50%.md"
    );
}

#[test]
fn leaves_a_plus_alone_because_it_is_not_a_space_in_a_path() {
    assert_eq!(
        vault_path_from_href("/dav/vault/A+B.md", BASE_PATH).expect("a note"),
        "A+B.md"
    );
}

#[test]
fn accepts_a_full_url_which_rfc_4918_allows_and_servers_do_send() {
    assert_eq!(
        vault_path_from_href("https://dav.example/dav/vault/Root%20Ticket.md", BASE_PATH)
            .expect("a full URL href"),
        "Root Ticket.md"
    );
}

#[test]
fn a_base_path_with_an_escape_of_its_own_is_compared_decoded_so_it_still_matches() {
    assert_eq!(
        vault_path_from_href("/dav/My%20Vault/Note.md", "/dav/My%20Vault")
            .expect("an encoded base"),
        "Note.md"
    );
}

#[test]
fn refuses_an_href_outside_the_base_rather_than_indexing_another_tree() {
    let error = vault_path_from_href("/elsewhere/Secret.md", BASE_PATH)
        .expect_err("another tree is refused");
    assert!(
        error
            .message()
            .contains("outside the configured vault base"),
        "{error}"
    );
}

#[test]
fn refuses_a_path_that_escapes_the_vault_root_through_dot_dot() {
    // The base path is stripped BEFORE `..` is folded, which is the only order in
    // which this href is an escape rather than a walk up to `/dav/etc/passwd`.
    let error = vault_path_from_href("/dav/vault/../etc/passwd", BASE_PATH)
        .expect_err("an escape is refused");
    assert!(
        error.message().contains("escapes the vault root"),
        "{error}"
    );
}

#[test]
fn resolves_a_redundant_dot_and_dot_dot_rather_than_keeping_them_in_a_notes_path() {
    assert_eq!(
        vault_path_from_href("/dav/vault/Tickets/../Root%20Ticket.md", BASE_PATH)
            .expect("a redundant path"),
        "Root Ticket.md"
    );
}

#[test]
fn refuses_malformed_percent_encoding_instead_of_decoding_it_to_nonsense() {
    let error =
        vault_path_from_href("/dav/vault/100%.md", BASE_PATH).expect_err("a bare % is malformed");
    assert!(
        error.message().contains("not valid percent-encoding"),
        "{error}"
    );
}

// ---------------------------------------------------------------------------
// Caller-supplied paths
// ---------------------------------------------------------------------------

#[test]
fn resolves_a_redundant_path_to_the_note_it_names_as_the_filesystem_backend_does() {
    assert_eq!(
        vault_relative_path("Tickets/../Root Ticket.md").expect("a redundant path"),
        "Root Ticket.md"
    );
    assert_eq!(
        vault_relative_path("./Tickets/Fix login redirect.md").expect("a redundant path"),
        "Tickets/Fix login redirect.md"
    );
}

#[test]
fn refuses_every_escape_including_a_leading_slash() {
    for escapee in ["../outside.md", "/etc/passwd", "Tickets/../../outside.md"] {
        let error = vault_relative_path(escapee).expect_err("an escape is refused");
        assert!(
            error.message().contains("escapes the vault root"),
            "{escapee}: {error}"
        );
    }
}

#[test]
fn an_escape_is_refused_before_any_request_is_made() {
    // Structural rather than promised: the guard runs on the argument, so a path
    // from an agent cannot become a URL outside the vault even if the guard were
    // later moved. The empty request log is the assertion.
    let server = fake([]);
    let source = source_over(&server);
    let _ = failure_of(source.read_text("../../etc/passwd"));
    assert!(server.requests().is_empty());
}

/// A backslash is a literal here, and stays one all the way onto the wire.
///
/// This is where the two backends genuinely disagree, and the disagreement is
/// deliberate. `VaultSource::list` documents its paths as vault-relative POSIX, and
/// `\` is not a legal character in one — but over HTTP it cannot walk out of the
/// vault even so, because `encode_segment` percent-encodes it and the server
/// decodes `%5C` back to a character rather than to a separator. So the same
/// argument that the filesystem backend has to refuse is here an ordinary file
/// name, and refusing it would make a note this server genuinely holds unreadable.
///
/// The place the two are made to agree is `assert_note_path`, which every backend
/// passes through, rather than here.
#[test]
fn a_backslash_stays_a_literal_segment_rather_than_walking_out_of_the_vault() {
    assert_eq!(
        vault_relative_path(r"..\..\outside\evil.md").expect("not an escape over HTTP"),
        r"..\..\outside\evil.md"
    );
    assert_eq!(
        vault_relative_path(r"Tickets\..\..\outside\evil.md").expect("not an escape over HTTP"),
        r"Tickets\..\..\outside\evil.md"
    );
}

#[test]
fn a_backslash_reaches_the_server_as_percent_encoded_and_one_path_segment() {
    // The encoding is load-bearing rather than incidental: a literal `\` in a URL
    // path is legal but a server is free to normalise it, and the request this
    // backend makes must name ONE resource inside the vault.
    let server = fake([]);
    let source = source_over(&server);
    let _ = failure_of(source.read_text(r"..\..\outside\evil.md"));

    assert_eq!(
        server.requests()[0].url,
        format!("{BASE_URL}/..%5C..%5Coutside%5Cevil.md")
    );
}

// ---------------------------------------------------------------------------
// A 207 Multi-Status body
// ---------------------------------------------------------------------------

#[test]
fn extracts_every_resource_by_the_vault_path_it_maps_to() {
    let paths: Vec<String> = parse_multistatus(CANNED, BASE_PATH)
        .expect("a multistatus")
        .into_iter()
        .map(|r| r.path)
        .collect();
    assert_eq!(
        paths,
        [
            "",
            "Root Project.md",
            "Projects",
            "Projects/SomeProject.md",
            "Tickets/Fix login redirect.md",
            "Café/50%.md",
        ]
    );
}

#[test]
fn tells_a_collection_from_a_file_on_the_resourcetype_element_alone() {
    let by_path = canned_by_path();
    assert!(by_path[""].is_collection);
    assert!(by_path["Projects"].is_collection);
    assert!(!by_path["Root Project.md"].is_collection);
    assert!(!by_path["Café/50%.md"].is_collection);
}

#[test]
fn extracts_the_byte_size_verbatim_and_unrounded() {
    let by_path = canned_by_path();
    assert_eq!(by_path["Root Project.md"].size, Some(512));
    assert_eq!(by_path["Projects/SomeProject.md"].size, Some(8));
    assert_eq!(by_path["Café/50%.md"].size, Some(17));
    assert_eq!(by_path["Projects"].size, None);
}

#[test]
fn extracts_getlastmodified_as_the_string_the_server_sent() {
    let by_path = canned_by_path();
    assert_eq!(
        by_path["Root Project.md"].last_modified.as_deref(),
        Some(SERVER_MTIME)
    );
    assert_eq!(
        by_path["Projects"].last_modified.as_deref(),
        Some("Tue, 01 Oct 2024 12:00:00 +0200")
    );
}

#[test]
fn drops_a_resource_whose_only_propstat_is_a_404() {
    // A listing can name a file that was deleted between the request and the
    // response. Indexing it would put a path in `list()` that a later `read_text`
    // cannot serve.
    let paths: Vec<String> = parse_multistatus(CANNED, BASE_PATH)
        .expect("a multistatus")
        .into_iter()
        .map(|r| r.path)
        .collect();
    assert!(!paths.contains(&"Deleted While Listing.md".to_string()));
}

#[test]
fn keeps_the_properties_from_a_200_propstat_when_a_sibling_propstat_is_a_404() {
    // The 404 block is for a property the server declined to report, not for the
    // resource. Reading only the first block, or merging blindly, loses the size.
    let by_path = canned_by_path();
    assert_eq!(by_path["Projects/SomeProject.md"].size, Some(8));
    assert_eq!(
        by_path["Projects/SomeProject.md"].last_modified.as_deref(),
        Some("Tue, 01 Oct 2024 12:00:00 +0200")
    );
}

#[test]
fn an_empty_collection_is_an_empty_listing_not_a_malformed_document() {
    assert!(parse_multistatus(EMPTY_LISTING, BASE_PATH)
        .expect("an empty collection is fine")
        .is_empty());
}

#[test]
fn refuses_a_body_that_is_not_a_multistatus_at_all() {
    // A login page or an HTML error body parsed as a vault is a vault with no
    // notes, which is the one confusion this server exists to avoid.
    for body in [
        "<html><body>404</body></html>",
        r#"<?xml version="1.0"?><D:error xmlns:D="DAV:"><D:status>403 Forbidden</D:status></D:error>"#,
    ] {
        let error = parse_multistatus(body, BASE_PATH).expect_err("not a multistatus");
        assert!(
            error.message().contains("did not return a multistatus"),
            "{error}"
        );
    }
}

// ---------------------------------------------------------------------------
// getlastmodified
// ---------------------------------------------------------------------------

#[test]
fn reads_the_instant_in_the_servers_own_timezone() {
    assert_eq!(
        parse_dav_date(SERVER_MTIME, "Note.md")
            .expect("a date")
            .timestamp_millis(),
        SERVER_MTIME_MS
    );
    assert_eq!(
        parse_dav_date("Tue, 01 Oct 2024 12:00:00 +0200", "Note.md")
            .expect("a date")
            .timestamp_millis(),
        1_727_776_800_000
    );
}

#[test]
fn reads_the_two_obsolete_formats_rfc_9110_still_allows() {
    assert_eq!(
        parse_dav_date("Tuesday, 01-Oct-24 10:11:12 GMT", "Note.md")
            .expect("an RFC 850 date")
            .timestamp_millis(),
        SERVER_MTIME_MS
    );
    assert_eq!(
        parse_dav_date("Tue Oct  1 10:11:12 2024", "Note.md")
            .expect("an asctime date")
            .timestamp_millis(),
        SERVER_MTIME_MS
    );
}

#[test]
fn refuses_a_date_it_cannot_read_rather_than_yielding_an_invalid_date() {
    // An unusable date reaches `file.mtime` and then a rendered cell as `NaN`,
    // which looks like data.
    let error = parse_dav_date("yesterday-ish", "Note.md").expect_err("not a date");
    assert!(
        error.message().contains("not a date this client can read"),
        "{error}"
    );
    assert!(error.message().contains("yesterday-ish"), "{error}");
}

// ---------------------------------------------------------------------------
// The response classifier
// ---------------------------------------------------------------------------

#[test]
fn a_2xx_passes_and_207_with_it() {
    for status in [200, 201, 204, 207, 299] {
        assert!(dav_refusal(
            DavOperation::Read,
            WebdavMethod::Get,
            "Note.md",
            status,
            "OK"
        )
        .is_none());
    }
}

#[test]
fn mkcol_tolerates_405_because_the_collection_is_already_there() {
    assert!(dav_refusal(
        DavOperation::EnsureDir,
        WebdavMethod::Mkcol,
        "Projects",
        405,
        "Method Not Allowed"
    )
    .is_none());
}

#[test]
fn delete_tolerates_404_because_the_resource_is_already_not_there() {
    // The deliberate divergence from the filesystem backend, which runs
    // `rm --force`. Both backends report "gone"; only this one was told `404`
    // first. `tests/vault_equivalence.rs` pins the filesystem half.
    assert!(dav_refusal(
        DavOperation::Delete,
        WebdavMethod::Delete,
        "Note.md",
        404,
        "Not Found"
    )
    .is_none());
}

#[test]
fn the_tolerance_is_the_operations_not_a_blanket_one() {
    // 405 on a read is a server that does not PROPFIND, and 404 on a read is a
    // note that is not there. Neither is the MKCOL or DELETE answer.
    assert!(dav_refusal(
        DavOperation::Read,
        WebdavMethod::Get,
        "Note.md",
        404,
        "Not Found"
    )
    .is_some());
    assert!(dav_refusal(
        DavOperation::List,
        WebdavMethod::Propfind,
        "",
        405,
        "Method Not Allowed"
    )
    .is_some());
    assert!(dav_refusal(
        DavOperation::Write,
        WebdavMethod::Put,
        "Note.md",
        404,
        "Not Found"
    )
    .is_some());
}

#[test]
fn every_other_status_refuses_a_redirect_included() {
    for status in [301, 302, 400, 401, 403, 500, 503] {
        assert!(
            dav_refusal(DavOperation::List, WebdavMethod::Propfind, "", status, "").is_some(),
            "{status}"
        );
    }
}

#[test]
fn the_refusal_names_the_operation_the_method_the_status_and_the_path() {
    let error = dav_refusal(
        DavOperation::Read,
        WebdavMethod::Get,
        "Tickets/Fix login redirect.md",
        404,
        "Not Found",
    )
    .expect("a 404 on a read is refused");
    assert_eq!(
        error.error.display_message(),
        "WebDAV read: GET \"Tickets/Fix login redirect.md\": 404 Not Found (construct: webdav)"
    );
    assert_eq!(error.status, Some(404));
    assert_eq!(error.operation, DavOperation::Read);
    assert_eq!(error.method, WebdavMethod::Get);
    assert_eq!(error.path, "Tickets/Fix login redirect.md");
}

#[test]
fn a_refusal_is_a_structured_failure_so_the_mcp_layer_can_report_it() {
    // Not an internal error: a server refusing a request is the server's answer,
    // and it must reach the agent as a structured result naming the operation.
    let error = dav_refusal(
        DavOperation::Stat,
        WebdavMethod::Propfind,
        "Note.md",
        500,
        "Internal Server Error",
    )
    .expect("a 500 is refused");
    assert_eq!(error.status, Some(500));
    assert_eq!(error.error.construct(), Some("webdav"));
}

// ---------------------------------------------------------------------------
// The requests the backend makes
// ---------------------------------------------------------------------------

#[test]
fn asks_for_one_collection_at_a_time_and_never_for_infinity() {
    let server = fake(corpus_files());
    run(corpus_source(&server).list()).expect("the fake serves the corpus");

    let verb_depth: Vec<(String, Option<String>)> =
        server.requests().iter().map(Recorded::verb_depth).collect();
    assert_eq!(
        verb_depth,
        [
            ("PROPFIND".to_string(), Some("1".to_string())),
            ("PROPFIND".to_string(), Some("1".to_string())),
            ("PROPFIND".to_string(), Some("1".to_string())),
        ]
    );
    // One request per collection, and nothing else. `Depth: infinity` would make
    // this a single request, and no mainstream server would answer it.
    let urls: Vec<String> = server.requests().iter().map(|r| r.url.clone()).collect();
    assert_eq!(
        urls,
        [
            format!("{BASE_URL}/"),
            format!("{BASE_URL}/Projects/"),
            format!("{BASE_URL}/Tickets/")
        ]
    );
}

#[test]
fn the_depth_type_cannot_express_infinity() {
    // The mechanism behind the assertion above, pinned on its own so a future
    // change to the walk cannot quietly reintroduce `infinity` without breaking
    // something that says so: there is no spelling of it.
    assert_eq!(Depth::Zero.as_str(), "0");
    assert_eq!(Depth::One.as_str(), "1");
    assert_eq!(Depth::One.to_string(), "1");
    // The fake refuses anything it does not recognise as a depth it serves, so a
    // stray `infinity` would not even parse into a listing.
    let server = fake(corpus_files());
    let source = source_over(&server);
    let _ = run(source.send(
        DavOperation::List,
        WebdavMethod::Propfind,
        "",
        DavRequestOptions {
            depth: Some(Depth::One),
            collection: true,
            ..Default::default()
        },
    ));
    assert!(
        server
            .requests()
            .iter()
            .all(|r| r.depth.as_deref() != Some("infinity")),
        "{:?}",
        server.requests()
    );
}

#[test]
fn asks_for_the_three_properties_it_reads_and_never_for_an_etag() {
    // The request states what the backend will use. A `getetag` in here would be a
    // plan to trust one, and the plan says a server ETag cannot be trusted: the
    // server this targets does not emit `getetag` in its WebDAV test suite at
    // all.
    let server = fake(corpus_files());
    run(corpus_source(&server).list()).expect("the fake serves the corpus");

    let body = server.requests()[0].body.clone().unwrap_or_default();
    assert!(body.contains("<d:resourcetype/>"), "{body}");
    assert!(body.contains("<d:getcontentlength/>"), "{body}");
    assert!(body.contains("<d:getlastmodified/>"), "{body}");
    assert!(!body.contains("getetag"), "{body}");
    assert!(!body.contains("allprop"), "{body}");
    assert_eq!(body, DAV_PROPFIND_BODY);
}

#[test]
fn walks_depth_first_so_a_grandchild_collection_is_listed_before_its_uncle() {
    // Breadth-first would answer `B/` before `A/A2/`. The order of the requests
    // IS the order of the walk, so this is the only place it is observable.
    let server = fake([
        VaultFile {
            path: "A/A2/Deep.md".into(),
            content: "# deep\n".into(),
        },
        VaultFile {
            path: "A/Alpha.md".into(),
            content: "# alpha\n".into(),
        },
        VaultFile {
            path: "B/Beta.md".into(),
            content: "# beta\n".into(),
        },
        VaultFile {
            path: "C.md".into(),
            content: "# c\n".into(),
        },
    ]);
    run(corpus_source(&server).list()).expect("the fake serves the tree");

    let urls: Vec<String> = server.requests().iter().map(|r| r.url.clone()).collect();
    assert_eq!(
        urls,
        [
            format!("{BASE_URL}/"),
            format!("{BASE_URL}/A/"),
            format!("{BASE_URL}/A/A2/"),
            format!("{BASE_URL}/B/"),
        ]
    );
}

#[test]
fn asks_for_a_listing_not_a_file_per_note() {
    // The alternative -- PROPFIND nothing and GET every note -- is the same number
    // of round trips for a flat vault and far worse for a deep one, and it cannot
    // see a collection it has no permission to enter.
    let server = fake(corpus_files());
    let paths = run(corpus_source(&server).list()).expect("the fake serves the corpus");
    assert_eq!(paths.len(), CORPUS_SIZE);
    assert!(server.requests().iter().all(|r| r.method == "PROPFIND"));
}

#[test]
fn refuses_a_listing_that_names_a_collection_inside_itself() {
    // The alternative to noticing the cycle is walking it: the same URLs,
    // forever, with a tool call hanging and nothing to report. `children_of`
    // already drops a listing's self-entry, so a cycle needs a collection to be
    // its own DESCENDANT -- which is why this fake is three collections deep.
    let listings: std::collections::BTreeMap<&str, Vec<&str>> = [
        ("", vec!["/dav/vault/", "/dav/vault/Loop/"]),
        ("Loop", vec!["/dav/vault/Loop/", "/dav/vault/Loop/Inner/"]),
        (
            "Loop/Inner",
            vec!["/dav/vault/Loop/Inner/", "/dav/vault/Loop/"],
        ),
    ]
    .into_iter()
    .collect();

    let transport = move |request: DavRequest| {
        let listings = listings.clone();
        async move {
            let url =
                reqwest::Url::parse(&request.url).map_err(|e| BasesError::new(e.to_string()))?;
            let asked = url
                .path()
                .strip_prefix(BASE_PATH)
                .unwrap_or_default()
                .trim_matches('/')
                .to_string();
            let Some(hrefs) = listings.get(asked.as_str()) else {
                return Ok(DavResponse::new(404, "Not Found", Vec::new()));
            };
            let responses: String = hrefs
                .iter()
                .map(|href| {
                    format!(
                        "<D:response><D:href>{href}</D:href><D:propstat><D:prop>\
                         <D:resourcetype><D:collection/></D:resourcetype></D:prop>\
                         <D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>"
                    )
                })
                .collect();
            Ok(DavResponse::new(
                207,
                "Multi-Status",
                format!(
                    "<?xml version=\"1.0\"?><D:multistatus xmlns:D=\"DAV:\">{responses}\
                     </D:multistatus>"
                )
                .into_bytes(),
            ))
        }
    };
    let source = WebdavVaultSource::new(
        WebdavVaultOptions::new(BASE_URL).with_transport(Box::new(ClosureTransport(transport))),
    )
    .expect("the base URL is valid");

    let error = failure_of(source.list());
    assert!(error.message().contains("no finite tree"), "{error}");
    assert!(error.message().contains("Loop"), "{error}");
}

/// The deliberate divergence from the filesystem backend, which swallows a failed
/// `readdir` and indexes whatever it could still reach. Over HTTP there is no
/// directory to stat first, and the result of swallowing is a vault that answers
/// every query with zero rows and no reason. The filesystem half of the same
/// divergence is pinned in `tests/vault_equivalence.rs`.
#[test]
fn refuses_rather_than_indexing_a_smaller_vault_when_a_listing_is_refused() {
    let server = fake(corpus_files());
    server.refusing(
        "Projects",
        Status {
            status: 403,
            status_text: "Forbidden",
        },
    );
    let error = failure_of(corpus_source(&server).list());

    assert!(error.message().contains("PROPFIND"), "{error}");
    assert!(error.message().contains("403"), "{error}");
}

#[test]
fn names_the_collection_it_could_not_read_not_the_whole_vault() {
    let server = fake([VaultFile {
        path: "Projects/Note.md".into(),
        content: "# n\n".into(),
    }]);
    server.refusing(
        "Projects",
        Status {
            status: 403,
            status_text: "Forbidden",
        },
    );
    let error = failure_of(corpus_source(&server).list());
    assert!(error.message().contains("Projects"), "{error}");
}

/// The refusal still carries the status as a field, not only in the message, even
/// though it arrived through the trait's plain error type. `send` is the seam that
/// keeps the structure, and it is public for that reason.
#[test]
fn a_refused_listing_carries_its_status_structurally() {
    let server = fake(corpus_files());
    server.refusing(
        "Projects",
        Status {
            status: 403,
            status_text: "Forbidden",
        },
    );
    let source = corpus_source(&server);
    let error = run(source.send(
        DavOperation::List,
        WebdavMethod::Propfind,
        "Projects",
        DavRequestOptions {
            body: Some(DAV_PROPFIND_BODY.to_string()),
            depth: Some(Depth::One),
            collection: true,
            ..Default::default()
        },
    ))
    .expect_err("the fake refuses Projects");
    assert_eq!(error.status, Some(403));
    assert_eq!(error.operation, DavOperation::List);
    assert_eq!(error.method, WebdavMethod::Propfind);
}

// ---------------------------------------------------------------------------
// list()
// ---------------------------------------------------------------------------

#[test]
fn list_matches_the_filesystem_backend_over_the_testing_vault_path_for_path() {
    // The oracle. `test/vault` is what `obsidian base:query` answers from, and the
    // corpus loader only ever hands the fake what the filesystem source will show.
    let server = fake(corpus_files());
    let over_dav = run(corpus_source(&server).list()).expect("the fake serves the corpus");
    let over_fs = run(FsVaultSource::new(vault_dir())
        .expect("the oracle is a directory")
        .list())
    .expect("the oracle is readable");
    assert_eq!(over_dav, over_fs);
}

#[test]
fn filters_exactly_as_is_indexable_does_dot_directories_and_extensions_included() {
    // The same tree the equivalence suite pins for fs and the in-memory source,
    // because "the same" is only meaningful across all three.
    let tree = common::dotfile_tree();
    let mut expected: Vec<String> = tree
        .iter()
        .map(|f| f.path.clone())
        .filter(|p| is_indexable(p))
        .collect();
    expected.sort();
    assert_eq!(
        expected,
        [
            "Notes/Alpha.md",
            "Notes/Deep/Beta.md",
            "Notes/Deep/Notes.base",
            "Templates/template.md",
        ]
    );
    let server = fake(tree);
    assert_eq!(
        run(corpus_source(&server).list()).expect("the fake serves the tree"),
        expected
    );
}

#[test]
fn does_not_descend_into_a_dot_directory() {
    // The filesystem walk skips one; a backend that listed it would find
    // `.obsidian/workspace.md` in `list()` and index a file Obsidian does not.
    let server = fake([
        VaultFile {
            path: ".obsidian/workspace.md".into(),
            content: "# not a note\n".into(),
        },
        VaultFile {
            path: "Note.md".into(),
            content: "# note\n".into(),
        },
    ]);
    assert_eq!(
        run(corpus_source(&server).list()).expect("the fake serves the tree"),
        ["Note.md"]
    );
    let urls: Vec<String> = server.requests().iter().map(|r| r.url.clone()).collect();
    assert_eq!(urls, [format!("{BASE_URL}/")]);
}

#[test]
fn is_sorted_because_order_feeds_link_resolution_and_every_unsorted_view() {
    let server = fake(corpus_files());
    let mut paths = run(corpus_source(&server).list()).expect("the fake serves the corpus");
    let sorted = {
        let mut copy = paths.clone();
        copy.sort();
        copy
    };
    paths.dedup();
    assert_eq!(paths, sorted);
}

#[test]
fn is_a_snapshot_until_a_write_and_refresh_rereads_it() {
    let server = fake(corpus_files());
    let source = corpus_source(&server);
    let before = run(source.list()).expect("the fake serves the corpus");

    run(source.write_text("Brand New.md", "# new\n")).expect("the write is verified");
    let mut expected = before.clone();
    expected.push("Brand New.md".to_string());
    expected.sort();
    assert_eq!(
        run(source.list()).expect("a write invalidates the snapshot"),
        expected
    );

    source.refresh();
    assert_eq!(
        run(source.list()).expect("refresh drops the snapshot"),
        expected
    );
}

// ---------------------------------------------------------------------------
// read_text()
// ---------------------------------------------------------------------------

#[test]
fn serves_the_stored_bytes_encoding_a_path_rather_than_guessing_at_it() {
    let content = "---\ntitle: Café\n---\n\n# Café\n";
    let server = fake([VaultFile {
        path: "Café/50%.md".into(),
        content: content.into(),
    }]);
    let source = source_over(&server);

    assert_eq!(
        run(source.read_text("Café/50%.md")).expect("the note is there"),
        content
    );
    let requests = server.requests();
    assert_eq!(requests[0].url, format!("{BASE_URL}/Caf%C3%A9/50%25.md"));
    assert_eq!(requests[0].method, "GET");
    assert_eq!(requests[0].depth, None);
}

#[test]
fn a_notes_name_cannot_truncate_the_request_with_a_hash_or_a_question_mark() {
    let server = fake([VaultFile {
        path: "Q1 #2.md".into(),
        content: "# q\n".into(),
    }]);
    run(source_over(&server).read_text("Q1 #2.md")).expect("the note is there");
    assert_eq!(server.requests()[0].url, format!("{BASE_URL}/Q1%20%232.md"));
}

#[test]
fn caches_under_the_normalised_path_so_two_spellings_are_one_note() {
    let server = fake([VaultFile {
        path: "Root Ticket.md".into(),
        content: "# ticket\n".into(),
    }]);
    let source = source_over(&server);

    assert_eq!(
        run(source.read_text("Tickets/../Root Ticket.md")).expect("a note"),
        "# ticket\n"
    );
    assert_eq!(server.requests().len(), 1);
    assert_eq!(
        run(source.read_text("Root Ticket.md")).expect("a note"),
        "# ticket\n"
    );
    assert_eq!(server.requests().len(), 1);
}

#[test]
fn a_missing_note_is_a_structured_404() {
    let server = fake([]);
    let source = source_over(&server);
    let error = run(source.send(
        DavOperation::Read,
        WebdavMethod::Get,
        "Nope.md",
        Default::default(),
    ))
    .expect_err("there is no such note");
    assert_eq!(error.status, Some(404));
    // And through the trait, the message still names the status, so a caller that
    // never saw `WebdavError` can still tell a 404 from a transport failure.
    let error = failure_of(source.read_text("Nope.md"));
    assert!(error.message().contains("404"), "{error}");
}

// ---------------------------------------------------------------------------
// stat()
// ---------------------------------------------------------------------------

#[test]
fn reports_the_size_and_mtime_the_server_sent_at_the_servers_own_resolution() {
    let content = "---\ntitle: ö\n---\n";
    let server = fake([VaultFile {
        path: "Note.md".into(),
        content: content.into(),
    }]);
    let source = source_over(&server);

    let stat = run(source.stat("Note.md")).expect("the note is there");
    assert_eq!(stat.size, content.len() as u64);
    assert_eq!(stat.mtime.timestamp_millis(), SERVER_MTIME_MS);
    // The offset is the server's, not the local one: this is the field the two
    // backends cannot agree on, and it is reported as sent.
    assert_eq!(stat.mtime.offset().local_minus_utc(), 0);
}

#[test]
fn asks_for_the_resource_itself_not_its_neighbours() {
    // Depth 0: a `Depth: 1` stat of a file is harmless but a `Depth: 1` stat of a
    // collection would silently return the wrong resource.
    let server = fake([VaultFile {
        path: "Note.md".into(),
        content: "# n\n".into(),
    }]);
    run(source_over(&server).stat("Note.md")).expect("the note is there");
    assert_eq!(server.requests()[0].depth.as_deref(), Some("0"));
}

#[test]
fn reports_the_servers_mtime_verbatim_never_the_local_clock() {
    // mtime is the one field the two backends cannot agree on: fs reads the local
    // clock, a server reports its own. Massaging the value towards the local
    // clock would make them agree on an instant that never happened.
    let server = fake([VaultFile {
        path: "Note.md".into(),
        content: "# n\n".into(),
    }]);
    server.with_mtime("Tue, 01 Oct 2024 12:00:00 +0200");
    let stat = run(source_over(&server).stat("Note.md")).expect("the note is there");
    // The instant, as UTC -- which is what the original's `toISOString` printed.
    assert_eq!(
        stat.mtime
            .with_timezone(&chrono::Utc)
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        "2024-10-01T10:00:00.000Z"
    );
    // And the OFFSET, kept as the server sent it: normalising that to UTC would be
    // reporting a clock the server never claimed.
    assert_eq!(stat.mtime.offset().local_minus_utc(), 7_200);
    assert_eq!(stat.mtime.to_rfc3339(), "2024-10-01T12:00:00+02:00");
}

#[test]
fn refuses_a_collection_a_missing_size_and_a_missing_mtime_each_by_name() {
    // Each of the three is a server declining to say, and each is refused BY NAME
    // rather than filled in: a `FileStat` with a plausible zero or epoch in it is
    // a wrong answer that looks like data.
    let server = fake([VaultFile {
        path: "Projects/Note.md".into(),
        content: "# n\n".into(),
    }]);
    let with_collection = source_over(&server);
    let error = failure_of(with_collection.stat("Projects"));
    assert!(error.message().contains("reported a collection"), "{error}");

    let no_size = source_answering(DavResponse::new(
        207,
        "Multi-Status",
        NO_SIZE_PROPFIND.into(),
    ));
    let error = failure_of(no_size.stat("Note.md"));
    assert!(
        error.message().contains("did not report getcontentlength"),
        "{error}"
    );

    let no_mtime = source_answering(DavResponse::new(
        207,
        "Multi-Status",
        NO_MTIME_PROPFIND.into(),
    ));
    let error = failure_of(no_mtime.stat("Note.md"));
    assert!(
        error.message().contains("did not report getlastmodified"),
        "{error}"
    );
}

#[test]
fn refuses_a_listing_that_names_no_resource_at_all() {
    let source = source_answering(DavResponse::new(207, "Multi-Status", EMPTY_LISTING.into()));
    let error = failure_of(source.stat("Note.md"));
    assert!(error.message().contains("returned no resource"), "{error}");
}

#[test]
fn refuses_an_unparsable_mtime_rather_than_yielding_an_invalid_date() {
    let server = fake([VaultFile {
        path: "Note.md".into(),
        content: "# n\n".into(),
    }]);
    server.with_mtime("whenever");
    let error = failure_of(source_over(&server).stat("Note.md"));
    assert!(
        error.message().contains("not a date this client can read"),
        "{error}"
    );
}

// ---------------------------------------------------------------------------
// hash()
// ---------------------------------------------------------------------------

#[test]
fn is_the_shared_content_hash_so_a_hash_means_the_same_on_both_backends() {
    let content = "# Root ticket\n";
    let server = fake([VaultFile {
        path: "Root Ticket.md".into(),
        content: content.into(),
    }]);
    let source = source_over(&server);

    assert_eq!(
        run(source.hash("Root Ticket.md")).expect("the note is there"),
        content_hash(content)
    );
    assert_eq!(
        run(source.hash("Root Ticket.md"))
            .expect("the note is there")
            .len(),
        32
    );
}

#[test]
fn agrees_with_the_filesystem_backend_on_every_file_of_the_testing_vault() {
    let corpus = corpus_files();
    let server = fake(corpus.clone());
    let source = source_over(&server);
    let fs = FsVaultSource::new(vault_dir()).expect("the oracle is a directory");

    for file in &corpus {
        assert_eq!(
            run(source.hash(&file.path)).expect("a hash"),
            run(fs.hash(&file.path)).expect("a hash"),
            "{}",
            file.path
        );
    }
}

#[test]
fn reads_once_and_never_an_etag() {
    let server = fake([VaultFile {
        path: "Root Ticket.md".into(),
        content: "# Root ticket\n".into(),
    }]);
    let source = source_over(&server);

    run(source.hash("Root Ticket.md")).expect("a hash");
    run(source.hash("Root Ticket.md")).expect("a hash");
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].body, None);
}

#[test]
fn follows_the_content_so_a_changed_note_hashes_differently() {
    let server = fake([VaultFile {
        path: "Note.md".into(),
        content: "# one\n".into(),
    }]);
    let source = source_over(&server);
    let before = run(source.hash("Note.md")).expect("a hash");

    run(source.write_text("Note.md", "# two\n")).expect("the write is verified");
    assert_ne!(run(source.hash("Note.md")).expect("a hash"), before);
    assert_eq!(
        run(source.hash("Note.md")).expect("a hash"),
        content_hash("# two\n")
    );
}

// ---------------------------------------------------------------------------
// write_text()
// ---------------------------------------------------------------------------

#[test]
fn puts_the_bytes_then_reads_them_back_before_reporting_success() {
    let server = fake([]);
    let source = source_over(&server);
    run(source.write_text("Note.md", "# note\n")).expect("the write is verified");

    let verb_url: Vec<(String, String)> =
        server.requests().iter().map(Recorded::verb_url).collect();
    assert_eq!(
        verb_url,
        [
            ("PUT".to_string(), format!("{BASE_URL}/Note.md")),
            ("GET".to_string(), format!("{BASE_URL}/Note.md")),
        ]
    );
    assert_eq!(server.requests()[0].body.as_deref(), Some("# note\n"));
    assert_eq!(server.stored("Note.md").as_deref(), Some("# note\n"));
}

#[test]
fn refuses_a_write_the_server_accepted_and_did_not_perform() {
    // The one failure a client cannot see in the response: `201 Created` for bytes
    // that are not there. This is why the read-back exists and why `hash()` is on
    // the interface at all.
    let data = "# requested\n";
    let swapped = "# swapped by the server\n";
    let transport = move |request: DavRequest| {
        let swapped = swapped.to_string();
        async move {
            Ok(match request.method {
                WebdavMethod::Put => DavResponse::new(201, "Created", Vec::new()),
                _ => DavResponse::new(200, "OK", swapped.into_bytes()),
            })
        }
    };
    let source = WebdavVaultSource::new(
        WebdavVaultOptions::new(BASE_URL).with_transport(Box::new(ClosureTransport(transport))),
    )
    .expect("the base URL is valid");

    let error = failure_of(source.write_text("Root Ticket.md", data));
    assert!(
        error
            .message()
            .contains("accepted but the bytes read back are not"),
        "{error}"
    );
    assert!(error.message().contains(&content_hash(data)), "{error}");
    assert!(error.message().contains(&content_hash(swapped)), "{error}");
    assert!(
        !error.message().contains(data),
        "the message must not echo the content: {error}"
    );
}

#[test]
fn a_refused_write_does_not_enter_the_caches_so_the_vault_is_not_left_lying() {
    let swapped = "# swapped by the server\n";
    let transport = |request: DavRequest| {
        let swapped = swapped.to_string();
        async move {
            Ok(match request.method {
                WebdavMethod::Put => DavResponse::new(201, "Created", Vec::new()),
                WebdavMethod::Propfind => {
                    DavResponse::new(207, "Multi-Status", EMPTY_LISTING.as_bytes().to_vec())
                }
                _ => DavResponse::new(200, "OK", swapped.into_bytes()),
            })
        }
    };
    let source = WebdavVaultSource::new(
        WebdavVaultOptions::new(BASE_URL).with_transport(Box::new(ClosureTransport(transport))),
    )
    .expect("the base URL is valid");

    let _ = failure_of(source.write_text("Note.md", "# requested\n"));
    assert_eq!(
        run(source.read_text("Note.md")).expect("what the server really holds"),
        swapped
    );
    assert_eq!(
        run(source.hash("Note.md")).expect("a hash"),
        content_hash(swapped)
    );
    assert!(run(source.list()).expect("a listing").is_empty());
}

#[test]
fn overwrites_silently_as_a_put_does_and_as_the_filesystem_backend_does() {
    let server = fake([VaultFile {
        path: "Note.md".into(),
        content: "first\n".into(),
    }]);
    let source = source_over(&server);

    run(source.write_text("Note.md", "second\n")).expect("the write is verified");
    assert_eq!(
        run(source.read_text("Note.md")).expect("a read"),
        "second\n"
    );
    assert_eq!(run(source.list()).expect("a listing"), ["Note.md"]);
}

#[test]
fn refuses_a_path_that_names_the_collection_rather_than_a_note() {
    // `PUT` and `DELETE` with no name address the collection, which is a request
    // with no honest reading. Refused before any request goes out, so a mistyped
    // path cannot put a body where a directory is.
    let server = fake([]);
    let source = source_over(&server);
    let write = failure_of(source.write_text("", "# n\n"));
    assert!(
        write.message().contains("collection, not a file"),
        "{write}"
    );
    let remove = failure_of(source.delete(""));
    assert!(
        remove.message().contains("collection, not a file"),
        "{remove}"
    );
    assert!(server.requests().is_empty());
}

#[test]
fn a_refused_put_surfaces_the_servers_own_status() {
    let server = fake([]);
    server.refusing(
        "Note.md",
        Status {
            status: 507,
            status_text: "Insufficient Storage",
        },
    );
    let source = source_over(&server);
    let error = run(source.send(
        DavOperation::Write,
        WebdavMethod::Put,
        "Note.md",
        DavRequestOptions {
            body: Some("# n\n".to_string()),
            content_type: Some("text/plain; charset=utf-8"),
            ..Default::default()
        },
    ))
    .expect_err("the fake refuses the PUT");
    assert_eq!(error.status, Some(507));
    assert!(error.error.message().contains("507"));
}

#[test]
fn creates_a_note_in_a_collection_that_does_not_exist_yet_and_says_so() {
    // A PUT into a missing collection is a 409, so the note is not written -- and
    // the failure is reported rather than swallowed into an empty vault.
    let server = fake([]);
    let error = failure_of(source_over(&server).write_text("Sandbox/Deep/N.md", "# n\n"));
    assert!(error.message().contains("409"), "{error}");
    assert_eq!(server.stored("Sandbox/Deep/N.md"), None);
}

// ---------------------------------------------------------------------------
// ensure_dir()
// ---------------------------------------------------------------------------

#[test]
fn creates_every_level_because_mkcol_has_no_recursive_form() {
    let server = fake([]);
    run(source_over(&server).ensure_dir("Sandbox/Deep/Nested")).expect("MKCOL 405 is tolerated");

    let verb_url: Vec<(String, String)> =
        server.requests().iter().map(Recorded::verb_url).collect();
    assert_eq!(
        verb_url,
        [
            ("MKCOL".to_string(), format!("{BASE_URL}/Sandbox")),
            ("MKCOL".to_string(), format!("{BASE_URL}/Sandbox/Deep")),
            (
                "MKCOL".to_string(),
                format!("{BASE_URL}/Sandbox/Deep/Nested")
            ),
        ]
    );
    assert!(server.has_dir("Sandbox/Deep/Nested"));
}

#[test]
fn a_collection_that_is_already_there_is_a_success_not_a_failure() {
    // MKCOL answers 405 for that, and `createNote` re-asks for a collection that
    // already exists on every note it writes into it.
    let server = fake([]);
    let source = source_over(&server);
    run(source.ensure_dir("Sandbox/Deep")).expect("the first MKCOL creates it");
    run(source.ensure_dir("Sandbox/Deep")).expect("the second is tolerated");
    assert!(server.requests().iter().all(|r| r.method == "MKCOL"));
}

#[test]
fn a_root_level_path_creates_no_collection_as_create_note_expects() {
    // `createNote` only calls `ensureDir` for a path with a separator. Slicing
    // unconditionally would give a root note the "parent" `Note.m`.
    let server = fake([]);
    run(source_over(&server).ensure_dir("")).expect("nothing to create");
    assert!(server.requests().is_empty());
}

#[test]
fn any_other_refusal_still_fails_so_a_409_is_not_mistaken_for_success() {
    let server = fake([]);
    server.refusing(
        "Sandbox/Deep",
        Status {
            status: 403,
            status_text: "Forbidden",
        },
    );
    let source = source_over(&server);
    let error = run(source.send(
        DavOperation::EnsureDir,
        WebdavMethod::Mkcol,
        "Sandbox/Deep",
        Default::default(),
    ))
    .expect_err("the fake refuses Sandbox/Deep");
    assert_eq!(error.status, Some(403));
}

// ---------------------------------------------------------------------------
// delete()
// ---------------------------------------------------------------------------

#[test]
fn deletes_the_file_and_forgets_it() {
    let server = fake([VaultFile {
        path: "Note.md".into(),
        content: "# n\n".into(),
    }]);
    let source = source_over(&server);
    assert_eq!(run(source.list()).expect("a listing"), ["Note.md"]);

    run(source.delete("Note.md")).expect("the DELETE succeeds");
    assert_eq!(
        server.requests().last().map(|r| r.method.as_str()),
        Some("DELETE")
    );
    assert_eq!(server.stored("Note.md"), None);
    assert!(run(source.list()).expect("a listing").is_empty());
}

#[test]
fn deleting_nothing_succeeds_because_the_desired_state_already_holds() {
    // The deliberate divergence from the filesystem backend's `rm --force`, in
    // wording rather than in outcome: the server says `404` and this source treats
    // that as the state the caller asked for. The filesystem half is in
    // `tests/vault_equivalence.rs`.
    let server = fake([]);
    let source = source_over(&server);
    run(source.delete("Never Existed.md")).expect("a 404 on a delete is tolerated");
    assert_eq!(server.requests()[0].method, "DELETE");
}

#[test]
fn a_refusal_is_still_a_refusal() {
    let server = fake([VaultFile {
        path: "Note.md".into(),
        content: "# n\n".into(),
    }]);
    server.refusing(
        "Note.md",
        Status {
            status: 403,
            status_text: "Forbidden",
        },
    );
    let source = source_over(&server);
    let error = run(source.send(
        DavOperation::Delete,
        WebdavMethod::Delete,
        "Note.md",
        Default::default(),
    ))
    .expect_err("the fake refuses the DELETE");
    assert_eq!(error.status, Some(403));
}

// ---------------------------------------------------------------------------
// Credentials
// ---------------------------------------------------------------------------

fn expected_basic() -> String {
    use base64::Engine;
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{USER}:{PASSWORD}"))
    )
}

#[test]
fn sends_http_basic_auth_on_every_request() {
    let server = fake([VaultFile {
        path: "Note.md".into(),
        content: "# n\n".into(),
    }]);
    let source = source_over(&server);
    run(source.list()).expect("a listing");
    run(source.read_text("Note.md")).expect("a read");

    let requests = server.requests();
    assert!(requests.len() > 1);
    for request in &requests {
        assert_eq!(
            request.authorization.as_deref(),
            Some(expected_basic().as_str())
        );
    }
}

#[test]
fn never_appears_in_an_error_message_whatever_failed() {
    // A credential in a tool result is a credential in an agent's transcript, and
    // from there in a log somewhere. Every refusal path is checked, not just one.
    let forbidden = Status {
        status: 403,
        status_text: "Forbidden",
    };
    let server = fake([VaultFile {
        path: "Note.md".into(),
        content: "# n\n".into(),
    }]);
    server.refusing("", forbidden);
    server.refusing("Note.md", forbidden);
    server.refusing("Projects", forbidden);
    let source = source_over(&server);

    // Every operation, each one refused, so no path through the source gets to
    // keep a credential because only the other six were checked.
    let failures = vec![
        failure_of(source.list()),
        failure_of(source.read_text("Note.md")),
        failure_of(source.stat("Note.md")),
        failure_of(source.hash("Note.md")),
        failure_of(source.write_text("Note.md", "# n\n")),
        failure_of(source.ensure_dir("Projects")),
        failure_of(source.delete("Note.md")),
    ];

    assert_eq!(failures.len(), 7);
    for error in &failures {
        assert!(!error.message().contains(PASSWORD), "{error}");
        assert!(!error.message().contains(USER), "{error}");
        assert!(error.message().contains("WebDAV"), "{error}");
    }
}

#[test]
fn never_appears_in_a_url_either() {
    let server = fake([VaultFile {
        path: "Note.md".into(),
        content: "# n\n".into(),
    }]);
    run(source_over(&server).read_text("Note.md")).expect("a read");
    for request in server.requests() {
        assert!(!request.url.contains(PASSWORD), "{}", request.url);
        assert!(!request.url.contains(USER), "{}", request.url);
        assert_eq!(request.url, format!("{BASE_URL}/Note.md"));
    }
}

#[test]
fn a_url_carrying_credentials_is_refused_rather_than_honoured() {
    // Honouring it would put the password inside the base URL, and this module
    // builds messages from strings. Keeping the credential in one place is what
    // makes "never log it" a property of the code rather than of care.
    let error = WebdavVaultSource::new(
        WebdavVaultOptions::new("https://agent:secret@dav.example/dav/vault").with_transport(
            Box::new(ClosureTransport(|_r| async {
                Ok(DavResponse::new(200, "OK", Vec::new()))
            })),
        ),
    )
    .expect_err("a URL with userinfo is refused");
    assert!(error.message().contains("carries credentials"), "{error}");
}

#[test]
fn a_username_without_a_password_is_a_misconfiguration_not_anonymous_access() {
    let transport = || {
        Box::new(ClosureTransport(|_r: DavRequest| async {
            Ok(DavResponse::new(200, "OK", Vec::new()))
        })) as Box<dyn WebdavTransport>
    };
    let only_user = WebdavVaultSource::new(
        WebdavVaultOptions::new(BASE_URL)
            .with_user(USER)
            .with_transport(transport()),
    );
    assert!(only_user.is_err(), "a username alone is refused");

    let only_password = WebdavVaultSource::new(
        WebdavVaultOptions::new(BASE_URL)
            .with_password(PASSWORD)
            .with_transport(transport()),
    );
    assert!(only_password.is_err(), "a password alone is refused");

    assert!(
        WebdavVaultSource::new(WebdavVaultOptions::new(BASE_URL).with_transport(transport()))
            .is_ok(),
        "neither is anonymous access"
    );
}

#[test]
fn an_unauthenticated_source_sends_no_authorization_header_at_all() {
    let server = fake([VaultFile {
        path: "Note.md".into(),
        content: "# n\n".into(),
    }]);
    let source = source_over_with(&server, None, None, None);
    run(source.read_text("Note.md")).expect("a read");
    assert_eq!(server.requests()[0].authorization, None);
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

#[test]
fn refuses_a_url_that_is_not_http_or_https() {
    // A `file:` URL would resolve against the local disk through a backend that
    // promises to be a network one, which is exactly the substitution the
    // entrypoint refuses to make silently.
    let error = WebdavVaultSource::new(WebdavVaultOptions::new("file:///tmp/vault"))
        .expect_err("a file URL is refused");
    assert!(error.message().contains("must be http"), "{error}");
}

#[test]
fn tolerates_a_trailing_slash_on_the_base_url_which_changes_nothing() {
    let server = fake([VaultFile {
        path: "Note.md".into(),
        content: "# n\n".into(),
    }]);
    let source = WebdavVaultSource::new(
        WebdavVaultOptions::new(format!("{BASE_URL}/"))
            .with_transport(Box::new(SharedTransport(Rc::clone(&server)))),
    )
    .expect("a trailing slash is tolerated");
    run(source.read_text("Note.md")).expect("a read");
    assert_eq!(server.requests()[0].url, format!("{BASE_URL}/Note.md"));
    assert_eq!(source.base_url(), BASE_URL);
    // The base PATH keeps the trailing slash it was configured with, while the base
    // URL does not. Faithful to the original, and harmless: href stripping drops
    // empty segments, so `/dav/vault/` and `/dav/vault` compare the same.
    assert_eq!(source.base_path(), "/dav/vault/");
}

#[test]
fn abandons_a_request_that_outlives_its_budget_and_says_so() {
    // A wedged PROPFIND would otherwise hang a tool call forever, with nothing to
    // report and no way to tell the agent to retry.
    let server = fake([]);
    server.wedged();
    let source = source_over_with(&server, None, None, Some(20));
    let error = failure_of(source.list());
    assert!(error.message().contains("got no response"), "{error}");
    // No status, structurally: nothing answered, and inventing one would make a
    // network problem look like a `503` worth retrying.
    let refusal = run(source.send(
        DavOperation::List,
        WebdavMethod::Propfind,
        "",
        DavRequestOptions {
            depth: Some(Depth::One),
            collection: true,
            ..Default::default()
        },
    ))
    .expect_err("nothing answers");
    assert_eq!(refusal.status, None);
    assert_eq!(refusal.operation, DavOperation::List);
}

/// The refusal type is what a caller gets when it wants the status rather than
/// the prose, and it converts into the trait's error so no method has to choose
/// between the two.
#[test]
fn a_webdav_error_converts_into_the_trait_error_and_keeps_its_fields() {
    let refusal = dav_refusal(
        DavOperation::Read,
        WebdavMethod::Get,
        "Note.md",
        404,
        "Not Found",
    )
    .expect("a 404 on a read is refused");
    let as_webdav: &WebdavError = &refusal;
    assert_eq!(as_webdav.status, Some(404));
    let as_bases: BasesError = refusal.into();
    assert_eq!(as_bases.construct(), Some("webdav"));
    assert!(as_bases.message().contains("404"));
}

// ---------------------------------------------------------------------------
// exists: one status means absent
// ---------------------------------------------------------------------------

/// The one question the backend is allowed to answer from a refusal.
///
/// `exists` guards a clobber, so `false` is not a neutral answer -- it is
/// permission to overwrite. Every status that is not the server stating "there is
/// nothing there" must therefore surface as an error, or a connectivity problem
/// becomes silent data loss.
#[test]
fn only_a_404_means_a_note_is_absent() {
    let server = fake([VaultFile {
        path: "Tickets/Kept.md".to_string(),
        content: "# kept".to_string(),
    }]);
    let source = source_over(&server);

    assert!(
        run(source.exists("Tickets/Kept.md")).expect("the server answers"),
        "a note the server holds must be reported present"
    );
    assert!(
        !run(source.exists("Tickets/Gone.md")).expect("the server answers"),
        "a 404 is the one status that means absent"
    );

    for status in [401, 403, 409, 500, 502, 503] {
        server.refusing(
            "Tickets/Kept.md",
            Status {
                status,
                status_text: "No",
            },
        );
        let error = failure_of(source.exists("Tickets/Kept.md"));
        assert!(
            error.message().contains(&status.to_string()),
            "{status} must be named in the refusal: {error}"
        );
        assert_eq!(
            error.construct(),
            Some("webdav"),
            "{status} must read as a backend refusal, not as an absent note"
        );
    }
}

/// A server that cannot be reached is not a server with nothing there.
#[test]
fn an_unreachable_server_is_an_error_rather_than_an_absent_note() {
    let server = fake([]);
    server.wedged();
    let source = source_over_with(&server, None, None, Some(20));

    let error = failure_of(source.exists("Tickets/Gone.md"));
    assert!(
        error.message().contains("got no response"),
        "a transport failure must not read as an absent note: {error}"
    );
}

/// The request is the one `stat` already makes, so no server gains a requirement.
#[test]
fn asks_the_server_with_a_depth_zero_propfind_and_never_the_listing() {
    let server = fake([VaultFile {
        path: "Tickets/Kept.md".to_string(),
        content: "# kept".to_string(),
    }]);
    let source = source_over(&server);

    run(source.exists("Tickets/Kept.md")).expect("the server answers");

    let requests = server.requests();
    assert_eq!(
        requests.len(),
        1,
        "existence is one round trip: {requests:?}"
    );
    assert_eq!(
        requests[0].verb_depth(),
        ("PROPFIND".to_string(), Some("0".to_string()))
    );
    assert_eq!(
        requests[0].url,
        format!("{BASE_URL}/Tickets/Kept.md"),
        "the path must be addressed as a file, not as a collection"
    );
    assert_eq!(
        requests[0].authorization.as_deref(),
        Some(format!("Basic {}", base64_of(format!("{USER}:{PASSWORD}"))).as_str()),
        "existence is authenticated like every other request"
    );
}

/// The filesystem backend answers `exists` from the filesystem, not its snapshot.
///
/// `FsVaultSource::list` is a snapshot that only a write or an explicit
/// `refresh` moves, so a cached answer here would be the very bug this method
/// exists to remove: the index cannot see a note a human saved in Obsidian.
#[test]
fn the_filesystem_backend_asks_the_filesystem_and_not_its_listing() {
    let dir = tempfile::TempDir::new().expect("a temp dir is writable");
    let source = FsVaultSource::new(dir.path()).expect("a directory is a vault root");

    // The listing is taken against an empty vault and cached, which is the state
    // every long-lived server process is in: it indexed the vault at startup and
    // has not listed it since.
    assert!(
        run(source.list()).expect("the vault lists").is_empty(),
        "the snapshot predates the note"
    );
    std::fs::write(dir.path().join("Later.md"), "# saved by a human").expect("a note is written");
    assert_eq!(
        run(source.list()).expect("the vault lists"),
        Vec::<String>::new(),
        "the snapshot is what `list` cached, so it still cannot see the note"
    );

    assert!(
        run(source.exists("Later.md")).expect("the filesystem answers"),
        "existence must not be answered from the listing"
    );
    assert!(
        !run(source.exists("Never.md")).expect("the filesystem answers"),
        "a path the filesystem does not hold is absent"
    );
}

/// The base64 the tests compare an `Authorization` header against.
fn base64_of(plain: String) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(plain)
}
