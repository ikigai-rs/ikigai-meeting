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
//! ## What an error may say, and what it IS
//!
//! An error travels through traces, logs and MCP replies, so nothing a credential or a caller
//! put into the request comes back in one. A server's 4xx contributes only its `message` /
//! `reason` / `error` field (a body that echoes the request is dropped); a transport's text is
//! scrubbed of every credential value (a transport that echoes the request it could not send would
//! otherwise name the account id, which rides in the token request's form body); an invalid input
//! is named, never echoed.
//!
//! Its TYPE is ikigai-http's one status table, [`ikigai_http::status`] (ledger #1546), the same
//! table `urn:http:*` and ikigai-llm answer from, so a Zoom answer means the same thing here as an
//! origin's answer anywhere else: a `401`/`403` is [`Error::Refused`] (Zoom refused the credential
//! THIS module presented; the capability floor's [`Error::Denied`] is never Zoom's word), a `429`
//! is [`Error::RetryAfter`] carrying the `Retry-After` delay when the header parses, a `500` is
//! [`Error::Endpoint`] (unknown: no promise that a repeat helps), a `502`/`503` is
//! [`Error::Unavailable`] and a `408`/`504` an [`Error::Timeout`] (transient), `413`/`414`/`431`
//! [`Error::OverBudget`]; a transport that got no answer is a [`Error::Timeout`] when it timed out
//! and an [`Error::Unavailable`] otherwise. The action DECLARES those classes
//! (`Description::fails`), so the manifold says how a schedule can fail before anyone calls it.
//!
//! ## The token is cached, on the invocation's clock
//!
//! A Server-to-Server token lives about an hour (`expires_in`), so the backend keeps each one
//! in-process, keyed by the token endpoint, the account id and the client id, and reuses it until
//! [`TOKEN_REFRESH_MARGIN_SECS`] before it expires. Time is the invocation's ([`Invocation::now`]):
//! a kernel with no clock has no time, and then every call mints a fresh token, which is the
//! honest answer rather than a read of the wall clock behind the host's back. The cache can still
//! be wrong about a token (a pinned corridor's clock, a revoked token), so a create-meeting `401`
//! for a CACHED token drops it and tries once more with a fresh one; a `401` for a fresh token is
//! the [`Error::Refused`] it says it is, and terminal: the credential's holder renewed it once,
//! which is the one retry the `refused` class allows. The credentials are still read, and the
//! authority still checked, on every call: the cache saves only the token exchange.
//!
//! ## Field shapes
//!
//! The Zoom request/response field names below are per Zoom's stable create-meeting + S2S-token
//! API. They are marked so a live call can confirm them; the logic is otherwise covered by
//! hermetic tests against a fake transport (no live credentials needed).
#![forbid(unsafe_code)]

use async_trait::async_trait;
use ikigai_core::{
    ArgSpec, Description, Endpoint, EndpointSpace, Error, Exact, FailureClass, Invocation,
    ReprType, Representation, Result, Time, Verb,
};
use ikigai_http::status::{response_error, transport_error};
use ikigai_http::{HttpRequest, HttpResponse, HttpTransport, Method};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// The net capability family the Sink declares: "holds some grant under `urn:cap:net:`". The
/// module's own rule then requires the two Zoom hosts.
pub const CAP_NET: &str = "urn:cap:net:*";

/// The secret-read family the Sink declares: "holds some grant under `urn:cap:secret:read:`". The
/// module's own rule then requires each credential's name.
pub const CAP_SECRET_READ: &str = "urn:cap:secret:read:*";

/// The faces `as=` selects, in the order the description declares them; the first is the
/// default.
pub const FACES: [&str; 3] = ["text/plain", "text/turtle", "application/json"];

/// The endpoint's one name: its description id (the MCP projection's tool name) and its catalog
/// label. Both bound IRIs, `urn:meeting:schedule` and `urn:meeting:zoom:schedule`, reach it.
const ID: &str = "meeting";

/// The meeting length when `duration` is not given, in minutes.
pub const DEFAULT_MINUTES: u32 = 30;

/// How long before its stated expiry a cached token stops being used, in seconds. A token whose
/// `expires_in` is no longer than this is used once and not cached.
pub const TOKEN_REFRESH_MARGIN_SECS: u64 = 60;

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

    /// The token endpoint. It carries no query: the account id is a credential and rides in the
    /// form body, so no URL this module sends, gates or reports ever names it.
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
///
/// What a caller sees when Zoom answers an error (ledger #1546): the variant, its class and its
/// display are ikigai-http's one status table's, with this module's words as the detail and the
/// body's `message` kept, the body's echo dropped. A `429` with `Retry-After: 30` and a `401`,
/// through a fake transport:
///
/// ```
/// use std::sync::Arc;
/// use async_trait::async_trait;
/// use ikigai_core::{ArgRef, Capability, Error, FailureClass, Iri, Kernel, Request, Verb};
/// use ikigai_http::{HttpRequest, HttpResponse, HttpTransport};
/// use ikigai_meeting::{SecretReader, ZoomConfig};
///
/// /// Zoom's token endpoint, answering one scripted status.
/// struct Zoom(u16, Vec<(String, String)>, &'static str);
/// #[async_trait]
/// impl HttpTransport for Zoom {
///     async fn send(&self, _: HttpRequest) -> Result<HttpResponse, String> {
///         Ok(HttpResponse { status: self.0, headers: self.1.clone(), body: self.2.as_bytes().to_vec() })
///     }
/// }
/// struct Secrets;
/// impl SecretReader for Secrets {
///     fn read(&self, _: &str) -> ikigai_core::Result<Vec<u8>> { Ok(b"fake".to_vec()) }
/// }
/// fn schedule(zoom: Zoom) -> Error {
///     let kernel = Kernel::new(Arc::new(ikigai_meeting::space(
///         Arc::new(zoom), Arc::new(Secrets), ZoomConfig::default(),
///     )));
///     let request = Request::new(Verb::Sink, Iri::parse("urn:meeting:schedule").unwrap())
///         .with_arg("topic", ArgRef::Inline(b"Intro call".to_vec()))
///         .with_arg("start", ArgRef::Inline(b"2026-07-30T22:00:00Z".to_vec()));
///     futures::executor::block_on(kernel.issue(request, &Capability::root())).unwrap_err()
/// }
///
/// let limited = schedule(Zoom(
///     429,
///     vec![("Retry-After".to_string(), "30".to_string())],
///     r#"{"message":"too many requests"}"#,
/// ));
/// assert_eq!(limited.class(), FailureClass::RetryAfter);
/// assert_eq!(limited.retry_after_millis(), Some(30_000));
/// assert_eq!(
///     limited.to_string(),
///     "retry after 30000 ms: rate-limited: zoom oauth returned 429: too many requests"
/// );
///
/// let refused = schedule(Zoom(401, vec![], r#"{"reason":"Invalid client_id or client_secret"}"#));
/// assert!(matches!(refused, Error::Refused(_)));
/// assert_eq!(refused.class(), FailureClass::Refused);
/// assert_eq!(refused.to_string(), "refused: zoom oauth returned 401: Invalid client_id or client_secret");
///
/// // A 500 is `Endpoint` (unknown), the class that says a repeat promises nothing.
/// assert_eq!(schedule(Zoom(500, vec![], "")).class(), FailureClass::Unknown);
/// ```
pub fn space(
    transport: Arc<dyn HttpTransport>,
    secrets: Arc<dyn SecretReader>,
    config: ZoomConfig,
) -> EndpointSpace {
    let zoom = ZoomBackend {
        transport,
        secrets,
        config,
        tokens: Arc::default(),
    };
    EndpointSpace::new()
        .bind(Exact::new("urn:meeting:zoom:schedule"), zoom.clone())
        .bind(Exact::new("urn:meeting:schedule"), zoom)
}

/// The Zoom backend: exchange the S2S credentials for a bearer token, then create a scheduled
/// meeting. Cloneable so the facade and the direct alias can share one configuration and one
/// token cache.
#[derive(Clone)]
pub struct ZoomBackend {
    transport: Arc<dyn HttpTransport>,
    secrets: Arc<dyn SecretReader>,
    config: ZoomConfig,
    tokens: Arc<TokenCache>,
}

/// One meeting as the caller asked for it — the validated inputs.
struct MeetingRequest {
    topic: String,
    start: String,
    duration: u32,
    timezone: Option<String>,
    agenda: Option<String>,
    attendees: Vec<String>,
}

/// Which token a cached entry is: the endpoint that minted it and the account and client it was
/// minted for. The client secret is not part of it: a token minted before a rotation is still the
/// account's token, and if Zoom refuses it the `401` path drops it.
#[derive(Clone, PartialEq, Eq, Hash)]
struct TokenKey {
    token_url: String,
    account_id: String,
    client_id: String,
}

impl TokenKey {
    fn of(token_url: &str, creds: &Credentials) -> TokenKey {
        TokenKey {
            token_url: token_url.to_string(),
            account_id: creds.account_id.clone(),
            client_id: creds.client_id.clone(),
        }
    }
}

/// A minted token and the instant (milliseconds, the invocation's clock) it stops being used.
struct CachedToken {
    token: String,
    until_millis: u64,
}

/// The in-process token cache. Never `Debug`: it holds bearer tokens.
#[derive(Default)]
struct TokenCache(Mutex<HashMap<TokenKey, CachedToken>>);

impl TokenCache {
    /// The cached token for `key` if it is still in use at `now`; an expired entry is dropped.
    fn get(&self, key: &TokenKey, now: u64) -> Option<String> {
        let mut tokens = self.0.lock().unwrap_or_else(|e| e.into_inner());
        match tokens.get(key) {
            Some(cached) if now < cached.until_millis => Some(cached.token.clone()),
            Some(_) => {
                tokens.remove(key);
                None
            }
            None => None,
        }
    }

    /// Keep `token`, minted at `now` and valid for `expires_in` seconds, until
    /// [`TOKEN_REFRESH_MARGIN_SECS`] before it expires. A lifetime no longer than the margin is
    /// not kept.
    fn put(&self, key: TokenKey, token: &str, now: u64, expires_in: u64) {
        if expires_in <= TOKEN_REFRESH_MARGIN_SECS {
            return;
        }
        let usable = expires_in - TOKEN_REFRESH_MARGIN_SECS;
        let until_millis = now.saturating_add(usable.saturating_mul(1000));
        self.0.lock().unwrap_or_else(|e| e.into_inner()).insert(
            key,
            CachedToken {
                token: token.to_string(),
                until_millis,
            },
        );
    }

    /// Drop `key`'s entry if it still holds `token` (a concurrent call may have replaced it).
    fn evict(&self, key: &TokenKey, token: &str) {
        let mut tokens = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if tokens.get(key).is_some_and(|cached| cached.token == token) {
            tokens.remove(key);
        }
    }
}

/// What a create-meeting call came back with: the meeting, or a `401` for a token the cache
/// supplied, which the caller answers by minting a fresh one and trying once more.
enum Created {
    Meeting(Meeting),
    TokenRefused,
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
        let attendees = match optional(inv, "attendees") {
            None => Vec::new(),
            Some(list) => parse_attendees(&list).ok_or_else(|| {
                invalid(
                    "attendees",
                    "not a list of email addresses (separated by commas, semicolons or whitespace)",
                )
            })?,
        };
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
        let request = MeetingRequest {
            topic,
            start,
            duration,
            timezone,
            agenda,
            attendees,
        };
        // The token: a cached one while the invocation's clock says it is still good, else a
        // fresh one (kept when there is a clock to judge it by). The same clock measures a
        // `Retry-After` given as an HTTP-date.
        let now = inv.now();
        let key = TokenKey::of(&token_url, &creds);
        let cached = now.and_then(|now| self.tokens.get(&key, now.as_millis()));
        let meeting = match cached {
            Some(token) => match self
                .create_meeting(&meetings_url, &token, &creds, &request, true, now)
                .await?
            {
                Created::Meeting(meeting) => meeting,
                Created::TokenRefused => {
                    self.tokens.evict(&key, &token);
                    let token = self.fresh_token(&token_url, &creds, &key, now).await?;
                    self.create_meeting(&meetings_url, &token, &creds, &request, false, now)
                        .await?
                        .into_meeting()?
                }
            },
            None => {
                let token = self.fresh_token(&token_url, &creds, &key, now).await?;
                self.create_meeting(&meetings_url, &token, &creds, &request, false, now)
                    .await?
                    .into_meeting()?
            }
        };

        // Faces: default text/plain = the join URL (what the invite needs); `as=text/turtle` = the
        // SHAREABLE graph (join-safe, `ical:conference`); `as=application/json` = the full envelope
        // for the trusted caller, INCLUDING the host `start_url` (a URL that carries a host token).
        Ok(match face.as_str() {
            "text/turtle" => repr("text/turtle", meeting.turtle()),
            "application/json" => repr("application/json", meeting.json_envelope()),
            _ => repr("text/plain", meeting.join_url.into_bytes()),
        })
    }

    // The catalog's short label and the description id are one name, `meeting`: a short noun in
    // kebab-case (ikigai-core's naming conventions). Both bound IRIs reach this one endpoint, so
    // the name is neither IRI. The id is the contract (selection and a conformance fixture key on
    // it) and the MCP projection's tool name, so renaming it moves an agent's tool list.
    fn name(&self) -> &str {
        ID
    }

    fn describe(&self) -> Description {
        Description::new(ID)
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
                ArgSpec::new("attendees")
                    .summary(
                        "who the meeting is for: email addresses, separated by commas, semicolons \
                         or whitespace (Zoom's meeting invitees; ical:attendee in the graph)",
                    )
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
            // How a schedule can fail (ledger #1546): every class ikigai-http's status table
            // can answer for an origin's error, since both Zoom calls pass their answer through
            // it, plus the transport's two transient failures. `permanent` (an invalid or
            // missing input, a 404) and `denied` (beside the two `requires`) are implied.
            .fails(FailureClass::Transient)
            .fails(FailureClass::RetryAfter)
            .fails(FailureClass::OverBudget)
            .fails(FailureClass::Refused)
            .fails(FailureClass::Unknown)
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
    /// `POST {oauth_base}/oauth/token`, HTTP Basic `client_id:client_secret`, with
    /// `grant_type=account_credentials&account_id=…` as an `application/x-www-form-urlencoded`
    /// body — RFC 6749 §4.4.2's client-credentials request. Zoom's own examples put those two
    /// parameters in the QUERY; the body keeps the account id (a credential) out of every URL a
    /// host transport might log. Answers the token and its `expires_in` (seconds), when stated.
    /// An error answer is typed by [`origin_error`] (so a `401` for the client credentials is
    /// [`Error::Refused`]), a transport failure by [`no_answer`].
    async fn access_token(
        &self,
        token_url: &str,
        creds: &Credentials,
        now: Option<Time>,
    ) -> Result<(String, Option<u64>)> {
        // The shared RFC 7617 builder (ledger #1857). Its refusal names the broken rule and the
        // stable code, never the client id or secret, so the error text is safe as it stands.
        let auth =
            ikigai_http::auth::basic(&creds.client_id, &creds.client_secret).map_err(|e| {
                Error::Endpoint(format!(
                    "zoom oauth: the client credentials cannot form a Basic header ({}): {e}",
                    e.code()
                ))
            })?;
        let secrets = [
            creds.account_id.as_str(),
            creds.client_id.as_str(),
            creds.client_secret.as_str(),
            auth.credentials(),
        ];
        let resp = self
            .transport
            .send(HttpRequest {
                method: Method::Post,
                url: token_url.to_string(),
                headers: vec![
                    ("Authorization".into(), auth.as_str().to_string()),
                    (
                        "Content-Type".to_string(),
                        "application/x-www-form-urlencoded".to_string(),
                    ),
                ],
                body: token_form(&creds.account_id),
            })
            .await
            .map_err(|e| no_answer(&e, token_url, "oauth", &secrets))?;
        if resp.status >= 400 {
            return Err(origin_error(&resp, now, "oauth", &secrets));
        }
        let v: Value = serde_json::from_slice(&resp.body)
            .map_err(|_| Error::Endpoint("zoom oauth: response not JSON".to_string()))?;
        let token = v["access_token"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| {
                Error::Endpoint("zoom oauth: no access_token in response".to_string())
            })?;
        // Zoom states `expires_in` as a number of seconds; accept a string too, defensively.
        let expires_in = v["expires_in"]
            .as_u64()
            .or_else(|| v["expires_in"].as_str().and_then(|s| s.trim().parse().ok()));
        Ok((token, expires_in))
    }

    /// A token minted now, kept in the cache when the invocation has a clock to judge it by.
    async fn fresh_token(
        &self,
        token_url: &str,
        creds: &Credentials,
        key: &TokenKey,
        now: Option<Time>,
    ) -> Result<String> {
        let (token, expires_in) = self.access_token(token_url, creds, now).await?;
        if let (Some(now), Some(expires_in)) = (now, expires_in) {
            self.tokens
                .put(key.clone(), &token, now.as_millis(), expires_in);
        }
        Ok(token)
    }

    /// Create a scheduled meeting. `POST {api_base}/users/{user_id}/meetings`, Bearer token.
    /// Request fields per Zoom's create-meeting API (`type: 2` = scheduled; the attendees as
    /// `settings.meeting_invitees`, a list of `{"email": …}`); response fields
    /// `id` / `join_url` / `start_url` / `password`. A `401` for a token the cache supplied
    /// (`cached`) is [`Created::TokenRefused`], not an error: nothing was created, so the caller
    /// may mint a fresh token and try once more. Every other error answer is typed by
    /// [`origin_error`], a transport failure by [`no_answer`].
    async fn create_meeting(
        &self,
        url: &str,
        token: &str,
        creds: &Credentials,
        request: &MeetingRequest,
        cached: bool,
        now: Option<Time>,
    ) -> Result<Created> {
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
        if !request.attendees.is_empty() {
            body["settings"]["meeting_invitees"] = request
                .attendees
                .iter()
                .map(|email| json!({ "email": email }))
                .collect();
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
            .map_err(|e| no_answer(&e, url, "create-meeting", &secrets))?;
        if resp.status == 401 && cached {
            return Ok(Created::TokenRefused);
        }
        if resp.status >= 400 {
            return Err(origin_error(&resp, now, "create-meeting", &secrets));
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
        Ok(Created::Meeting(Meeting {
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
            attendees: request.attendees.clone(),
        }))
    }
}

impl Created {
    /// The meeting. Only a call made with a cached token can answer `TokenRefused`, so after a
    /// fresh token it is the plain `401` it would otherwise have been: a [`Error::Refused`].
    fn into_meeting(self) -> Result<Meeting> {
        match self {
            Created::Meeting(meeting) => Ok(meeting),
            Created::TokenRefused => Err(Error::Refused(
                "zoom create-meeting returned 401".to_string(),
            )),
        }
    }
}

/// An error ANSWER from Zoom, typed by ikigai-http's one status table
/// ([`ikigai_http::status::response_error`], ledger #1546): the variant is the table's row for
/// the status (`401`/`403` [`Error::Refused`], `429` [`Error::RetryAfter`] with the `Retry-After`
/// delay measured on `now`, `500` [`Error::Endpoint`], ...), and the detail is this module's
/// [`server_error`] text, scrubbed of every credential. `what` names the call (`oauth`,
/// `create-meeting`). Only called for a status of 400 or more; a success handed in anyway is
/// still reported as a failure, never a panic.
fn origin_error(resp: &HttpResponse, now: Option<Time>, what: &str, secrets: &[&str]) -> Error {
    let detail = scrub(server_error(what, resp.status, &resp.body), secrets);
    response_error(resp, now, detail.clone()).unwrap_or(Error::Endpoint(detail))
}

/// A request that got NO answer: the host transport's failure, typed by
/// [`ikigai_http::status::transport_error`] (a timeout is [`Error::Timeout`], anything else
/// [`Error::Unavailable`], both transient), with the transport's words scrubbed of every
/// credential as the detail. The URL is removed before the words are read, so a host whose name
/// says "timeout" cannot type its own refusals.
fn no_answer(error: &str, url: &str, what: &str, secrets: &[&str]) -> Error {
    let detail = scrub(format!("zoom {what} transport error: {error}"), secrets);
    transport_error(error, url, detail)
}

/// The attendee list: addresses separated by commas, semicolons or whitespace (so a newline-
/// separated list, the map convention, is one too). Each must be an address [`is_email`] accepts;
/// a repeat (ignoring ASCII case) is dropped, keeping the first spelling. `None` when any item is
/// not an address, or when nothing but separators was given.
fn parse_attendees(list: &str) -> Option<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for item in list
        .split(|c: char| c == ',' || c == ';' || c.is_whitespace())
        .filter(|s| !s.is_empty())
    {
        if !is_email(item) {
            return None;
        }
        if !out.iter().any(|seen| seen.eq_ignore_ascii_case(item)) {
            out.push(item.to_string());
        }
    }
    (!out.is_empty()).then_some(out)
}

/// An email address this module will send and emit as a `mailto:` IRI unchanged: a dot-atom
/// local part (at most 64 bytes) of ASCII letters, digits and `. _ - ~ ! $ ' * +` — the
/// characters RFC 6068 lets a `mailto:` carry raw — then `@` and a domain of letter, digit and
/// hyphen labels. Narrower than RFC 5322 on purpose: an address outside it would need
/// percent-encoding, and then the graph's IRI and the JSON's string would no longer name the same
/// address.
fn is_email(s: &str) -> bool {
    let Some((local, domain)) = s.split_once('@') else {
        return false;
    };
    let local_ok = !local.is_empty()
        && local.len() <= 64
        && !local.starts_with('.')
        && !local.ends_with('.')
        && !local.contains("..")
        && local
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-~!$'*+".contains(&b));
    let domain_ok = !domain.is_empty()
        && domain.len() <= 253
        && domain.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        });
    local_ok && domain_ok
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
/// never the URL: a URL is the caller's to configure, and a host is all the refusal needs.
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
    /// The addresses the meeting is for, as sent (already held to [`is_email`]).
    attendees: Vec<String>,
}

impl Meeting {
    /// The SHAREABLE graph — join-safe fields only. The meeting is an `ical:Vevent` whose
    /// `ical:conference` is the join URL (RFC 7986's CONFERENCE property, a URI; an ICS generator
    /// maps it to a `CONFERENCE:` line + the event's link), `dcterms:identifier` the provider's
    /// meeting id and `schema:provider` the provider. The host `start_url` is deliberately
    /// withheld. `ik:passcode` is the shared vocabulary's own term (`ikigai-vocab` 0.1.69,
    /// `rdfs:range xsd:string`, no `rdfs:domain` — its subject is `ical:Vevent`, an external
    /// class, and that vocabulary declares domains only for its own classes): the code a
    /// participant types has no well-known home, because RFC 7986 carries a passcode only
    /// inside a conference URI and Zoom's `pwd=` is an encrypted token, not the code. It is
    /// emitted as a string literal, never a number — leading zeros are significant. Each attendee
    /// is one `ical:attendee <mailto:address>` (RFC 5545's ATTENDEE, a CAL-ADDRESS), the same
    /// address the JSON face lists under `attendees`.
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
        for attendee in &self.attendees {
            s.push_str(&format!(
                "    ical:attendee {} ;\n",
                ttl_iri(&format!("mailto:{attendee}"))
            ));
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
            "attendees": self.attendees,
        }))
        .unwrap_or_default()
    }
}

fn repr(content_type: &str, body: Vec<u8>) -> Representation {
    Representation::new(ReprType::new(content_type), body)
}

/// The S2S token request's form body: `grant_type=account_credentials&account_id=…`,
/// `application/x-www-form-urlencoded`, so an account id holding `&`, `=`, `+` or a space is one
/// value, not several.
fn token_form(account_id: &str) -> Vec<u8> {
    url::form_urlencoded::Serializer::new(String::new())
        .append_pair("grant_type", "account_credentials")
        .append_pair("account_id", account_id)
        .finish()
        .into_bytes()
}

/// A Turtle string literal (quote-and-escape). A `"`-quoted Turtle string may not hold a raw `"`,
/// `\`, line feed or carriage return (Turtle grammar production 22, `STRING_LITERAL_QUOTE`), so
/// those four are escaped and everything else passes through: a topic with a line break is the
/// same term in this face and the JSON one.
fn ttl_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
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
        // Basic base64("cid-1:csec-1"), written out: the shared builder's own vectors pin the
        // encoder, and this pins that the oauth call sends the client id and secret through it.
        let want = "Basic Y2lkLTE6Y3NlYy0x";
        assert!(
            oauth
                .headers
                .iter()
                .any(|(k, v)| k == "Authorization" && v == want),
            "{:?}",
            oauth.headers
        );
        assert!(oauth.url.ends_with("/oauth/token"), "{}", oauth.url);
        assert!(!oauth.url.contains("acct-1"), "{}", oauth.url);
        assert_eq!(
            oauth.body,
            b"grant_type=account_credentials&account_id=acct-1".to_vec()
        );
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
    fn a_client_id_with_a_colon_is_refused_before_any_request_and_never_echoed() {
        // RFC 7617 §2: the first colon separates user id from password, so a client id holding
        // one cannot be encoded. The shared builder refuses it; nothing is sent, and the error
        // names the rule, never the id or the secret.
        struct ColonSecrets;
        impl SecretReader for ColonSecrets {
            fn read(&self, name: &str) -> Result<Vec<u8>> {
                Ok(match name {
                    "zoom-account-id" => b"acct-1".to_vec(),
                    "zoom-client-id" => b"cid:colon".to_vec(),
                    "zoom-client-secret" => b"csec-1".to_vec(),
                    other => return Err(Error::Endpoint(format!("no secret {other}"))),
                })
            }
        }
        let transport = Arc::new(FakeZoom::default());
        let k = Kernel::new(Arc::new(space(
            Arc::clone(&transport) as Arc<dyn HttpTransport>,
            Arc::new(ColonSecrets),
            ZoomConfig::default(),
        )));
        let req = Request::new(Verb::Sink, Iri::parse("urn:meeting:zoom:schedule").unwrap())
            .with_arg("topic", ArgRef::Inline(b"t".to_vec()))
            .with_arg("start", ArgRef::Inline(b"2026-07-30T22:00:00Z".to_vec()));
        let err = block_on(k.issue(req, &zoom_cap())).unwrap_err().to_string();
        assert!(err.contains("basic-user-id-colon"), "{err}");
        assert!(!err.contains("cid:colon"), "{err}");
        assert!(!err.contains("csec-1"), "{err}");
        assert!(transport.seen.lock().unwrap().is_empty());
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

    /// One scripted answer: a status, its headers and a body.
    type Answer = (u16, Vec<(String, String)>, &'static str);

    /// A transport whose token endpoint answers one scripted reply (an [`Answer`], or a
    /// transport failure), so each row of the shared status table can be provoked.
    struct Scripted(std::result::Result<Answer, String>);

    #[async_trait]
    impl HttpTransport for Scripted {
        async fn send(&self, _: HttpRequest) -> std::result::Result<HttpResponse, String> {
            let (status, headers, body) = self.0.clone()?;
            Ok(HttpResponse {
                status,
                headers,
                body: body.as_bytes().to_vec(),
            })
        }
    }

    /// Schedule through `transport`, with a clock at `now_millis` when given, and hand back
    /// the error.
    fn fails_with(transport: Scripted, now_millis: Option<u64>) -> Error {
        struct At(u64);
        impl ikigai_core::Clock for At {
            fn now(&self) -> ikigai_core::Time {
                ikigai_core::Time::from_millis(self.0)
            }
        }
        let mut k = Kernel::new(Arc::new(space(
            Arc::new(transport),
            Arc::new(FakeSecrets),
            ZoomConfig::default(),
        )));
        if let Some(now) = now_millis {
            k = k.with_clock(Arc::new(At(now)));
        }
        let req = Request::new(Verb::Sink, Iri::parse("urn:meeting:zoom:schedule").unwrap())
            .with_arg("topic", ArgRef::Inline(b"t".to_vec()))
            .with_arg("start", ArgRef::Inline(b"2026-07-30T22:00:00Z".to_vec()));
        block_on(k.issue(req, &zoom_cap())).unwrap_err()
    }

    /// The classes the Sink declares, as the manifold states them.
    fn declared() -> ikigai_core::ActionSpec {
        let k = kernel(Arc::new(FakeZoom::default()));
        let specs = k
            .describe_pattern("urn:meeting:zoom:schedule")
            .unwrap()
            .action_specs();
        assert_eq!(specs.len(), 1);
        specs.into_iter().next().unwrap()
    }

    /// Zoom's error answers are typed by ikigai-http's one status table (ledger #1546), not by
    /// a private `>= 400 -> Endpoint` rule: a `401`/`403` is `Refused` (the class whose one
    /// retry is the credential holder's), a `429` is `RetryAfter` carrying the header's delay,
    /// a `500` stays the opaque `Endpoint`, a `503` and a `502` are transient, a `413` is
    /// `OverBudget`. The detail keeps this module's words and the body's message, scrubbed, and
    /// the Sink's declaration admits every class seen.
    #[test]
    fn zoom_answers_are_typed_by_the_shared_status_table() {
        let spec = declared();
        let header = |name: &str, value: &str| vec![(name.to_string(), value.to_string())];
        let message = r#"{"message":"account acct-1 said no"}"#;
        // Each case: the answer, the class it must have, and how its display must begin.
        let cases: Vec<(Answer, FailureClass, &str)> = vec![
            (
                (401, vec![], message),
                FailureClass::Refused,
                "refused: zoom oauth returned 401:",
            ),
            (
                (403, vec![], message),
                FailureClass::Refused,
                "refused: zoom oauth returned 403:",
            ),
            (
                (429, header("Retry-After", "30"), message),
                FailureClass::RetryAfter,
                "retry after 30000 ms: rate-limited: zoom oauth returned 429:",
            ),
            (
                (429, vec![], message),
                FailureClass::RetryAfter,
                "retry later: rate-limited: zoom oauth returned 429:",
            ),
            (
                (500, vec![], message),
                FailureClass::Unknown,
                "zoom oauth returned 500:",
            ),
            (
                (502, vec![], message),
                FailureClass::Transient,
                "zoom oauth returned 502:",
            ),
            (
                (503, vec![], message),
                FailureClass::Transient,
                "zoom oauth returned 503:",
            ),
            (
                (503, header("Retry-After", "5"), message),
                FailureClass::RetryAfter,
                "retry after 5000 ms: zoom oauth returned 503:",
            ),
            (
                (413, vec![], message),
                FailureClass::OverBudget,
                "zoom oauth returned 413:",
            ),
            (
                (404, vec![], message),
                FailureClass::Permanent,
                "zoom oauth returned 404:",
            ),
        ];
        for (answer, class, prefix) in cases {
            let status = answer.0;
            let err = fails_with(Scripted(Ok(answer)), None);
            assert_eq!(err.class(), class, "{status}: {err:?}");
            let text = err.to_string();
            assert!(text.contains(prefix), "{status}: {text}");
            assert!(
                text.contains("account [redacted] said no"),
                "{status}: the body's message is kept, the credential in it scrubbed: {text}"
            );
            assert_eq!(
                spec.admits_failure(class),
                Some(true),
                "{status}: the manifold admits `{class:?}`"
            );
        }
        let refused = fails_with(Scripted(Ok((401, vec![], message))), None);
        assert!(matches!(refused, Error::Refused(_)), "{refused:?}");
        assert!(
            !matches!(refused, Error::Denied(_)),
            "Denied is the floor's word"
        );

        // A `Retry-After` HTTP-date is measured on the invocation's clock: 90 s ahead of it.
        let at = 1_445_412_480_000u64; // 2015-10-21T07:28:00Z
        let dated = fails_with(
            Scripted(Ok((
                429,
                header("Retry-After", "Wed, 21 Oct 2015 07:28:00 GMT"),
                message,
            ))),
            Some(at - 90_000),
        );
        assert_eq!(dated.retry_after_millis(), Some(90_000), "{dated}");
        // With no clock the date cannot be measured: a rate limit with no delay.
        let undated = fails_with(
            Scripted(Ok((
                429,
                header("Retry-After", "Wed, 21 Oct 2015 07:28:00 GMT"),
                message,
            ))),
            None,
        );
        assert!(
            matches!(undated, Error::RetryAfter { delay: None, .. }),
            "{undated:?}"
        );
    }

    /// A request that got no answer is transient (ledger #1546): a transport that timed out is
    /// a `Timeout`, any other failure an `Unavailable`, so the reliability overlays engage in
    /// front of this module. The transport's words are kept as the detail, scrubbed of every
    /// credential (here the account id the form body carries). Both classes are declared.
    #[test]
    fn a_transport_failure_is_transient_and_its_text_scrubbed() {
        let spec = declared();
        let slow = fails_with(
            Scripted(Err(
                "operation timed out sending account_id=acct-1".to_string()
            )),
            None,
        );
        assert!(matches!(slow, Error::Timeout(_)), "{slow:?}");
        let down = fails_with(
            Scripted(Err("connection refused (account_id=acct-1)".to_string())),
            None,
        );
        assert!(matches!(down, Error::Unavailable(_)), "{down:?}");
        for err in [slow, down] {
            assert_eq!(err.class(), FailureClass::Transient);
            assert!(err.is_transient());
            assert_eq!(spec.admits_failure(err.class()), Some(true));
            let text = err.to_string();
            assert!(text.contains("zoom oauth transport error"), "{text}");
            assert!(
                text.contains("[redacted]") && !text.contains("acct-1"),
                "{text}"
            );
        }
    }

    /// The manifold states how a schedule can fail: the five classes the status table and the
    /// transport can answer, with `permanent` and (beside the two `requires`) `denied` implied.
    #[test]
    fn the_sink_declares_its_failure_classes() {
        let spec = declared();
        assert_eq!(
            spec.failures.classes(),
            Some(
                [
                    FailureClass::Transient,
                    FailureClass::RetryAfter,
                    FailureClass::OverBudget,
                    FailureClass::Refused,
                    FailureClass::Unknown,
                ]
                .as_slice()
            )
        );
        assert_eq!(spec.admits_failure(FailureClass::Permanent), Some(true));
        assert_eq!(spec.admits_failure(FailureClass::Denied), Some(true));
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
    fn attendee_lists_split_on_commas_semicolons_and_whitespace_and_drop_repeats() {
        assert_eq!(
            parse_attendees("a@example.invalid, b@example.invalid;c@example.invalid\nd@example.invalid  e@example.invalid"),
            Some(
                [
                    "a@example.invalid",
                    "b@example.invalid",
                    "c@example.invalid",
                    "d@example.invalid",
                    "e@example.invalid"
                ]
                .map(str::to_string)
                .to_vec()
            )
        );
        assert_eq!(
            parse_attendees("Pat@Example.invalid,pat@example.INVALID, ,"),
            Some(vec!["Pat@Example.invalid".to_string()]),
            "a repeat in another case is the same address; empty items are separators"
        );
        assert_eq!(parse_attendees(" , ; "), None, "nothing but separators");
        assert_eq!(
            parse_attendees("a@example.invalid, not-an-address"),
            None,
            "one bad item refuses the list"
        );
    }

    #[test]
    fn is_email_accepts_what_a_mailto_carries_raw() {
        for ok in [
            "pat@example.invalid",
            "pat.o'neil+zoom@mail.example.invalid",
            "a_b-c~d!e$f*g@x-1.example.invalid",
            "pat@localhost",
        ] {
            assert!(is_email(ok), "{ok}");
        }
        for bad in [
            "",
            "pat",
            "@example.invalid",
            "pat@",
            "pat@@example.invalid",
            "pat@example..invalid",
            "pat@-example.invalid",
            ".pat@example.invalid",
            "pat.@example.invalid",
            "pa..t@example.invalid",
            "pat smith@example.invalid",
            "pat%40@example.invalid",
            "pat?cc=x@example.invalid",
            "pat#x@example.invalid",
            "<pat@example.invalid>",
            "pat@example.invalid>",
            "pat@exämple.invalid",
        ] {
            assert!(!is_email(bad), "{bad:?}");
        }
        assert!(!is_email(&format!("{}@example.invalid", "a".repeat(65))));
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
    fn token_form_encodes_the_account_id_as_one_value() {
        assert_eq!(
            token_form("acct-1"),
            b"grant_type=account_credentials&account_id=acct-1".to_vec()
        );
        let awkward = "a&b=c d+e";
        let body = token_form(awkward);
        let pairs: Vec<(String, String)> =
            url::form_urlencoded::parse(&body).into_owned().collect();
        assert_eq!(
            pairs,
            [
                ("grant_type".to_string(), "account_credentials".to_string()),
                ("account_id".to_string(), awkward.to_string()),
            ]
        );
    }

    #[test]
    fn ttl_str_escapes_what_a_quoted_turtle_string_cannot_carry() {
        assert_eq!(ttl_str("plain"), "\"plain\"");
        assert_eq!(
            ttl_str("say \"hi\" \\ a\nb\rc\td"),
            "\"say \\\"hi\\\" \\\\ a\\nb\\rc\td\""
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
}
