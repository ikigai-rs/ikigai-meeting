//! `ikigai-meeting` — video-conference scheduling as ikigai ROC resources.
//!
//! `urn:meeting:schedule` (a provider-agnostic facade) dispatches to pluggable backends
//! `urn:meeting:<provider>:schedule`, the same shape as `urn:llm:ask`. Slice 0 ships a **Zoom**
//! backend over Server-to-Server OAuth. The scheduled meeting's join URL is emitted as
//! `ical:conference` (RFC 7986's CONFERENCE property), so ONE value flows into the `.ics`, the
//! calendar event, and — later — an org entry, without re-plumbing at each destination.
//!
//! ## Injection, not dependencies
//!
//! Like ikigai-llm, this crate bakes in no HTTP client and no keystore. The host injects:
//! - an [`HttpTransport`] (ureq/reqwest natively, fetch in the browser), and
//! - a [`SecretReader`] — the host resolves `urn:secret:<name>` under a capability and hands the
//!   bytes in, so the Zoom credentials stay in the OS keystore and this crate never links it.
//!
//! ## Authority: declared = enforced, and before anything happens
//!
//! The Sink declares [`CAP_NET`] and [`CAP_SECRET_READ`], so the kernel refuses a caller holding
//! no grant under either prefix before the endpoint runs. The finer rules are the module's:
//! both Zoom hosts must be granted (`urn:cap:net:zoom.us`, `urn:cap:net:api.zoom.us` — port-aware,
//! [`ikigai_http::net_allows_port`]) and every credential's NAME must be readable
//! (`urn:cap:secret:read:zoom-client-secret`, …). Both run first: a refusal is a typed
//! [`Error::Denied`] issued before any secret is read and before any socket opens, and the
//! injected [`SecretReader`] — which has no capability of its own — is never asked.
//!
//! ## What an error may say
//!
//! An error travels through traces, logs and MCP replies, so nothing a credential or a caller
//! put into the request comes back in one. A server's 4xx contributes only its `message` /
//! `reason` / `error` field (a body that echoes the request is dropped); a transport's text is
//! scrubbed of every credential value (a transport that names the URL would otherwise name the
//! account id, which rides in the token request's query); an invalid input is named, never
//! echoed.
//!
//! ## Field shapes
//!
//! The Zoom request/response field names below are per Zoom's stable create-meeting + S2S-token
//! API. They are marked so a live call can confirm them; the logic is otherwise covered by
//! hermetic tests against a fake transport (no live credentials needed).
#![forbid(unsafe_code)]

use async_trait::async_trait;
use ikigai_core::{
    ArgSpec, Description, Endpoint, EndpointSpace, Error, Exact, Invocation, ReprType,
    Representation, Result, Verb,
};
use ikigai_http::{HttpRequest, HttpTransport, Method};
use serde_json::{json, Value};
use std::sync::Arc;

/// The net capability family the Sink declares: "holds some grant under `urn:cap:net:`". The
/// module's own rule then requires the two Zoom hosts.
pub const CAP_NET: &str = "urn:cap:net:*";

/// The secret-read family the Sink declares: "holds some grant under `urn:cap:secret:read:`". The
/// module's own rule then requires each credential's name.
pub const CAP_SECRET_READ: &str = "urn:cap:secret:read:*";

/// The faces `as=` selects, in the order the description declares them; the first is the
/// default.
pub const FACES: [&str; 3] = ["text/plain", "text/turtle", "application/json"];

/// The meeting length when `duration` is not given, in minutes.
pub const DEFAULT_MINUTES: u32 = 30;

const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";
const XSD_DATETIME: &str = "http://www.w3.org/2001/XMLSchema#dateTime";
const XSD_INTEGER: &str = "http://www.w3.org/2001/XMLSchema#integer";

/// What a credential value becomes in any error text this module composes from foreign text.
const REDACTED: &str = "[redacted]";

/// A read-only secret resolver. The host resolves `urn:secret:<name>` under a capability and hands
/// the bytes in — the same injection discipline the [`HttpTransport`] gets. Sync because a keystore
/// `get` is a fast blocking call (mirrors ikigai-secret's own `Backend`).
pub trait SecretReader: Send + Sync {
    /// The secret's bytes, or `Error::Denied`/`Error::Endpoint` if the host refuses or can't read.
    fn read(&self, name: &str) -> Result<Vec<u8>>;
}

/// Which secrets hold the Zoom Server-to-Server OAuth credentials, and where the API lives.
#[derive(Clone, Debug)]
pub struct ZoomConfig {
    /// Secret name for the Zoom account id (S2S `account_credentials`).
    pub account_id_secret: String,
    /// Secret name for the OAuth client id.
    pub client_id_secret: String,
    /// Secret name for the OAuth client secret.
    pub client_secret_secret: String,
    /// The user whose calendar hosts the meeting; `me` = the token's own user.
    pub user_id: String,
    /// OAuth token endpoint origin (`https://zoom.us`).
    pub oauth_base: String,
    /// REST API base (`https://api.zoom.us/v2`).
    pub api_base: String,
}

impl Default for ZoomConfig {
    fn default() -> Self {
        ZoomConfig {
            account_id_secret: "zoom-account-id".to_string(),
            client_id_secret: "zoom-client-id".to_string(),
            client_secret_secret: "zoom-client-secret".to_string(),
            user_id: "me".to_string(),
            oauth_base: "https://zoom.us".to_string(),
            api_base: "https://api.zoom.us/v2".to_string(),
        }
    }
}

impl ZoomConfig {
    /// The three credential names, in the order they are read.
    fn secret_names(&self) -> [&str; 3] {
        [
            &self.account_id_secret,
            &self.client_id_secret,
            &self.client_secret_secret,
        ]
    }

    /// The token endpoint, without its query (the account id rides in the query and is a
    /// credential — the gated and reported URL never carries it).
    fn token_url(&self) -> String {
        format!("{}/oauth/token", self.oauth_base.trim_end_matches('/'))
    }

    /// The create-meeting endpoint for the configured user.
    fn meetings_url(&self) -> String {
        format!(
            "{}/users/{}/meetings",
            self.api_base.trim_end_matches('/'),
            self.user_id
        )
    }
}

/// The meeting module. Binds `urn:meeting:schedule` (facade) and `urn:meeting:zoom:schedule`
/// (backend). Slice 0 has one provider, so the facade delegates straight to Zoom; when more
/// backends land it becomes a `provider=`/registry-default router (the `urn:llm:ask` shape).
pub fn space(
    transport: Arc<dyn HttpTransport>,
    secrets: Arc<dyn SecretReader>,
    config: ZoomConfig,
) -> EndpointSpace {
    let zoom = ZoomBackend {
        transport,
        secrets,
        config,
    };
    EndpointSpace::new()
        .bind(Exact::new("urn:meeting:zoom:schedule"), zoom.clone())
        .bind(Exact::new("urn:meeting:schedule"), zoom)
}

/// The Zoom backend: exchange the S2S credentials for a bearer token, then create a scheduled
/// meeting. Cloneable so the facade and the direct alias can share one configuration.
#[derive(Clone)]
pub struct ZoomBackend {
    transport: Arc<dyn HttpTransport>,
    secrets: Arc<dyn SecretReader>,
    config: ZoomConfig,
}

/// One meeting as the caller asked for it — the validated inputs.
struct MeetingRequest {
    topic: String,
    start: String,
    duration: u32,
    timezone: Option<String>,
    agenda: Option<String>,
}

/// The three S2S credentials, read from the keystore.
struct Credentials {
    account_id: String,
    client_id: String,
    client_secret: String,
}

#[async_trait]
impl Endpoint for ZoomBackend {
    async fn invoke(&self, inv: &Invocation<'_>) -> Result<Representation> {
        // Inputs are held to their declared classes: a value outside the class is a typed
        // `InvalidArgument` that names the input and never echoes the value.
        let topic = required(inv, "topic")?;
        let start = required(inv, "start")?.trim().to_string();
        if !is_xsd_datetime(&start) {
            return Err(invalid(
                "start",
                "not an xsd:dateTime (YYYY-MM-DDThh:mm:ss, optionally .fff and Z or ±hh:mm)",
            ));
        }
        let duration = match optional(inv, "duration") {
            None => DEFAULT_MINUTES,
            Some(minutes) => match minutes.parse::<u32>() {
                Ok(n) if n >= 1 => n,
                _ => return Err(invalid("duration", "not a positive xsd:integer (minutes)")),
            },
        };
        let timezone = optional(inv, "timezone");
        // Pipeline citizenship: a named `agenda` wins; the piped `content` is the fallback.
        let agenda = optional(inv, "agenda").or_else(|| optional(inv, "content"));
        let face = optional(inv, "as").unwrap_or_else(|| FACES[0].to_string());
        if !FACES.contains(&face.as_str()) {
            return Err(invalid(
                "as",
                "not one of text/plain, text/turtle, application/json",
            ));
        }

        // Authority first: a typed `Denied` before any secret is read or any socket opens.
        let token_url = self.config.token_url();
        let meetings_url = self.config.meetings_url();
        self.authorize(inv, &[&token_url, &meetings_url])?;

        // Credentials the host resolved from the keystore.
        let creds = self.credentials()?;
        let token = self.access_token(&token_url, &creds).await?;
        let request = MeetingRequest {
            topic,
            start,
            duration,
            timezone,
            agenda,
        };
        let meeting = self
            .create_meeting(&meetings_url, &token, &creds, &request)
            .await?;

        // Faces: default text/plain = the join URL (what the invite needs); `as=text/turtle` = the
        // SHAREABLE graph (join-safe, `ical:conference`); `as=application/json` = the full envelope
        // for the trusted caller, INCLUDING the host `start_url` (a URL that carries a host token).
        Ok(match face.as_str() {
            "text/turtle" => repr("text/turtle", meeting.turtle()),
            "application/json" => repr("application/json", meeting.json_envelope()),
            _ => repr("text/plain", meeting.join_url.into_bytes()),
        })
    }

    // The catalog's short label, which predates the id and differs from it (`meeting` vs
    // `urn:meeting:zoom:schedule`). The description id is the contract — selection, MCP and a
    // conformance fixture all key on it. Renaming either is wave two (ikigai-core PENDING §1),
    // one coordinated pass, because the id is a live MCP tool name.
    fn name(&self) -> &str {
        "meeting"
    }

    fn describe(&self) -> Description {
        Description::new("urn:meeting:zoom:schedule")
            .summary(
                "Schedule a Zoom meeting. Returns the join URL (text), the shareable graph \
                 (as=text/turtle — ical:conference), or the full envelope (as=application/json, \
                 which includes the host start URL). A piped value is the agenda.",
            )
            .verb(Verb::Sink)
            .requires(CAP_NET)
            .requires(CAP_SECRET_READ)
            .input(
                ArgSpec::new("topic")
                    .summary("the meeting title")
                    .class(XSD_STRING),
            )
            .input(
                ArgSpec::new("start")
                    .summary(
                        "start time, ISO 8601 (e.g. 2026-07-30T15:00:00Z, or a local time with \
                         `timezone`)",
                    )
                    .class(XSD_DATETIME),
            )
            .input(
                ArgSpec::new("duration")
                    .summary("length in minutes")
                    .class(XSD_INTEGER)
                    .default_value(DEFAULT_MINUTES.to_string())
                    .optional(),
            )
            .input(
                ArgSpec::new("timezone")
                    .summary("IANA zone the start time is in (e.g. America/Los_Angeles)")
                    .class(XSD_STRING)
                    .optional(),
            )
            .input(
                ArgSpec::new("agenda")
                    .summary("a longer description")
                    .class(XSD_STRING)
                    .optional(),
            )
            .input(
                ArgSpec::new("content")
                    .summary("the agenda, when piped or given as the sink's body — a named `agenda` wins")
                    .class(XSD_STRING)
                    .optional(),
            )
            .input(
                ArgSpec::new("as")
                    .summary("the face: the join URL, the shareable graph, or the full envelope")
                    .class(XSD_STRING)
                    .one_of(FACES)
                    .default_value(FACES[0])
                    .optional(),
            )
            .output(FACES[0])
            .output(FACES[1])
            .output(FACES[2])
    }
}

impl ZoomBackend {
    /// The module's own rules, checked before anything else: every URL's host must be granted
    /// (port-aware), and every credential name must be readable. Root passes both.
    fn authorize(&self, inv: &Invocation<'_>, urls: &[&str]) -> Result<()> {
        for url in urls {
            gate_net(inv, url)?;
        }
        for name in self.config.secret_names() {
            let scope = format!("urn:cap:secret:read:{name}");
            if !inv.capability.allows(&scope) {
                return Err(Error::Denied(format!(
                    "capability does not allow reading secret `{name}` (needs {scope})"
                )));
            }
        }
        Ok(())
    }

    /// The three credentials, each a trimmed UTF-8 string.
    fn credentials(&self) -> Result<Credentials> {
        let [account_id, client_id, client_secret] = self.config.secret_names();
        Ok(Credentials {
            account_id: self.secret_str(account_id)?,
            client_id: self.secret_str(client_id)?,
            client_secret: self.secret_str(client_secret)?,
        })
    }

    /// A secret's bytes as a trimmed UTF-8 string.
    fn secret_str(&self, name: &str) -> Result<String> {
        let bytes = self.secrets.read(name)?;
        String::from_utf8(bytes)
            .map(|s| s.trim().to_string())
            .map_err(|_| Error::Endpoint(format!("secret `{name}` is not valid UTF-8")))
    }

    /// Exchange the S2S credentials for a short-lived bearer token.
    /// `POST {oauth_base}/oauth/token?grant_type=account_credentials&account_id=…`, HTTP Basic
    /// `client_id:client_secret`. (No caching in Slice 0 — a booking approval is rare; caching the
    /// ~1h token is a later optimization.)
    async fn access_token(&self, token_url: &str, creds: &Credentials) -> Result<String> {
        let url = format!(
            "{token_url}?grant_type=account_credentials&account_id={}",
            creds.account_id
        );
        let basic =
            base64_encode(format!("{}:{}", creds.client_id, creds.client_secret).as_bytes());
        let secrets = [
            creds.account_id.as_str(),
            creds.client_id.as_str(),
            creds.client_secret.as_str(),
            basic.as_str(),
        ];
        let resp = self
            .transport
            .send(HttpRequest {
                method: Method::Post,
                url,
                headers: vec![
                    ("Authorization".to_string(), format!("Basic {basic}")),
                    (
                        "Content-Type".to_string(),
                        "application/x-www-form-urlencoded".to_string(),
                    ),
                ],
                body: Vec::new(),
            })
            .await
            .map_err(|e| {
                Error::Endpoint(scrub(format!("zoom oauth transport error: {e}"), &secrets))
            })?;
        if resp.status >= 400 {
            return Err(Error::Endpoint(scrub(
                server_error("oauth", resp.status, &resp.body),
                &secrets,
            )));
        }
        let v: Value = serde_json::from_slice(&resp.body)
            .map_err(|_| Error::Endpoint("zoom oauth: response not JSON".to_string()))?;
        v["access_token"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| Error::Endpoint("zoom oauth: no access_token in response".to_string()))
    }

    /// Create a scheduled meeting. `POST {api_base}/users/{user_id}/meetings`, Bearer token.
    /// Request fields per Zoom's create-meeting API (`type: 2` = scheduled); response fields
    /// `id` / `join_url` / `start_url` / `password`.
    async fn create_meeting(
        &self,
        url: &str,
        token: &str,
        creds: &Credentials,
        request: &MeetingRequest,
    ) -> Result<Meeting> {
        let secrets = [
            creds.account_id.as_str(),
            creds.client_id.as_str(),
            creds.client_secret.as_str(),
            token,
        ];
        let mut body = json!({
            "topic": request.topic,
            "type": 2,
            "start_time": request.start,
            "duration": request.duration,
            "settings": { "join_before_host": true, "waiting_room": false }
        });
        if let Some(tz) = &request.timezone {
            body["timezone"] = json!(tz);
        }
        if let Some(agenda) = &request.agenda {
            body["agenda"] = json!(agenda);
        }
        let resp = self
            .transport
            .send(HttpRequest {
                method: Method::Post,
                url: url.to_string(),
                headers: vec![
                    ("Authorization".to_string(), format!("Bearer {token}")),
                    ("Content-Type".to_string(), "application/json".to_string()),
                ],
                body: serde_json::to_vec(&body).unwrap_or_default(),
            })
            .await
            .map_err(|e| {
                Error::Endpoint(scrub(
                    format!("zoom create-meeting transport error: {e}"),
                    &secrets,
                ))
            })?;
        if resp.status >= 400 {
            return Err(Error::Endpoint(scrub(
                server_error("create-meeting", resp.status, &resp.body),
                &secrets,
            )));
        }
        let v: Value = serde_json::from_slice(&resp.body)
            .map_err(|_| Error::Endpoint("zoom create-meeting: response not JSON".to_string()))?;
        // Zoom returns the meeting id as a NUMBER; accept a string too, defensively.
        let id = v["id"]
            .as_i64()
            .map(|n| n.to_string())
            .or_else(|| v["id"].as_str().map(str::to_string))
            .ok_or_else(|| Error::Endpoint("zoom create-meeting: no id in response".to_string()))?;
        let join_url = v["join_url"].as_str().unwrap_or("").to_string();
        if join_url.is_empty() {
            return Err(Error::Endpoint(
                "zoom create-meeting: no join_url in response".to_string(),
            ));
        }
        Ok(Meeting {
            id,
            join_url,
            start_url: v["start_url"].as_str().unwrap_or("").to_string(),
            passcode: v["password"].as_str().unwrap_or("").to_string(),
            topic: request.topic.clone(),
            start_time: v["start_time"]
                .as_str()
                .unwrap_or(&request.start)
                .to_string(),
            duration: request.duration,
        })
    }
}

/// A required by-value input, or `MissingArgument` naming it.
fn required(inv: &Invocation<'_>, name: &str) -> Result<String> {
    inv.inline_str(name)
        .map(str::to_string)
        .map_err(|_| Error::MissingArgument(name.to_string()))
}

/// An optional by-value input, trimmed; absent or blank is `None`.
fn optional(inv: &Invocation<'_>, name: &str) -> Option<String> {
    inv.inline_str(name)
        .ok()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn invalid(name: &str, detail: &str) -> Error {
    Error::InvalidArgument {
        name: name.to_string(),
        detail: detail.to_string(),
    }
}

/// Refuse before the socket if the capability doesn't grant this URL's host (and port) — so a
/// declared `urn:cap:net:*` is enforced per host, not merely offered. The message names the host,
/// never the URL: the token URL's query carries a credential.
fn gate_net(inv: &Invocation<'_>, url: &str) -> Result<()> {
    let parsed = url::Url::parse(url).map_err(|e| Error::Endpoint(format!("bad url: {e}")))?;
    let host = parsed.host_str().unwrap_or("");
    if ikigai_http::net_allows_port(
        inv.capability,
        host,
        parsed.port_or_known_default(),
        parsed.path(),
    ) {
        Ok(())
    } else {
        Err(Error::Denied(format!(
            "capability does not allow reaching `{host}` (needs urn:cap:net:{host})"
        )))
    }
}

/// What a server's error response contributes to an error: its status and, when the body is
/// JSON, its `message` / `reason` / `error` string — never the body itself, which a server may
/// fill with the request it refused (headers included).
fn server_error(what: &str, status: u16, body: &[u8]) -> String {
    let detail = serde_json::from_slice::<Value>(body).ok().and_then(|v| {
        ["message", "reason", "error"]
            .iter()
            .find_map(|key| v[*key].as_str().map(str::to_string))
    });
    match detail {
        Some(message) => format!("zoom {what} returned {status}: {message}"),
        None => format!(
            "zoom {what} returned {status} ({} bytes, not JSON)",
            body.len()
        ),
    }
}

/// Replace every credential value in `text` — a transport's or a server's own words, which this
/// module does not control — with a marker. An empty value is skipped (it matches everywhere).
fn scrub(text: String, secrets: &[&str]) -> String {
    secrets
        .iter()
        .filter(|s| !s.is_empty())
        .fold(text, |acc, secret| acc.replace(secret, REDACTED))
}

/// A lexical `xsd:dateTime` (the shape Zoom accepts): `YYYY-MM-DDThh:mm:ss`, an optional
/// fractional second, an optional `Z` or `±hh:mm`. Field ranges are the provider's to judge.
fn is_xsd_datetime(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() < 19 {
        return false;
    }
    let digits = |range: std::ops::Range<usize>| b[range].iter().all(u8::is_ascii_digit);
    let shape = digits(0..4)
        && b[4] == b'-'
        && digits(5..7)
        && b[7] == b'-'
        && digits(8..10)
        && b[10] == b'T'
        && digits(11..13)
        && b[13] == b':'
        && digits(14..16)
        && b[16] == b':'
        && digits(17..19);
    if !shape {
        return false;
    }
    // The first 19 bytes are ASCII, so 19 is a char boundary.
    let mut rest = &s[19..];
    if let Some(fraction) = rest.strip_prefix('.') {
        let n = fraction.bytes().take_while(u8::is_ascii_digit).count();
        if n == 0 {
            return false;
        }
        rest = &fraction[n..];
    }
    let z = rest.as_bytes();
    match z {
        [] | [b'Z'] => true,
        [sign, h1, h2, b':', m1, m2] => {
            matches!(sign, b'+' | b'-') && [h1, h2, m1, m2].iter().all(|d| d.is_ascii_digit())
        }
        _ => false,
    }
}

/// A scheduled meeting, provider-neutral.
struct Meeting {
    id: String,
    join_url: String,
    /// The host URL — carries a start token, so it is NEVER in the shareable turtle face.
    start_url: String,
    passcode: String,
    topic: String,
    start_time: String,
    duration: u32,
}

impl Meeting {
    /// The SHAREABLE graph — join-safe fields only. The meeting is an `ical:Vevent` whose
    /// `ical:conference` is the join URL (RFC 7986's CONFERENCE property, a URI; an ICS generator
    /// maps it to a `CONFERENCE:` line + the event's link), `dcterms:identifier` the provider's
    /// meeting id and `schema:provider` the provider. The host `start_url` is deliberately
    /// withheld. `ik:passcode` is the one term the shared vocabulary does not define yet (a
    /// numeric passcode has no well-known home: RFC 7986 carries a passcode only inside a
    /// conference URI, and Zoom's `pwd=` is an encrypted token, not the code).
    fn turtle(&self) -> Vec<u8> {
        let mut s = String::new();
        s.push_str("@prefix ik: <https://ikigai-rs.dev/ns#> .\n");
        s.push_str("@prefix ical: <http://www.w3.org/2002/12/cal/ical#> .\n");
        s.push_str("@prefix dcterms: <http://purl.org/dc/terms/> .\n");
        s.push_str("@prefix schema: <https://schema.org/> .\n");
        s.push_str("@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .\n\n");
        s.push_str(&format!(
            "{} a ical:Vevent ;\n",
            ttl_iri(&format!("urn:meeting:zoom:{}", self.id))
        ));
        s.push_str("    schema:provider \"zoom\" ;\n");
        s.push_str(&format!("    dcterms:identifier {} ;\n", ttl_str(&self.id)));
        s.push_str(&format!("    ical:summary {} ;\n", ttl_str(&self.topic)));
        s.push_str(&format!(
            "    ical:conference {} ;\n",
            ttl_iri(&self.join_url)
        ));
        if !self.passcode.is_empty() {
            s.push_str(&format!("    ik:passcode {} ;\n", ttl_str(&self.passcode)));
        }
        s.push_str(&format!(
            "    ical:dtstart {}^^xsd:dateTime .\n",
            ttl_str(&self.start_time)
        ));
        s.into_bytes()
    }

    /// The FULL envelope for the trusted caller — includes the host `start_url`.
    fn json_envelope(&self) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "provider": "zoom",
            "id": self.id,
            "join_url": self.join_url,
            "start_url": self.start_url,
            "passcode": self.passcode,
            "topic": self.topic,
            "start_time": self.start_time,
            "duration": self.duration,
        }))
        .unwrap_or_default()
    }
}

fn repr(content_type: &str, body: Vec<u8>) -> Representation {
    Representation::new(ReprType::new(content_type), body)
}

/// A Turtle string literal (quote-and-escape).
fn ttl_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// A Turtle IRI reference. Characters an IRIREF cannot carry (Turtle §19: `<>"{}|^\``, space
/// and controls) are percent-encoded, so a provider's value can never break the graph.
fn ttl_iri(iri: &str) -> String {
    let mut out = String::with_capacity(iri.len() + 2);
    out.push('<');
    for byte in iri.bytes() {
        match byte {
            b'<' | b'>' | b'"' | b'{' | b'}' | b'|' | b'^' | b'`' | b'\\' | 0x00..=0x20 => {
                out.push_str(&format!("%{byte:02X}"));
            }
            _ => out.push(byte as char),
        }
    }
    out.push('>');
    out
}

/// Standard base64 with padding — just enough for the HTTP Basic header, so no dependency.
fn base64_encode(input: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
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

#[cfg(test)]
mod tests {
    use super::*;
    use futures::executor::block_on;
    use ikigai_core::{ArgRef, Capability, Iri, Kernel, Request};
    use ikigai_http::HttpResponse;
    use std::sync::Mutex;

    /// A fake transport with canned replies keyed by URL substring, recording every request so a
    /// test can assert the auth header and body shape. No network, no credentials.
    #[derive(Default)]
    struct FakeZoom {
        seen: Mutex<Vec<HttpRequest>>,
    }

    #[async_trait]
    impl HttpTransport for FakeZoom {
        async fn send(&self, request: HttpRequest) -> std::result::Result<HttpResponse, String> {
            let body = if request.url.contains("/oauth/token") {
                r#"{"access_token":"tok-123","token_type":"bearer","expires_in":3600}"#
            } else if request.url.contains("/meetings") {
                r#"{"id":88899900011,"join_url":"https://zoom.us/j/88899900011?pwd=abc","start_url":"https://zoom.us/s/88899900011?zak=SECRET","password":"a1b2c3","start_time":"2026-07-30T22:00:00Z"}"#
            } else {
                return Err(format!("unexpected url {}", request.url));
            };
            self.seen.lock().unwrap().push(request);
            Ok(HttpResponse {
                status: 200,
                headers: vec![],
                body: body.as_bytes().to_vec(),
            })
        }
    }

    struct FakeSecrets;
    impl SecretReader for FakeSecrets {
        fn read(&self, name: &str) -> Result<Vec<u8>> {
            Ok(match name {
                "zoom-account-id" => b"acct-1".to_vec(),
                "zoom-client-id" => b"cid-1".to_vec(),
                "zoom-client-secret" => b"csec-1".to_vec(),
                other => return Err(Error::Endpoint(format!("no secret {other}"))),
            })
        }
    }

    fn kernel(transport: Arc<FakeZoom>) -> Kernel {
        Kernel::new(Arc::new(space(
            transport,
            Arc::new(FakeSecrets),
            ZoomConfig::default(),
        )))
    }

    /// A capability that grants the two Zoom hosts and the three credential names (what the host
    /// would attenuate the handler to).
    fn zoom_cap() -> Capability {
        Capability::scoped(vec![
            "urn:cap:meeting:zoom:schedule".to_string(),
            "urn:cap:net:zoom.us".to_string(),
            "urn:cap:net:api.zoom.us".to_string(),
            "urn:cap:secret:read:zoom-account-id".to_string(),
            "urn:cap:secret:read:zoom-client-id".to_string(),
            "urn:cap:secret:read:zoom-client-secret".to_string(),
        ])
    }

    fn schedule(
        transport: Arc<FakeZoom>,
        cap: &Capability,
        args: &[(&str, &str)],
    ) -> std::result::Result<String, Error> {
        let k = kernel(transport);
        let mut req = Request::new(Verb::Sink, Iri::parse("urn:meeting:zoom:schedule").unwrap());
        for (name, value) in args {
            req = req.with_arg(*name, ArgRef::Inline(value.as_bytes().to_vec()));
        }
        block_on(k.issue(req, cap)).map(|r| String::from_utf8(r.bytes).unwrap())
    }

    #[test]
    fn schedule_returns_the_join_url_by_default() {
        let out = schedule(
            Arc::new(FakeZoom::default()),
            &zoom_cap(),
            &[
                ("topic", "Intro call"),
                ("start", "2026-07-30T22:00:00Z"),
                ("duration", "30"),
            ],
        )
        .expect("schedules");
        assert_eq!(out.trim(), "https://zoom.us/j/88899900011?pwd=abc");
    }

    #[test]
    fn the_turtle_face_carries_ical_conference_but_not_the_host_url() {
        let out = schedule(
            Arc::new(FakeZoom::default()),
            &zoom_cap(),
            &[
                ("topic", "Intro call"),
                ("start", "2026-07-30T22:00:00Z"),
                ("as", "text/turtle"),
            ],
        )
        .expect("schedules");
        assert!(
            out.contains("ical:conference <https://zoom.us/j/88899900011?pwd=abc>"),
            "{out}"
        );
        assert!(
            out.contains("<urn:meeting:zoom:88899900011> a ical:Vevent"),
            "{out}"
        );
        assert!(out.contains("dcterms:identifier \"88899900011\""), "{out}");
        assert!(out.contains("schema:provider \"zoom\""), "{out}");
        assert!(out.contains("ik:passcode \"a1b2c3\""), "{out}");
        assert!(
            out.contains("ical:dtstart \"2026-07-30T22:00:00Z\"^^xsd:dateTime"),
            "{out}"
        );
        // The host start_url carries a token — it must never reach the shareable graph.
        assert!(
            !out.contains("zak=SECRET"),
            "host url leaked into turtle: {out}"
        );
        assert!(!out.contains("start_url"), "{out}");
    }

    #[test]
    fn the_oauth_call_uses_http_basic_and_the_meeting_call_bears_the_token() {
        let transport = Arc::new(FakeZoom::default());
        schedule(
            Arc::clone(&transport),
            &zoom_cap(),
            &[("topic", "t"), ("start", "2026-07-30T22:00:00Z")],
        )
        .expect("schedules");
        let seen = transport.seen.lock().unwrap();
        let oauth = seen
            .iter()
            .find(|r| r.url.contains("/oauth/token"))
            .unwrap();
        // Basic base64("cid-1:csec-1").
        let want = format!("Basic {}", base64_encode(b"cid-1:csec-1"));
        assert!(
            oauth
                .headers
                .iter()
                .any(|(k, v)| k == "Authorization" && *v == want),
            "{:?}",
            oauth.headers
        );
        assert!(oauth.url.contains("account_id=acct-1"), "{}", oauth.url);
        let create = seen.iter().find(|r| r.url.contains("/meetings")).unwrap();
        assert!(create
            .headers
            .iter()
            .any(|(k, v)| k == "Authorization" && v == "Bearer tok-123"));
        let sent: Value = serde_json::from_slice(&create.body).unwrap();
        assert_eq!(sent["type"], 2);
        assert_eq!(sent["topic"], "t");
        assert_eq!(sent["duration"], DEFAULT_MINUTES);
    }

    #[test]
    fn a_missing_net_capability_is_refused_before_the_socket() {
        // Holds the meeting cap and the secret grants but NOT the net grants → the kernel's
        // declared floor refuses; the transport is untouched.
        let cap = Capability::scoped(vec![
            "urn:cap:meeting:zoom:schedule".to_string(),
            "urn:cap:secret:read:zoom-account-id".to_string(),
        ]);
        let transport = Arc::new(FakeZoom::default());
        let err = schedule(
            Arc::clone(&transport),
            &cap,
            &[("topic", "t"), ("start", "2026-07-30T22:00:00Z")],
        )
        .unwrap_err();
        assert!(matches!(err, Error::Denied(_)), "{err:?}");
        assert!(err.to_string().contains(CAP_NET), "{err}");
        assert!(
            transport.seen.lock().unwrap().is_empty(),
            "reached the network without a net cap"
        );
    }

    #[test]
    fn a_missing_topic_is_a_clear_error() {
        let err = schedule(
            Arc::new(FakeZoom::default()),
            &zoom_cap(),
            &[("start", "2026-07-30T22:00:00Z")],
        )
        .unwrap_err();
        assert!(
            matches!(err, Error::MissingArgument(ref a) if a == "topic"),
            "{err:?}"
        );
    }

    #[test]
    fn inputs_are_held_to_their_declared_classes() {
        for (name, value) in [
            ("start", "tomorrow at noon"),
            ("start", "2026-07-30 22:00:00Z"),
            ("duration", "ninety"),
            ("duration", "0"),
            ("as", "text/html"),
        ] {
            let mut args = vec![("topic", "t"), ("start", "2026-07-30T22:00:00Z")];
            args.retain(|(n, _)| *n != name);
            args.push((name, value));
            let err = schedule(Arc::new(FakeZoom::default()), &zoom_cap(), &args).unwrap_err();
            assert!(
                matches!(err, Error::InvalidArgument { name: ref n, .. } if n == name),
                "{name}={value}: {err:?}"
            );
            assert!(!err.to_string().contains(value), "echoed the value: {err}");
        }
    }

    #[test]
    fn xsd_datetime_lexical_forms() {
        for ok in [
            "2026-07-30T22:00:00Z",
            "2026-07-30T22:00:00",
            "2026-07-30T22:00:00.5Z",
            "2026-07-30T22:00:00.123456-07:00",
            "2026-07-30T22:00:00+05:30",
        ] {
            assert!(is_xsd_datetime(ok), "{ok}");
        }
        for bad in [
            "",
            "2026-07-30",
            "2026-07-30 22:00:00",
            "2026-07-30T22:00",
            "2026-07-30T22:00:00.Z",
            "2026-07-30T22:00:00+0530",
            "2026-07-30T22:00:00Zed",
            "2026-07-30T22:00:00 ",
        ] {
            assert!(!is_xsd_datetime(bad), "{bad:?}");
        }
    }

    #[test]
    fn scrub_redacts_every_credential_and_skips_an_empty_one() {
        let text = "https://zoom.us/oauth/token?account_id=acct-1: refused for cid-1".to_string();
        let out = scrub(text, &["acct-1", "", "cid-1"]);
        assert_eq!(
            out,
            "https://zoom.us/oauth/token?account_id=[redacted]: refused for [redacted]"
        );
    }

    #[test]
    fn a_server_error_contributes_only_its_message() {
        let echo = br#"{"code":300,"message":"bad request","echo":{"authorization":"Bearer tok"}}"#;
        assert_eq!(
            server_error("create-meeting", 400, echo),
            "zoom create-meeting returned 400: bad request"
        );
        assert_eq!(
            server_error(
                "oauth",
                401,
                br#"{"reason":"Invalid client_id or client_secret","error":"invalid_client"}"#
            ),
            "zoom oauth returned 401: Invalid client_id or client_secret"
        );
        assert_eq!(
            server_error("oauth", 502, b"<html>Bearer tok</html>"),
            "zoom oauth returned 502 (23 bytes, not JSON)"
        );
    }

    #[test]
    fn ttl_iri_percent_encodes_what_an_iriref_cannot_carry() {
        assert_eq!(ttl_iri("urn:meeting:zoom:1"), "<urn:meeting:zoom:1>");
        assert_eq!(
            ttl_iri("https://zoom.us/j/1?pwd=a b>c"),
            "<https://zoom.us/j/1?pwd=a%20b%3Ec>"
        );
    }

    #[test]
    fn base64_encode_matches_rfc4648_vectors() {
        // A self-consistent encoder passes the auth-header test yet could still be WRONG — the
        // fake transport can't catch that. RFC 4648 §10's vectors exercise every padding case
        // (0, 1, 2 `=`) and pin the encoder to correct, so the HTTP Basic header is real base64.
        for (input, want) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
            ("hello", "aGVsbG8="),
        ] {
            assert_eq!(base64_encode(input.as_bytes()), want, "base64({input:?})");
        }
    }
}
