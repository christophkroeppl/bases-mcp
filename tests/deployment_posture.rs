//! The deployment posture: the four claims the operator relies on, pinned.
//!
//! # What this suite is for
//!
//! The trust model of the HTTP transport is "the network is safe", and that
//! model rests on four claims. Three of them are about the CODE, and one is
//! about a FILE the operator reads. All four are load-bearing, and none of
//! them was pinned by a test before this file existed:
//!
//! 1. **Unset binds loopback.** A deployment that says nothing binds
//!    `127.0.0.1`, so a misconfiguration does not listen on every interface.
//! 2. **The container widens it deliberately.** `compose.yaml` sets
//!    `MCP_HTTP_HOST: "0.0.0.0"` because the loopback OF THE CONTAINER is
//!    unreachable from outside it. That is the operator's explicit choice,
//!    and it is deliberate — not a default somebody has to undo.
//! 3. **The WebDAV credential stays in the WebDAV backend.** The transport's
//!    trust model and the storage credential are separate, and a credential
//!    that leaks into a diagnostic is a credential in a log.
//! 4. **The container runs as non-root uid 1000.** A containment control, not
//!    an access control — and the uid is PINNED, because guarden reads the
//!    image's user out of the image and renders `Userns=keep-id:uid=…` from
//!    it.
//!
//! # Why a test file at all
//!
//! Claims 1 and 3 are properties of the code and are asserted against the
//! real types, not against a copy of the rule. Claims 2 and 4 are properties
//! of FILES, and a file that drifts is the failure this project has already
//! made more than once: a compose file that stops setting `0.0.0.0` leaves a
//! container whose listener nothing can reach, and a `Containerfile` that
//! stops pinning the uid gives guarden a different number to pin on every
//! base image it rebuilds.
//!
//! A claim in prose decays silently. A claim in a test fails loudly.

use std::path::{Path, PathBuf};

use bases_mcp::config::{ConfigResult, Env, VaultConfig, parse_config};
use bases_mcp::transport::HttpEndpoint;
use bases_mcp::vault::webdav::{DavRequest, WebdavMethod};

/// The repository root, from the manifest directory.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

/// A file in the repository, read as text.
fn read(relative: &str) -> String {
    let path = repo_root().join(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!(
            "{} is unreadable: {error}\n\
             The deployment posture is asserted against this file, so an unreadable one \
             cannot be a passing check.",
            path.display()
        )
    })
}

// ---------------------------------------------------------------------------
// 1. Unset binds loopback
// ---------------------------------------------------------------------------

/// The default is loopback, and it is the default whatever the shell says.
///
/// The property is asserted over the VALUES rather than over the environment,
/// because the environment is not the default: it carries whatever
/// `MCP_HTTP_HOST` the shell happened to export. A test that read the
/// environment would assert the ambient value while claiming to assert the
/// default — and would fail on a developer machine that exports the variable,
/// which is a false red about the code.
#[test]
fn an_unset_bind_address_is_loopback() {
    let endpoint = HttpEndpoint::from_values(None, None).expect("absent is loopback");
    assert_eq!(
        endpoint.address(),
        "127.0.0.1:8765".parse().unwrap(),
        "the default binds the loopback of this host"
    );
    assert!(
        endpoint.address().ip().is_loopback(),
        "the default is a loopback address: {}",
        endpoint.address()
    );
    assert!(
        !endpoint.address().ip().is_unspecified(),
        "the default is never a wildcard bind: {}",
        endpoint.address()
    );
}

/// A blank bind address is the default, not a name.
///
/// A blank value trimmed to nothing is what an env file with an empty line
/// leaves behind. Carried as a NAME it would be resolved by the operating
/// system, and a resolver that answers an empty name with the wildcard
/// address would turn a blank line into a listener on every interface — the
/// exact failure the loopback default exists to prevent.
#[test]
fn a_blank_bind_address_is_the_default_and_not_a_name() {
    for blank in ["", " ", "   ", "\t"] {
        let endpoint = HttpEndpoint::from_values(Some(blank.to_string()), None)
            .expect("a blank bind address is an absent one");
        assert_eq!(
            endpoint.address(),
            "127.0.0.1:8765".parse().unwrap(),
            "a blank bind address ({blank:?}) binds loopback"
        );
    }
}

/// The default is what the DOCUMENTATION says it is.
///
/// The docs are where the operator learns what the server does when it says
/// nothing, and a doc that drifts from the code is worse than no doc: the
/// operator sets `MCP_HTTP_HOST=0.0.0.0` for a container because the doc
/// says the default is loopback, and the doc stops being true.
#[test]
fn the_documented_default_is_the_loopback_default() {
    let tooling = read("docs/tooling.md");
    assert!(
        tooling.contains("MCP_HTTP_HOST    bind address (default 127.0.0.1)"),
        "docs/tooling.md states the default bind address"
    );
    let example = read(".env.example");
    assert!(
        example.contains("MCP_HTTP_HOST="),
        ".env.example names the bind address variable"
    );
}

// ---------------------------------------------------------------------------
// 2. The container widens it deliberately
// ---------------------------------------------------------------------------

/// `compose.yaml` binds every interface, on purpose, and says why.
///
/// The container's own loopback is unreachable from outside the container, so
/// a container that kept the default would have a listener nothing could
/// dial. The override is the operator's explicit choice — but a choice that
/// is only visible in a YAML file's `environment:` block, with no comment, is
/// a choice the next reader cannot distinguish from a mistake.
#[test]
fn the_compose_binds_every_interface_and_says_why() {
    let compose = read("compose.yaml");
    assert!(
        compose.contains("MCP_HTTP_HOST: \"0.0.0.0\""),
        "the container widens the bind to every interface"
    );
    // The WHY is the part that decays: a comment removed in a reformat leaves
    // a widening that reads like a mistake, and the next person "fixes" it
    // back to loopback and the container stops answering.
    assert!(
        compose.contains("must be `0.0.0.0` inside a container"),
        "the compose says WHY the container widens the bind"
    );
    assert!(
        compose.contains("loopback OF THE CONTAINER"),
        "the compose names the failure a loopback bind would have inside a container"
    );
}

/// The guarden manifest says the same thing the compose does.
///
/// The manifest is the unit source, and it carries the security note an
/// operator reads when they deploy the service rather than the compose.
#[test]
fn the_guarden_manifest_states_the_trust_model() {
    let manifest = read(".guarden/service.yml");
    assert!(
        manifest.contains("unauthenticated"),
        "the guarden manifest says the listener is unauthenticated"
    );
    assert!(
        manifest.contains("network is assumed safe"),
        "the guarden manifest states the assumption the deployment rests on"
    );
    assert!(
        manifest.contains("docs/divergences.md"),
        "the guarden manifest points at where the gap is recorded"
    );
}

/// The startup warning fires when every interface is bound.
///
/// The operator who widens the bind has to be told, at the moment they widen
/// it, that the listener has no authentication. This is the one place the
/// server itself announces it.
#[test]
fn the_server_announces_a_widened_bind_at_startup() {
    let source = read("src/main.rs");
    assert!(
        source.contains("every interface is bound"),
        "the startup line says every interface is bound"
    );
    assert!(
        source.contains("has no authentication"),
        "the startup line says the listener has no authentication"
    );
    assert!(
        source.contains("safe only on a network you trust"),
        "the startup line says what the binding is safe on"
    );
}

// ---------------------------------------------------------------------------
// 3. The WebDAV credential stays in the WebDAV backend
// ---------------------------------------------------------------------------

/// A credential never reaches a diagnostic.
///
/// The WebDAV backend's own rule is that "a credential never appears in a
/// string" — but that rule was enforced only over the STRINGS the backend
/// builds. The DERIVED `Debug` was a hole in it: formatting a request or a
/// configuration printed the credential in cleartext, and a `Debug` is what
/// every log line, every assertion message and every panic reaches for.
///
/// The base64 is not a protection: `Basic dXNlcjpzZWNyZXQ=` decodes to
/// `user:secret` in one step, so a credential in a `Debug` is a credential in
/// every log that ever formatted the type.
#[test]
fn a_webdav_configuration_never_prints_its_password() {
    let mut env = Env::new();
    env.insert(
        "BASES_MCP_WEBDAV_URL".to_string(),
        "https://dav.example/dav/vault".to_string(),
    );
    env.insert("BASES_MCP_WEBDAV_USER".to_string(), "agent".to_string());
    env.insert(
        "BASES_MCP_WEBDAV_PASSWORD".to_string(),
        "correct horse battery staple".to_string(),
    );

    let ConfigResult::Ok { config, .. } = parse_config(&env) else {
        panic!("a WebDAV configuration with both credentials is accepted");
    };

    let rendered = format!("{config:?}");
    assert!(
        !rendered.contains("correct horse battery staple"),
        "the password is not in the rendered configuration: {rendered}"
    );
    // The field is PRESENT and withheld, not absent: a missing field reads as
    // "there is no password", which is a different claim from "there is one
    // and it is not shown here".
    assert!(
        rendered.contains("password"),
        "the rendered configuration still names the password field: {rendered}"
    );
    assert!(
        rendered.contains("***"),
        "the withheld password is marked as withheld: {rendered}"
    );
    // The non-secret parts are still there, or the diagnostic says nothing.
    assert!(
        rendered.contains("agent"),
        "the username is not a secret and is still shown: {rendered}"
    );
    assert!(
        rendered.contains("dav.example"),
        "the URL is still shown: {rendered}"
    );
}

/// A request never prints its `Authorization` header.
///
/// Same hole, same shape: the derived `Debug` of a `DavRequest` printed
/// `headers: [("Authorization", "Basic …")]`.
#[test]
fn a_request_never_prints_its_authorization_header() {
    // The credential, base64-encoded the way the header carries it.
    let encoded = "YWdlbnQ6Y29ycmVjdCBob3JzZSBiYXR0ZXJ5IHN0YXBsZQ==";
    let request = DavRequest {
        url: "https://dav.example/dav/vault/Note.md".to_string(),
        method: WebdavMethod::Get,
        headers: vec![
            ("Authorization".to_string(), format!("Basic {encoded}")),
            ("Depth".to_string(), "1".to_string()),
        ],
        body: None,
        timeout: std::time::Duration::from_millis(30),
    };

    let rendered = format!("{request:?}");
    // The WHOLE encoded value, not a prefix of it: a prefix is a string the
    // author chose, and a chosen prefix can miss. The value as a whole is
    // what must be absent.
    assert!(
        !rendered.contains(encoded),
        "the Authorization value is not in the rendered request: {rendered}"
    );
    assert!(
        !rendered.contains("correct horse"),
        "the decoded credential is not in the rendered request: {rendered}"
    );
    // The header NAME is shown, and so are the headers that carry nothing
    // secret — a diagnostic that printed nothing useful would be a
    // diagnostic nobody reads.
    assert!(
        rendered.contains("Authorization"),
        "the rendered request still names the header: {rendered}"
    );
    assert!(
        rendered.contains("Depth"),
        "the rendered request still shows the headers that carry no secret: {rendered}"
    );
}

/// The WebDAV credential and the transport's trust model are separate things.
///
/// The transport answers whoever can open the socket; the WebDAV credential
/// authenticates THIS PROCESS to a storage endpoint. Conflating them is how
/// a storage credential ends up in an HTTP header the transport serves, or how
/// the transport's lack of authentication is mistaken for a property of the
/// vault. The two live in different modules and the documentation says so.
#[test]
fn the_two_trust_models_are_separate_and_documented_as_such() {
    // The credential lives in the vault layer.
    let config = read("src/config.rs");
    assert!(
        config.contains("BASES_MCP_WEBDAV_PASSWORD"),
        "the storage credential is read by the configuration layer"
    );
    assert!(
        config.contains("BASES_MCP_WEBDAV_URL"),
        "the storage endpoint is read by the configuration layer"
    );
    // The transport's own variables are NOT storage variables. The transport
    // may NAME them in its usage text — it has to, or the operator is not told
    // where the vault comes from — but it never READS one.
    let transport = read("src/transport.rs");
    assert!(
        !transport.contains("env::var(\"BASES_MCP_WEBDAV"),
        "the transport never reads a storage credential: it has no business with one"
    );
    assert!(
        !transport.contains("BASES_MCP_WEBDAV_USER"),
        "the transport never reads a storage username"
    );

    // And the two live in different layers: the credential is read where the
    // vault is configured, and nowhere else.
    let webdav = read("src/vault/webdav.rs");
    assert!(
        webdav.contains("BASES_MCP_WEBDAV_USER"),
        "the storage credential is the vault layer's business"
    );
}

/// The credential is refused in a URL, at both layers.
///
/// The URL is the shape a credential leaks through most easily, and the rule
/// is enforced twice on purpose: the configuration layer refuses it, and the
/// backend refuses it again, so neither can be reached with a password baked
/// into a URL by another caller.
#[test]
fn a_credential_in_a_url_is_refused_and_never_echoed() {
    let mut env = Env::new();
    env.insert(
        "BASES_MCP_WEBDAV_URL".to_string(),
        "http://sekret:alsosecret@localhost:5000/vault".to_string(),
    );
    env.insert("BASES_MCP_WEBDAV_USER".to_string(), "u".to_string());
    env.insert("BASES_MCP_WEBDAV_PASSWORD".to_string(), "p".to_string());

    let ConfigResult::Err { message } = parse_config(&env) else {
        panic!("a URL carrying credentials is refused");
    };
    assert!(
        !message.contains("alsosecret"),
        "the refusal does not echo the password: {message}"
    );
    assert!(
        !message.contains("sekret"),
        "the refusal does not echo the username: {message}"
    );
}

// ---------------------------------------------------------------------------
// 4. The container runs as non-root uid 1000
// ---------------------------------------------------------------------------

/// The `Containerfile` pins the uid, and the pin is visible.
///
/// The uid is pinned rather than left to `useradd --system`, which picks 999
/// on Debian and can pick something else on another base. Pinning is what
/// makes the vault writable — under the right user namespace — and it is what
/// lets guarden read the image's user out of the image and render
/// `Userns=keep-id:uid=…,gid=…` from it (ADR-0019 in the guarden
/// repository). A repository that leaves the uid to the base image gives the
/// host a different number to pin on every rebuild.
#[test]
fn the_containerfile_pins_the_uid_and_says_what_it_is_for() {
    let containerfile = read("Containerfile");
    assert!(
        containerfile.contains("--uid 1000"),
        "the container creates its user with the pinned uid 1000"
    );
    assert!(
        containerfile.contains("USER bases-mcp"),
        "the image runs AS that user rather than as root"
    );
    // The WHY is what decays. The pin without the reason reads like an
    // arbitrary number, and an arbitrary number is one a future edit "tidies
    // up".
    assert!(
        containerfile.contains("keep-id"),
        "the Containerfile names the user namespace the pin depends on"
    );

    // WHO reads the pinned uid is documented where the operator reads it:
    // guarden probes the image's user and renders `Userns=keep-id:uid=…` from
    // it (ADR-0019 in the guarden repository), so a repository that leaves
    // the uid to `useradd --system` gives the host a different number to pin
    // on every base image it rebuilds.
    let tooling = read("docs/tooling.md");
    assert!(
        tooling.contains("guarden"),
        "the documentation says who reads the pinned uid out of the image"
    );
    assert!(
        tooling.contains("Userns=keep-id"),
        "the documentation names what guarden renders from the pinned uid"
    );
}

/// The pinned uid is the one the documentation names.
///
/// Two files, one number. A drift between them is invisible until a base
/// image is rebuilt and the vault stops being writable — a failure that
/// presents as `Permission denied (os error 13)`, which names the file and
/// says nothing about the uid.
#[test]
fn the_documented_uid_is_the_pinned_uid() {
    let tooling = read("docs/tooling.md");
    assert!(
        tooling.contains("uid 1000"),
        "docs/tooling.md names the uid 1000"
    );
    assert!(
        tooling.contains("keep-id"),
        "docs/tooling.md explains the user namespace the uid depends on"
    );
    // And the docs say what the uid IS: a containment control, not an access
    // control. A non-root uid says nothing about who may call the tools.
    assert!(
        tooling.contains("no home and no shell"),
        "docs/tooling.md says what the image's user can reach"
    );
}

// ---------------------------------------------------------------------------
// The gap itself
// ---------------------------------------------------------------------------

/// The unauthenticated gap is RECORDED, not hidden.
///
/// This suite pins a deployment posture, and a posture that is not written
/// down is a posture nobody inherits. The Divergence registry is where a
/// deliberate difference from Obsidian is recorded — and D9 is where the
/// network-accessible, unauthenticated listener is recorded.
#[test]
fn the_unauthenticated_gap_is_recorded_in_the_divergence_registry() {
    let divergences = read("docs/divergences.md");
    assert!(
        divergences.contains("D9"),
        "the divergence registry carries the entry for this surface"
    );
    assert!(
        divergences.contains("no authentication"),
        "the entry says the listener has no authentication"
    );
    assert!(
        divergences.contains("network is safe"),
        "the entry names the assumption the trust model rests on"
    );
}

/// The `0.0.0.0` bind is recorded too, and it is a WIDENING.
///
/// The `Host` allow-list is derived from the bind address: a specific
/// address is served on that address, and `0.0.0.0` serves EVERY `Host`,
/// because the address a request arrived on is not knowable at bind time. A
/// widening that is not recorded looks like the narrower thing it replaced.
#[test]
fn the_widened_bind_is_recorded_as_a_widening() {
    let divergences = read("docs/divergences.md");
    assert!(
        divergences.contains("0.0.0.0"),
        "the entry records the wildcard bind"
    );
    assert!(
        divergences.contains("every") && divergences.contains("Host"),
        "the entry records that the wildcard bind accepts every Host header"
    );
}

/// The loopback default is what the divergence registry says it is.
///
/// The registry and the code have to agree, because the registry is what an
/// operator reads to decide whether the server is safe on their network.
#[test]
fn the_registry_and_the_code_agree_about_the_default() {
    let divergences = read("docs/divergences.md");
    assert!(
        divergences.contains("127.0.0.1` by default"),
        "the registry states the loopback default"
    );
    let endpoint = HttpEndpoint::from_values(None, None).expect("absent is loopback");
    assert_eq!(
        endpoint.address(),
        "127.0.0.1:8765".parse().unwrap(),
        "and the code does what the registry says"
    );
}

/// The compose says WHERE the boundary is, and that the uid is not it.
///
/// The compose is the unit source the deployment is rendered from, so it is
/// where an operator looks when they ask "is this service on the network?".
/// The answer is "no, because the port is published to loopback" — and that
/// answer has to be here, because a `0.0.0.0` bind on its own reads like the
/// opposite.
#[test]
fn the_compose_says_where_the_boundary_actually_is() {
    let compose = read("compose.yaml");
    assert!(
        compose.contains("THE BOUNDARY IS THE PUBLICATION"),
        "the compose says the boundary is the publication, not the bind"
    );
    assert!(
        compose.contains("no"),
        "the compose states what the listener lacks"
    );
    assert!(
        compose.contains("CONTAINMENT control"),
        "the compose says the non-root user is containment, not access control"
    );
    // And it points at the place the publication is actually decided.
    assert!(
        compose.contains(".guarden/service.yml"),
        "the compose points at where the publication is declared"
    );
}

/// The Containerfile says what the non-root user does and does not do.
///
/// The pin without the reason is decoration: an arbitrary number in a
/// Dockerfile is one a future edit tidies up, and the tidied number breaks
/// the guarden user-namespace mapping on the next base image rebuild.
#[test]
fn the_containerfile_says_what_the_non_root_user_is_for() {
    let containerfile = read("Containerfile");
    assert!(
        containerfile.contains("CONTAINMENT control, not an access control"),
        "the Containerfile says the non-root user is containment, not access control"
    );
    assert!(
        containerfile.contains("NOTHING about who may call the"),
        "the Containerfile says what the uid does not decide"
    );
    assert!(
        containerfile.contains("ADR-0019"),
        "the Containerfile names the ADR that depends on the pinned uid"
    );
}

/// The network boundary is stated where the operator reads it.
///
/// The ticket asks for the intended boundary to be documented "where the
/// operator reads it", rather than only in a backlog file outside the
/// repository. `docs/tooling.md` is the deployment instructions, so that is
/// where it belongs — and the words have to say the thing that is true, which
/// is that the server does not enforce a boundary, it assumes one.
#[test]
fn the_network_boundary_is_documented_where_the_operator_reads_it() {
    let tooling = read("docs/tooling.md");
    assert!(
        tooling.contains("no authentication, no authorization and no rate"),
        "the deployment docs name what the transport does not have"
    );
    assert!(
        tooling.contains("no authentication"),
        "the deployment docs say the listener has no authentication"
    );
    // The two writers, named: a reader who only sees "no authentication" may
    // not know that the surface can WRITE.
    assert!(
        tooling.contains("write_note") && tooling.contains("add_note_to_base"),
        "the deployment docs name the tools that write"
    );
    // The containment claim, which the ticket asks for explicitly: the
    // non-root uid bounds an escape, and says nothing about who may call.
    assert!(
        tooling.contains("containment control, not an access control"),
        "the deployment docs say the non-root user is containment, not access control"
    );
    assert!(
        tooling.contains("nothing about who may call the tools"),
        "the deployment docs say what the uid does not decide"
    );
}

/// The two trust models are told apart in the operator's own file.
///
/// The WebDAV credential authenticates this process to a storage endpoint. The
/// transport has no credential at all. An operator reading the container
/// recipe sees both in the same command block, and the natural mistake is to
/// assume the storage credential protects the transport.
#[test]
fn the_two_trust_models_are_told_apart_in_the_operator_docs() {
    let tooling = read("docs/tooling.md");
    assert!(
        tooling.contains("BASES_MCP_WEBDAV_PASSWORD"),
        "the deployment docs name the storage credential"
    );
    // And the credential is not the transport's credential.
    assert!(
        tooling.contains("never logged or echoed in an error"),
        "the deployment docs say the credential is never logged"
    );
}

/// The `.env.example` — the documented home for environment variables — says
/// which of its variables is the trust-relevant one.
#[test]
fn the_env_example_names_the_network_trust_model() {
    let example = read(".env.example");
    assert!(
        example.contains("no authentication"),
        ".env.example says the listener has no authentication"
    );
    assert!(
        example.contains("Bind 0.0.0.0 only on a network you trust"),
        ".env.example says what widening the bind requires"
    );
}

/// The configuration's own `Debug` never leaks, whatever it holds.
///
/// The filesystem variant carries a path, which is not a secret but is still
/// a diagnostic that should read like the code's other diagnostics.
#[test]
fn a_filesystem_configuration_prints_its_path_and_nothing_else() {
    let config = VaultConfig::Fs {
        dir: PathBuf::from("/tmp/vault"),
    };
    let rendered = format!("{config:?}");
    assert!(
        rendered.contains("/tmp/vault"),
        "the path is shown: {rendered}"
    );
    assert!(
        !rendered.contains("password"),
        "a filesystem configuration has no credential to withhold: {rendered}"
    );
}
