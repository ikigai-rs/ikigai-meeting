//! The module recipe as one test: `ikigai-conformance` walks the two entries
//! [`ikigai_meeting::space`] binds — one description, `urn:meeting:zoom:schedule`,
//! reachable as itself and as the `urn:meeting:schedule` facade — and reports
//! every violation at once.
//!
//! ## The fixture kernel: a loopback Zoom, and a secret reader that counts
//!
//! The one action is a Sink that creates a meeting at api.zoom.us with a token
//! from zoom.us, and the suite FIRES it (ENFORCED under no grants, the Turtle
//! face under root, PIPELINE under root). An ungated walk would create real
//! meetings, so [`Stub`] is an HTTP/1.1 listener on `127.0.0.1` at an ephemeral
//! port speaking the two routes this module speaks (`POST /oauth/token`,
//! `POST /v2/users/me/meetings`), recording every request — the `Authorization`
//! header included — and counting every connection it accepts: the count is how a
//! test proves a socket was NEVER opened. [`Secrets`] answers the three credential
//! names and counts reads, so a test can prove the keystore was never asked.
//! [`Client`] is the smallest [`HttpTransport`] that speaks to the stub, injected
//! the way a host injects `ureq`. Both Zoom bases point at the stub.
//!
//! ## Declarations, and why each
//!
//! - Nothing is `pure` and nothing is `cacheable`: a schedule is an effect, and
//!   the suite's cache probe never runs on a Sink anyway. That it is live is
//!   pinned by hand ([`a_schedule_is_never_cached`], conformance PENDING #22).
//! - `NAMES` is dropped suite-wide, not opted out per id: the id
//!   `urn:meeting:zoom:schedule` is a live MCP tool name, renamed in one
//!   coordinated pass (wave two, `ikigai-core-PENDING.md` §1). `Suite::opt_out`
//!   cannot carry this (it drops the invoking checks and leaves NAMES running), so
//!   [`names_are_wave_two`] pins the one finding the pass will flip.
//! - `space(transport, secrets, config)` is declared HOST-named
//!   (`Suite::host_named_space`, SPACE-NAME): it is instance-built, so its doors
//!   answer to whatever the host handed it, and it carries no name of its own.
//! - No opt-outs: every firing lands on the stub. No module namespace: the Turtle
//!   face uses `ical:`, `dcterms:` and `schema:`, plus the one `ik:` term the shared
//!   vocabulary defines for it, `ik:passcode` (`ikigai-vocab` 0.1.69). The walk is
//!   clean — [`conforms`] asserts exactly that, with no term carved out.
//!
//! ## What the suite cannot see, pinned by hand
//!
//! - **Denied before any secret is read or any socket opens**
//!   ([`denied_before_any_secret_is_read_or_any_socket_opens`]): the suite's
//!   ENFORCED sees a typed `Denied` under no grants — the KERNEL's floor on the
//!   declared `urn:cap:net:*` / `urn:cap:secret:read:*` (PENDING #46). Three more
//!   shapes reach the module's own rules: a net grant on another host, a secret
//!   grant on another name, and each family alone. Under every one the stub
//!   accepts no connection and the reader is never asked.
//! - **A schedule is live** ([`a_schedule_is_never_cached`]): two identical Sinks
//!   are two meetings.
//! - **Declared outputs against what is served** (PENDING #11/#31/#79,
//!   [`declared_outputs_are_the_media_types_served`]): every `as=` face served
//!   is declared, every declared face is served, and an undeclared `as=` is a
//!   typed `InvalidArgument` before any socket.
//! - **No credential and no request text in an error** (PENDING #67,
//!   [`errors_carry_no_credential_and_no_request_text`]): the module's denial, a
//!   transport failure that echoes the request it could not send (whose form
//!   body carries the account id), a
//!   server 4xx whose JSON body echoes the request beside its `message`, a
//!   server 4xx whose text body echoes it, and each invalid input.
//! - **`content` reaches the wire as the agenda**
//!   ([`content_is_the_agenda_when_piped`]): PIPELINE can only say `content` did
//!   not raise `MissingArgument`; the stub says the bytes arrived, and that a
//!   named `agenda` wins.
//! - **The manifold states the contract** ([`the_manifold_states_the_contract`]):
//!   one Sink action, exactly the two capability families, every input classed,
//!   only `topic` and `start` required (no check can see "required but actually
//!   optional", PENDING #5/#49), and the entry's `name()` (`meeting`) is not the
//!   description id — a fixture keyed on the name would be silently inert
//!   (PENDING #57).

use async_trait::async_trait;
use ikigai_conformance::{Check, Checks, Suite};
use ikigai_core::{ArgRef, Capability, Error, Expiry, Iri, Kernel, Representation, Request, Verb};
use ikigai_http::{HttpRequest, HttpResponse, HttpTransport};
use ikigai_meeting::{SecretReader, ZoomConfig, CAP_NET, CAP_SECRET_READ, FACES};
use oxrdf::{Literal, NamedNode, Term};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use url::Url;

/// The one description id, and the two entries that reach it.
const ID: &str = "urn:meeting:zoom:schedule";
const ENTRIES: [&str; 2] = ["urn:meeting:zoom:schedule", "urn:meeting:schedule"];

/// The credentials the reader answers, the token the stub mints, and request text
/// none of which may appear in an error.
const ACCOUNT_ID: &str = "acct-SECRET-4f2a91";
const CLIENT_ID: &str = "cid-SECRET-9b7c33";
const CLIENT_SECRET: &str = "csec-SECRET-e1d0aa";
const TOKEN: &str = "tok-SECRET-77af02";
const TOPIC: &str = "Call with attendee@example.com";
const AGENDA: &str = "PRIVATE-AGENDA-c3b1";

const SECRET_NAMES: [&str; 3] = ["zoom-account-id", "zoom-client-id", "zoom-client-secret"];

/// The scope that admits the stub, and one that admits a host it is not.
const STUB_SCOPE: &str = "urn:cap:net:127.0.0.1";
const OTHER_SCOPE: &str = "urn:cap:net:example.com";

const TOKEN_PATH: &str = "/oauth/token";
const MEETINGS_PATH: &str = "/v2/users/me/meetings";
const JOIN_URL: &str = "https://zoom.us/j/88899900011?pwd=abc";

/// One request as the stub received it.
#[derive(Clone, Debug)]
struct Received {
    /// The request target as it crossed the wire: the path and any query.
    path: String,
    authorization: String,
    content_type: String,
    body: Vec<u8>,
}

/// One HTTP/1.x message off a stream: the start line, the headers, and a body
/// of `Content-Length` bytes (or to EOF).
struct Message {
    start: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

fn read_message(stream: &mut TcpStream) -> Option<Message> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.lines();
    let start = lines.next()?.to_string();
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();
    let len: usize = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    let mut body = buf[head_end..].to_vec();
    while body.len() < len {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(len);
    Some(Message {
        start,
        headers,
        body,
    })
}

/// A loopback Zoom on an ephemeral port, recording what it receives and counting
/// what it accepts.
struct Stub {
    addr: SocketAddr,
    connections: Arc<AtomicUsize>,
    received: Arc<Mutex<Vec<Received>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Stub {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind a loopback port");
        let addr = listener.local_addr().unwrap();
        let connections = Arc::new(AtomicUsize::new(0));
        let received = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (connections, received, stop) =
                (connections.clone(), received.clone(), stop.clone());
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { continue };
                    if stop.load(Ordering::SeqCst) {
                        break;
                    }
                    connections.fetch_add(1, Ordering::SeqCst);
                    let Some(Message {
                        start,
                        headers,
                        body,
                    }) = read_message(&mut stream)
                    else {
                        continue;
                    };
                    let mut parts = start.split_whitespace();
                    let method = parts.next().unwrap_or("").to_string();
                    let path = parts.next().unwrap_or("/").to_string();
                    let header = |name: &str| {
                        headers
                            .iter()
                            .find(|(k, _)| k.eq_ignore_ascii_case(name))
                            .map(|(_, v)| v.clone())
                            .unwrap_or_default()
                    };
                    let authorization = header("authorization");
                    let content_type = header("content-type");
                    let (status, reason, response_type, body_out) =
                        respond(&method, &path, &authorization, &body);
                    received.lock().unwrap().push(Received {
                        path,
                        authorization,
                        content_type,
                        body,
                    });
                    let head = format!(
                        "HTTP/1.1 {status} {reason}\r\nConnection: close\r\n\
                         Content-Type: {response_type}\r\nContent-Length: {}\r\n\r\n",
                        body_out.len()
                    );
                    let _ = stream.write_all(head.as_bytes());
                    let _ = stream.write_all(&body_out);
                    let _ = stream.flush();
                }
            })
        };
        Stub {
            addr,
            connections,
            received,
            stop,
            thread: Some(thread),
        }
    }

    /// A base URL on this stub, by the address the capability is granted on.
    fn base(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.addr.port())
    }

    fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }

    fn received(&self) -> Vec<Received> {
        self.received.lock().unwrap().clone()
    }

    /// How many requests reached `path` (the query, if any, excluded).
    fn hits(&self, path: &str) -> usize {
        self.received()
            .iter()
            .filter(|r| r.path.split('?').next() == Some(path))
            .count()
    }
}

impl Drop for Stub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Unblock the accept loop so the thread sees the flag.
        let _ = TcpStream::connect(self.addr);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The stub's script. Under `/v2` the create-meeting route answers as Zoom does,
/// echoing the request's `start_time`; under `/echo/v2` a 400 whose JSON body
/// carries the request back (the `Authorization` header and the body) beside its
/// `message`; under `/text/v2` a 400 whose plain-text body does the same;
/// anything else a 404.
fn respond(
    method: &str,
    path: &str,
    authorization: &str,
    request: &[u8],
) -> (u16, &'static str, &'static str, Vec<u8>) {
    let json =
        |status, reason, body: String| (status, reason, "application/json", body.into_bytes());
    let path = path.split('?').next().unwrap_or(path);
    match (method, path) {
        ("POST", TOKEN_PATH) => json(
            200,
            "OK",
            format!(r#"{{"access_token":"{TOKEN}","token_type":"bearer","expires_in":3600}}"#),
        ),
        ("POST", MEETINGS_PATH) => {
            let start = serde_json::from_slice::<serde_json::Value>(request)
                .ok()
                .and_then(|v| v["start_time"].as_str().map(str::to_string))
                .unwrap_or_else(|| "2026-01-01T00:00:00Z".to_string());
            json(
                200,
                "OK",
                format!(
                    r#"{{"id":88899900011,"join_url":"{JOIN_URL}","start_url":"https://zoom.us/s/88899900011?zak=HOST-SECRET","password":"a1b2c3","start_time":"{start}"}}"#
                ),
            )
        }
        ("POST", "/echo/v2/users/me/meetings") => json(
            400,
            "Bad Request",
            format!(
                r#"{{"code":300,"message":"bad request","echo":{{"authorization":{},"body":{}}}}}"#,
                serde_json::Value::String(authorization.to_string()),
                serde_json::Value::String(String::from_utf8_lossy(request).into_owned())
            ),
        ),
        ("POST", "/text/v2/users/me/meetings") => (
            400,
            "Bad Request",
            "text/plain",
            format!(
                "refused: {authorization} {}",
                String::from_utf8_lossy(request)
            )
            .into_bytes(),
        ),
        _ => json(
            404,
            "Not Found",
            r#"{"code":404,"message":"no such path"}"#.to_string(),
        ),
    }
}

/// The smallest transport that speaks to the stub: one blocking HTTP/1.1
/// exchange per request.
struct Client;

#[async_trait]
impl HttpTransport for Client {
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse, String> {
        let url = Url::parse(&request.url).map_err(|e| e.to_string())?;
        let host = url.host_str().ok_or("no host")?;
        let port = url.port_or_known_default().ok_or("no port")?;
        let mut target = url.path().to_string();
        if let Some(q) = url.query() {
            target.push('?');
            target.push_str(q);
        }
        let mut stream = TcpStream::connect((host, port)).map_err(|e| e.to_string())?;
        let mut head = format!(
            "{} {target} HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\nContent-Length: {}\r\n",
            request.method.as_str(),
            request.body.len()
        );
        for (k, v) in &request.headers {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        head.push_str("\r\n");
        stream
            .write_all(head.as_bytes())
            .and_then(|()| stream.write_all(&request.body))
            .map_err(|e| e.to_string())?;
        let Message {
            start,
            headers,
            body,
        } = read_message(&mut stream).ok_or("no response")?;
        let status = start
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| format!("bad status line `{start}`"))?;
        Ok(HttpResponse {
            status,
            headers,
            body,
        })
    }
}

/// A transport that fails the way a careless client does: naming the URL it
/// could not reach and echoing the body it could not send — which, for the
/// token request, carries the account id.
struct Failing;

#[async_trait]
impl HttpTransport for Failing {
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse, String> {
        Err(format!(
            "{} ({}): connection refused",
            request.url,
            String::from_utf8_lossy(&request.body)
        ))
    }
}

/// The keystore as the host injects it: answers the three credential names and
/// counts every read.
#[derive(Default)]
struct Secrets {
    reads: AtomicUsize,
}

impl Secrets {
    fn reads(&self) -> usize {
        self.reads.load(Ordering::SeqCst)
    }
}

impl SecretReader for Secrets {
    fn read(&self, name: &str) -> ikigai_core::Result<Vec<u8>> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        let value = match name {
            "zoom-account-id" => ACCOUNT_ID,
            "zoom-client-id" => CLIENT_ID,
            "zoom-client-secret" => CLIENT_SECRET,
            other => return Err(Error::Endpoint(format!("no secret `{other}`"))),
        };
        Ok(value.as_bytes().to_vec())
    }
}

/// Both Zoom bases on the stub; `api` is the API prefix (`/v2`, or an error route).
fn config(stub: &Stub, api: &str) -> ZoomConfig {
    ZoomConfig {
        oauth_base: stub.base(""),
        api_base: stub.base(api),
        ..ZoomConfig::default()
    }
}

fn kernel(stub: &Stub, secrets: Arc<Secrets>, api: &str) -> Kernel {
    Kernel::new(Arc::new(ikigai_meeting::space(
        Arc::new(Client),
        secrets,
        config(stub, api),
    )))
}

fn request(iri: &str, args: &[(&str, &str)]) -> Request {
    let mut request = Request::new(Verb::Sink, Iri::parse(iri).unwrap());
    for (name, value) in args {
        request = request.with_arg(*name, ArgRef::Inline(value.as_bytes().to_vec()));
    }
    request
}

fn issue(
    kernel: &Kernel,
    iri: &str,
    args: &[(&str, &str)],
    capability: &Capability,
) -> Result<Representation, Error> {
    futures::executor::block_on(kernel.issue(request(iri, args), capability))
}

fn scoped(scopes: &[&str]) -> Capability {
    Capability::scoped(scopes.iter().map(|s| s.to_string()))
}

/// The three per-name secret grants.
fn secret_grants() -> Vec<&'static str> {
    vec![
        "urn:cap:secret:read:zoom-account-id",
        "urn:cap:secret:read:zoom-client-id",
        "urn:cap:secret:read:zoom-client-secret",
    ]
}

/// The whole grant: the stub's host and every credential name.
fn full_grant() -> Capability {
    let mut scopes = secret_grants();
    scopes.push(STUB_SCOPE);
    scoped(&scopes)
}

/// The smallest call the contract admits, with the text no error may carry.
fn minimal() -> Vec<(&'static str, &'static str)> {
    vec![
        ("topic", TOPIC),
        ("start", "2026-01-01T00:00:00Z"),
        ("agenda", AGENDA),
    ]
}

#[test]
fn conforms() {
    let stub = Stub::start();
    let secrets = Arc::new(Secrets::default());
    // `space(transport, secrets, config)` is instance-built: its doors answer to
    // whatever transport, keystore and Zoom bases the host handed it, so only the
    // host knows which instance it is. Declared host-named, by value: the same
    // space goes to the kernel and the suite.
    let space = Arc::new(ikigai_meeting::space(
        Arc::new(Client),
        secrets.clone(),
        config(&stub, "/v2"),
    ));
    let kernel = Kernel::new(space.clone());
    let report = Suite::new()
        .checks(Checks::all() - Checks::NAMES)
        .host_named_space("ikigai_meeting::space(transport, secrets, config)", space)
        .run_blocking(&kernel);
    // Printed even when clean (`--nocapture`): the report is the record.
    eprintln!("{report}");

    // Clean, with nothing carved out: `ik:passcode` — the Turtle face's one `ik:`
    // term, and the module's last VOCABULARY finding — is defined by the shared
    // vocabulary as of `ikigai-vocab` 0.1.69.
    assert!(report.is_clean(), "{report}");

    // One description, two entries, one Sink action each; only NAMES skipped;
    // nothing declared.
    assert_eq!(report.endpoints, 1, "{report}");
    assert_eq!(report.actions, ENTRIES.len(), "{report}");
    assert_eq!(
        report.checks.skipped().collect::<Vec<_>>(),
        vec![Check::Names],
        "only NAMES is skipped: {report}"
    );
    assert!(report.declared.opted_out.is_empty(), "{report}");
    assert!(report.declared.pure.is_empty(), "{report}");
    assert!(report.declared.cacheable.is_empty(), "{report}");
    assert!(report.declared.namespaces.is_empty(), "{report}");

    // The footprint: per entry, ENFORCED under no grants reaches nothing (the
    // kernel's floor), the Turtle face fires once and PIPELINE fires once — two
    // meetings, each a token exchange and a create, each three secret reads.
    let firings = 2 * ENTRIES.len();
    assert_eq!(stub.hits(TOKEN_PATH), firings, "one token per firing");
    assert_eq!(stub.hits(MEETINGS_PATH), firings, "one meeting per firing");
    assert_eq!(stub.connections(), 2 * firings);
    assert_eq!(secrets.reads(), 3 * firings, "three reads per firing");
    for r in stub.received() {
        let path = r.path.split('?').next().unwrap_or(&r.path);
        assert!(
            path == TOKEN_PATH || path == MEETINGS_PATH,
            "an unscripted route was requested: {}",
            r.path
        );
    }
}

/// The one finding the wave-two rename will flip: the id is an IRI, not a
/// kebab-case noun — and nothing else under that check.
#[test]
fn names_are_wave_two() {
    let stub = Stub::start();
    let kernel = kernel(&stub, Arc::new(Secrets::default()), "/v2");
    let report = Suite::new().checks(Checks::NAMES).run_blocking(&kernel);
    eprintln!("{report}");
    let flagged: Vec<&str> = report
        .of(Check::Names)
        .map(|f| f.endpoint.as_str())
        .collect();
    assert_eq!(flagged, [ID], "{report}");
    assert_eq!(report.findings.len(), 1, "{report}");
    assert_eq!(report.endpoints, 1, "{report}");
    assert_eq!(stub.connections(), 0, "NAMES is a static check");
}

/// Under every capability short of the whole grant, both entries refuse with a
/// typed, permanent `Denied` that names what refused it — and the stub accepts
/// no connection, the reader is never asked. Two gates are in play: with no grant
/// under a declared family the KERNEL's floor refuses before the endpoint runs;
/// with a grant under each family but on the wrong host or the wrong name the
/// module's own rule refuses. Under the whole grant a meeting is scheduled: three
/// reads, two connections.
#[test]
fn denied_before_any_secret_is_read_or_any_socket_opens() {
    let stub = Stub::start();
    let secrets = Arc::new(Secrets::default());
    let kernel = kernel(&stub, secrets.clone(), "/v2");
    let mut net_and_other_secret = vec![STUB_SCOPE, "urn:cap:secret:read:other"];
    net_and_other_secret.push("urn:cap:secret:read:zoom-client-id");
    let mut other_host_and_secrets = secret_grants();
    other_host_and_secrets.push(OTHER_SCOPE);
    let shapes: Vec<(Capability, &str)> = vec![
        // The kernel's floor, one family at a time.
        (scoped(&[]), CAP_NET),
        (scoped(&[STUB_SCOPE]), CAP_SECRET_READ),
        (scoped(&secret_grants()), CAP_NET),
        // The module's rules: a net grant on another host; a secret grant on
        // another name (the first unreadable name is the one refused).
        (scoped(&other_host_and_secrets), "127.0.0.1"),
        (scoped(&net_and_other_secret), "zoom-account-id"),
    ];
    for (capability, gate) in &shapes {
        for iri in ENTRIES {
            let err = issue(&kernel, iri, &minimal(), capability)
                .err()
                .unwrap_or_else(|| panic!("{iri} resolved under {capability:?}"));
            assert!(matches!(err, Error::Denied(_)), "{iri}: {err:?}");
            assert!(!err.is_transient(), "{iri}: {err:?}");
            assert!(
                err.to_string().contains(gate),
                "{iri} under {capability:?}: the denial names what refused it: {err}"
            );
        }
    }
    assert_eq!(
        stub.connections(),
        0,
        "the gates precede the socket: nothing connected"
    );
    assert_eq!(
        secrets.reads(),
        0,
        "the gates precede the keystore: nothing read"
    );

    let ok = issue(&kernel, ID, &minimal(), &full_grant()).unwrap();
    assert_eq!(ok.repr_type.media_type, "text/plain");
    assert_eq!(ok.bytes, JOIN_URL.as_bytes());
    assert_eq!(secrets.reads(), 3, "each credential read once");
    assert_eq!(stub.connections(), 2, "a token exchange and a create");
    assert_eq!(stub.hits(MEETINGS_PATH), 1);
}

/// A schedule is an effect: two identical Sinks are two meetings, the result is
/// live and names no thread, and the kernel holds nothing for the request. The
/// suite cannot see this (its cache probe never runs on a Sink); the stub can.
#[test]
fn a_schedule_is_never_cached() {
    let stub = Stub::start();
    let kernel = kernel(&stub, Arc::new(Secrets::default()), "/v2");
    let root = Capability::root();
    for _ in 0..2 {
        let repr = issue(&kernel, ID, &minimal(), &root).unwrap();
        assert_eq!(repr.expiry, Expiry::Always, "a schedule is live");
        assert!(repr.threads().is_empty(), "an effect names no thread");
    }
    assert_eq!(stub.hits(MEETINGS_PATH), 2, "two resolutions, two meetings");
    assert!(!kernel.is_cached(&request(ID, &minimal()), &root));
}

fn declared_outputs(kernel: &Kernel, iri: &str) -> Vec<String> {
    kernel
        .describe_pattern(iri)
        .unwrap_or_else(|| panic!("{iri} describes itself"))
        .outputs
        .iter()
        .map(|o| ikigai_conformance::rdf::bare_media_type(o))
        .collect()
}

/// What `ikigai-conformance` 0.1.0 does not check (its PENDING #11/#31/#79): a
/// declared output that is not an RDF face is never compared with what the action
/// serves, and an `as=`-selected face is invisible without one. Every `as=` the
/// contract admits, plus none: the media type served is declared, every declared
/// output is served by some call, the Turtle face is join-safe and the JSON face
/// is the trusted envelope. An `as=` outside the declared list is refused before
/// any socket.
#[test]
fn declared_outputs_are_the_media_types_served() {
    let stub = Stub::start();
    let kernel = kernel(&stub, Arc::new(Secrets::default()), "/v2");
    let root = Capability::root();
    let declared = declared_outputs(&kernel, ID);
    assert_eq!(declared, FACES, "{declared:?}");
    let mut served = Vec::new();
    for face in std::iter::once(None).chain(FACES.iter().map(Some)) {
        let mut args = minimal();
        if let Some(face) = face {
            args.push(("as", face));
        }
        let repr = issue(&kernel, ID, &args, &root).unwrap_or_else(|e| panic!("as={face:?}: {e}"));
        let got = ikigai_conformance::rdf::bare_media_type(&repr.repr_type.media_type);
        assert!(
            declared.contains(&got),
            "as={face:?} served `{got}`, declared only {declared:?}"
        );
        assert_eq!(
            got,
            face.copied().unwrap_or(FACES[0]),
            "as={face:?}: the face asked for"
        );
        let text = String::from_utf8_lossy(&repr.bytes).into_owned();
        match got.as_str() {
            "text/turtle" => {
                assert!(
                    text.contains(&format!("ical:conference <{JOIN_URL}>")),
                    "{text}"
                );
                assert!(!text.contains("zak="), "the host URL leaked: {text}");
            }
            "application/json" => {
                let v: serde_json::Value = serde_json::from_str(&text).unwrap();
                assert_eq!(v["join_url"], JOIN_URL);
                assert!(v["start_url"].as_str().unwrap().contains("zak="), "{text}");
            }
            _ => assert_eq!(text, JOIN_URL),
        }
        served.push(got);
    }
    for face in &declared {
        assert!(
            served.contains(face),
            "declares `{face}` but no call above served it"
        );
    }

    let before = stub.connections();
    let mut args = minimal();
    args.push(("as", "text/html"));
    let err = issue(&kernel, ID, &args, &root).unwrap_err();
    assert!(
        matches!(err, Error::InvalidArgument { ref name, .. } if name == "as"),
        "{err:?}"
    );
    assert_eq!(stub.connections(), before, "refused before any socket");
}

/// The errors this module composes never carry a credential or the request's
/// text. Each case is one way foreign text enters an error: the module's own
/// denial (names the host, never the URL); a transport that echoes the request
/// it could not send (the token request's form body carries the account id); a
/// server 4xx
/// whose JSON body echoes the request beside its `message` (only the message is
/// kept); a server 4xx whose plain-text body echoes it (dropped, its size
/// stated); and an invalid input (named, not echoed). An error travels through
/// traces, logs and MCP replies.
#[test]
fn errors_carry_no_credential_and_no_request_text() {
    let stub = Stub::start();
    let secrets = Arc::new(Secrets::default());
    let root = Capability::root();
    let echo = kernel(&stub, secrets.clone(), "/echo/v2");
    let text = kernel(&stub, secrets.clone(), "/text/v2");
    let failing = Kernel::new(Arc::new(ikigai_meeting::space(
        Arc::new(Failing),
        secrets.clone(),
        ZoomConfig::default(),
    )));
    let mut other_host = secret_grants();
    other_host.push(OTHER_SCOPE);
    let bad = |name: &'static str, value: &'static str| {
        let mut args = minimal();
        args.retain(|(n, _)| *n != name);
        args.push((name, value));
        args
    };
    let errors: Vec<(&str, Error)> = vec![
        (
            "denied on the host",
            issue(&echo, ID, &minimal(), &scoped(&other_host)).unwrap_err(),
        ),
        (
            "transport failure naming the URL",
            issue(&failing, ID, &minimal(), &root).unwrap_err(),
        ),
        (
            "server 4xx echoing the request in JSON",
            issue(&echo, ID, &minimal(), &root).unwrap_err(),
        ),
        (
            "server 4xx echoing the request in text",
            issue(&text, ID, &minimal(), &root).unwrap_err(),
        ),
        (
            "bad start",
            issue(&echo, ID, &bad("start", "next tuesday at noon"), &root).unwrap_err(),
        ),
        (
            "bad duration",
            issue(&echo, ID, &bad("duration", "ninety minutes"), &root).unwrap_err(),
        ),
    ];
    let basic = base64(&format!("{CLIENT_ID}:{CLIENT_SECRET}"));
    let never: [&str; 9] = [
        ACCOUNT_ID,
        CLIENT_ID,
        CLIENT_SECRET,
        TOKEN,
        &basic,
        TOPIC,
        "attendee@example.com",
        AGENDA,
        "next tuesday",
    ];
    for (case, err) in &errors {
        let rendered = format!("{err} / {err:?}");
        for secret in never {
            assert!(
                !rendered.contains(secret),
                "{case}: the error carries `{secret}`: {rendered}"
            );
        }
    }
    // The shapes each error keeps.
    assert!(matches!(errors[0].1, Error::Denied(_)), "{:?}", errors[0]);
    assert!(
        errors[0].1.to_string().contains("127.0.0.1"),
        "{}",
        errors[0].1
    );
    assert!(
        errors[1].1.to_string().contains("transport error")
            && errors[1].1.to_string().contains("[redacted]"),
        "the account id the transport echoed is redacted: {}",
        errors[1].1
    );
    assert!(
        errors[2].1.to_string().contains("400: bad request")
            && !errors[2].1.to_string().contains("echo"),
        "only the server's message is kept: {}",
        errors[2].1
    );
    assert!(
        errors[3].1.to_string().contains("400") && errors[3].1.to_string().contains("not JSON"),
        "a text body is dropped: {}",
        errors[3].1
    );
    assert!(
        matches!(errors[4].1, Error::InvalidArgument { ref name, .. } if name == "start"),
        "{:?}",
        errors[4]
    );
    assert!(
        matches!(errors[5].1, Error::InvalidArgument { ref name, .. } if name == "duration"),
        "{:?}",
        errors[5]
    );
    // The echo routes really did receive the token and the request text — the
    // scrub is what kept them out.
    let echoed: Vec<Received> = stub
        .received()
        .into_iter()
        .filter(|r| r.path.starts_with("/echo/") || r.path.starts_with("/text/"))
        .collect();
    assert_eq!(echoed.len(), 2);
    for r in &echoed {
        assert_eq!(r.authorization, format!("Bearer {TOKEN}"));
        let body = String::from_utf8_lossy(&r.body);
        assert!(body.contains(TOPIC) && body.contains(AGENDA), "{body}");
    }
}

/// The account id is a credential (ledger #171): it travels in the token
/// request's FORM BODY, never in its request target, so a host transport that
/// logs URLs sees no credential. Read off the wire by the stub: the target is the
/// bare token path, the body is `application/x-www-form-urlencoded` carrying the
/// grant type and the account id, and the client credentials ride only in the
/// Basic header.
#[test]
fn the_account_id_travels_in_the_form_body_never_the_url() {
    let stub = Stub::start();
    let kernel = kernel(&stub, Arc::new(Secrets::default()), "/v2");
    issue(&kernel, ID, &minimal(), &full_grant()).unwrap();
    let token: Vec<Received> = stub
        .received()
        .into_iter()
        .filter(|r| r.path.split('?').next() == Some(TOKEN_PATH))
        .collect();
    assert_eq!(token.len(), 1, "one token exchange");
    let r = &token[0];
    assert_eq!(r.path, TOKEN_PATH, "the request target carries no query");
    for credential in [ACCOUNT_ID, CLIENT_ID, CLIENT_SECRET] {
        assert!(
            !r.path.contains(credential),
            "a credential is in the URL: {}",
            r.path
        );
    }
    assert_eq!(r.content_type, "application/x-www-form-urlencoded");
    let form: Vec<(String, String)> = url::form_urlencoded::parse(&r.body).into_owned().collect();
    assert_eq!(
        form,
        [
            ("grant_type".to_string(), "account_credentials".to_string()),
            ("account_id".to_string(), ACCOUNT_ID.to_string()),
        ],
        "the form body"
    );
    assert_eq!(
        r.authorization,
        format!("Basic {}", base64(&format!("{CLIENT_ID}:{CLIENT_SECRET}")))
    );
    let body = String::from_utf8_lossy(&r.body);
    assert!(
        !body.contains(CLIENT_ID) && !body.contains(CLIENT_SECRET),
        "the client credentials ride only in the Basic header: {body}"
    );
}

/// The Turtle and JSON faces of one meeting state the same values as the same
/// RDF TERMS (ledger #170), not merely the same text: every field the shareable
/// graph carries is compared, as a parsed term, with the term its JSON value
/// denotes under the predicate's range. `ik:passcode`'s range is read from the
/// shared vocabulary itself, so a range change there turns this red instead of
/// leaving the faces silently unequal (the `ik:batchAt` shape in ikigai-llm: a
/// bare Turtle integer against `xsd:positiveInteger`). Topics with a quote, a
/// backslash and line breaks are the escaping the Turtle face must survive.
#[test]
fn the_turtle_and_json_faces_are_term_equal() {
    const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";
    const XSD_DATETIME: &str = "http://www.w3.org/2001/XMLSchema#dateTime";
    const ICAL: &str = "http://www.w3.org/2002/12/cal/ical#";
    let passcode = format!("{}passcode", ikigai_vocab::NS);
    let vocabulary =
        ikigai_conformance::rdf::parse("text/turtle", ikigai_vocab::VOCABULARY.as_bytes())
            .expect("the shared vocabulary parses");
    let passcode_range = vocabulary
        .iter()
        .find(|t| {
            t.subject.to_string() == format!("<{passcode}>")
                && t.predicate.as_str() == "http://www.w3.org/2000/01/rdf-schema#range"
        })
        .map(|t| match &t.object {
            Term::NamedNode(range) => range.as_str().to_string(),
            other => panic!("ik:passcode's range is not an IRI: {other}"),
        })
        .expect("the vocabulary states ik:passcode's range");

    let stub = Stub::start();
    let kernel = kernel(&stub, Arc::new(Secrets::default()), "/v2");
    let root = Capability::root();
    for topic in [
        "Intro call",
        r#"Say "hi" \ then go"#,
        "line one\nline two\r\tend",
    ] {
        let face = |face: &str| {
            let args = [
                ("topic", topic),
                ("start", "2026-01-01T00:00:00Z"),
                ("as", face),
            ];
            issue(&kernel, ID, &args, &root)
                .unwrap_or_else(|e| panic!("{topic:?} as={face}: {e}"))
                .bytes
        };
        let turtle = face("text/turtle");
        let json: serde_json::Value = serde_json::from_slice(&face("application/json")).unwrap();
        assert_eq!(json["topic"], topic, "the JSON face carries the topic sent");
        let triples = ikigai_conformance::rdf::parse("text/turtle", &turtle).unwrap_or_else(|e| {
            panic!(
                "{topic:?}: the Turtle face does not parse: {e}\n{}",
                String::from_utf8_lossy(&turtle)
            )
        });
        let field = |name: &str| {
            json[name]
                .as_str()
                .unwrap_or_else(|| panic!("the JSON face has no string `{name}`: {json}"))
        };
        let subject = NamedNode::new(format!("urn:meeting:zoom:{}", field("id"))).unwrap();
        let objects = |predicate: &str| -> Vec<Term> {
            triples
                .iter()
                .filter(|t| {
                    t.subject == subject.clone().into() && t.predicate.as_str() == predicate
                })
                .map(|t| t.object.clone())
                .collect()
        };
        let typed = |name: &str, datatype: &str| {
            vec![Term::from(Literal::new_typed_literal(
                field(name),
                NamedNode::new(datatype).unwrap(),
            ))]
        };
        let iri = |name: &str| vec![Term::from(NamedNode::new(field(name)).unwrap())];
        for (predicate, want) in [
            (
                "http://purl.org/dc/terms/identifier".to_string(),
                typed("id", XSD_STRING),
            ),
            (
                "https://schema.org/provider".to_string(),
                typed("provider", XSD_STRING),
            ),
            (format!("{ICAL}summary"), typed("topic", XSD_STRING)),
            (format!("{ICAL}conference"), iri("join_url")),
            (passcode.clone(), typed("passcode", &passcode_range)),
            (format!("{ICAL}dtstart"), typed("start_time", XSD_DATETIME)),
        ] {
            assert_eq!(objects(&predicate), want, "{topic:?}: <{predicate}>");
        }
    }
}

/// PIPELINE can only say `content` did not raise `MissingArgument`; the stub can
/// say the bytes arrived as the meeting's agenda, and that a named `agenda` wins
/// over a piped one.
#[test]
fn content_is_the_agenda_when_piped() {
    let stub = Stub::start();
    let kernel = kernel(&stub, Arc::new(Secrets::default()), "/v2");
    let root = Capability::root();
    let base = [("topic", "t"), ("start", "2026-01-01T00:00:00Z")];
    let agenda_sent = |args: &[(&str, &str)]| {
        issue(&kernel, ID, args, &root).unwrap();
        let last = stub.received().pop().unwrap();
        let body: serde_json::Value = serde_json::from_slice(&last.body).unwrap();
        body["agenda"].as_str().map(str::to_string)
    };
    let mut piped = base.to_vec();
    piped.push(("content", "piped agenda"));
    assert_eq!(agenda_sent(&piped).as_deref(), Some("piped agenda"));
    let mut both = piped.clone();
    both.push(("agenda", "named agenda"));
    assert_eq!(
        agenda_sent(&both).as_deref(),
        Some("named agenda"),
        "a named agenda wins"
    );
    assert_eq!(agenda_sent(&base), None, "no agenda, no field");
}

/// The contract as the manifold states it: one Sink action per entry, both
/// entries one description; exactly the two capability families; every input
/// classed; only `topic` and `start` required; `content` declared and optional;
/// `as` enumerates the faces with the join URL as its default. And the entry's
/// `name()` is not the id.
#[test]
fn the_manifold_states_the_contract() {
    let stub = Stub::start();
    let kernel = kernel(&stub, Arc::new(Secrets::default()), "/v2");
    for iri in ENTRIES {
        let description = kernel.describe_pattern(iri).unwrap();
        assert_eq!(description.id, ID, "{iri}: one description");
        let specs = description.action_specs();
        assert_eq!(specs.len(), 1, "{iri}: one action");
        let spec = &specs[0];
        assert_eq!(spec.verb, Verb::Sink, "{iri}");
        assert_eq!(
            spec.requires,
            [CAP_NET, CAP_SECRET_READ],
            "{iri}: declared = enforced"
        );
        for input in &spec.inputs {
            assert!(
                input.class.as_deref().is_some_and(|c| c.contains(':')),
                "{iri}: input `{}` has a class",
                input.name
            );
        }
        let required: Vec<&str> = spec
            .inputs
            .iter()
            .filter(|i| i.required)
            .map(|i| i.name.as_str())
            .collect();
        assert_eq!(required, ["topic", "start"], "{iri}: the required inputs");
        let content = spec.inputs.iter().find(|i| i.name == "content");
        assert!(
            content.is_some_and(|c| !c.required),
            "{iri}: `content` is optional"
        );
        let face = spec.inputs.iter().find(|i| i.name == "as").unwrap();
        assert_eq!(face.one_of, FACES, "{iri}");
        assert_eq!(face.default.as_deref(), Some(FACES[0]), "{iri}");
        assert_eq!(declared_outputs(&kernel, iri), FACES, "{iri}");
    }
    let entries = kernel.entries().expect("enumerable");
    let ours: Vec<_> = entries
        .iter()
        .filter(|e| ENTRIES.contains(&e.pattern.as_str()))
        .collect();
    assert_eq!(ours.len(), ENTRIES.len());
    for entry in ours {
        // `name()` predates the id and differs from it: a conformance fixture keyed
        // on `meeting` would match nothing (PENDING #57).
        assert_eq!(entry.endpoint, "meeting", "{}", entry.pattern);
        assert_ne!(entry.endpoint, ID);
    }
    assert_eq!(SECRET_NAMES.len(), secret_grants().len());
}

/// Standard base64, for the Basic header the error test must not find.
fn base64(input: &str) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = input.as_bytes();
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        out.push(A[(n >> 18 & 63) as usize] as char);
        out.push(A[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            A[(n >> 6 & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            A[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}
