//! `ikigai-meeting` — video-conference scheduling as ikigai ROC resources.
//!
//! `urn:meeting:schedule` (a provider-agnostic facade) dispatches to pluggable backends
//! `urn:meeting:<provider>:schedule`, the same shape as `urn:llm:ask`. Slice 0 ships a **Zoom**
//! backend over Server-to-Server OAuth. The scheduled meeting's join URL is emitted as
//! `ical:conference` (RFC 7986's CONFERENCE property), so ONE value flows into the `.ics`, the
//! calendar event, and — later — an org entry, without re-plumbing at each destination.
//!
//! ## A meeting is a resource
//!
//! `urn:meeting:id:{m}` is one meeting: Source reads it, Sink moves or changes it (only what is
//! given is sent), Delete cancels it. `{m}` names the provider and its id (`zoom-88899900011`),
//! so the name stays provider-neutral. Every schedule makes one meeting, never a provider's
//! recurring series. The settings (waiting room, joining before the host, authenticated join,
//! auto-recording) are arguments with stated defaults, never hard-coded. `teacher=<t>` acts in
//! that teacher's account, from their record in the keystore. The host link (`start_url`) is
//! sealed to the presenter: [`CAP_HOST_MEETING`] or [`CAP_HOST_TEACHER`].
//! Only the meeting's host's account reaches it: an update, a cancel, or a read naming a
//! teacher is [`Error::Denied`] unless the Zoom user the account acts for is the meeting's
//! `host_id`, so one teacher's grants never reach another teacher's meeting.
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
use chrono::{DateTime, NaiveDateTime, SecondsFormat, TimeZone, Utc};
use ikigai_core::{
    ActionSpec, ArgSpec, Description, Endpoint, EndpointSpace, Error, Exact, FailureClass,
    Invocation, ReprType, Representation, Result, Time, UriTemplate, Verb,
};
use ikigai_http::status::{response_error, transport_error};
use ikigai_http::{HttpBody, HttpRequest, HttpResponse, HttpTransport, Method};
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

/// Stable description ids and catalog labels for the two scheduling doors.
const ZOOM_ID: &str = "meeting-zoom-schedule";
const FACADE_ID: &str = "meeting-schedule";
/// The description id and catalog label of `urn:meeting:id:{m}`.
const MEETING_ID: &str = "meeting-id";

/// One meeting as a resource. `{m}` names the provider and its id: `zoom-88899900011`.
pub const MEETING_TEMPLATE: &str = "urn:meeting:id:{m}";
const MEETING_PREFIX: &str = "urn:meeting:id:";

/// The presenter's grant for one meeting's host link: `urn:cap:meeting:host:id:{m}`.
pub const CAP_HOST_MEETING: &str = "urn:cap:meeting:host:id:";
/// The presenter's grant for the host links of every meeting in one teacher's account:
/// `urn:cap:meeting:host:teacher:{t}`.
pub const CAP_HOST_TEACHER: &str = "urn:cap:meeting:host:teacher:";

/// The meeting length when `duration` is not given, in minutes.
pub const DEFAULT_MINUTES: u32 = 30;

/// How long before its stated expiry a cached token stops being used, in seconds. A token whose
/// `expires_in` is no longer than this is used once and not cached.
pub const TOKEN_REFRESH_MARGIN_SECS: u64 = 60;

const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";
const XSD_DATETIME: &str = "http://www.w3.org/2001/XMLSchema#dateTime";
const XSD_INTEGER: &str = "http://www.w3.org/2001/XMLSchema#integer";
const XSD_BOOLEAN: &str = "http://www.w3.org/2001/XMLSchema#boolean";

/// What a credential value becomes in any error text this module composes from foreign text.
const REDACTED: &str = "[redacted]";

/// The most of a Zoom answer this module reads: 1 MiB. ikigai-http 0.4 hands the body over
/// unread (an [`HttpBody`] the host may stream), so the reader sets the bound. A token or a
/// created meeting is a few hundred bytes of JSON; a body past this is answered
/// [`Error::OverBudget`] (a class the Sink declares), never buffered whole.
const MAX_ZOOM_BODY: usize = 1024 * 1024;

/// A read-only secret resolver. The host resolves `urn:secret:<name>` under a capability and hands
/// the bytes in — the same injection discipline the [`HttpTransport`] gets. Sync because a keystore
/// `get` is a fast blocking call (mirrors ikigai-secret's own `Backend`).
pub trait SecretReader: Send + Sync {
    /// The secret's bytes, or `Error::Denied`/`Error::Endpoint` if the host refuses or can't read.
    fn read(&self, name: &str) -> Result<Vec<u8>>;

    /// Replace a secret's bytes. Only a teacher record for a user-level OAuth app is ever
    /// stored: Zoom answers each refresh with a new refresh token, and the next refresh must
    /// use it. The module checks `urn:cap:secret:write:<name>` before it asks. The default
    /// refuses with [`Error::Unsupported`], so a read-only keystore keeps working for every
    /// Server-to-Server account.
    fn store(&self, name: &str, value: &[u8]) -> Result<()> {
        let _ = value;
        Err(Error::Unsupported {
            verb: None,
            detail: format!("this keystore cannot store secret `{name}`"),
        })
    }
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
    /// The prefix of each teacher's record in the keystore: teacher `t`'s record is the
    /// secret `{teacher_secret_prefix}{t}` (`zoom-teacher-t` by default). See [`space`].
    pub teacher_secret_prefix: String,
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
            teacher_secret_prefix: "zoom-teacher-".to_string(),
        }
    }
}

impl ZoomConfig {
    /// The three shared Server-to-Server credential names, in the order they are read.
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

    /// The create-meeting endpoint for a user: a path segment, already encoded.
    fn meetings_url(&self, user: &str) -> String {
        format!(
            "{}/users/{user}/meetings",
            self.api_base.trim_end_matches('/')
        )
    }

    /// One user's endpoint, for the Zoom user id an account acts for: a path segment, already
    /// encoded, or `me`.
    fn user_url(&self, user: &str) -> String {
        format!("{}/users/{user}", self.api_base.trim_end_matches('/'))
    }

    /// One meeting's endpoint (read, update, cancel), by Zoom's numeric id.
    fn meeting_url(&self, id: &str) -> String {
        format!("{}/meetings/{id}", self.api_base.trim_end_matches('/'))
    }

    /// The keystore name of teacher `t`'s record.
    fn teacher_secret(&self, teacher: &str) -> String {
        format!("{}{teacher}", self.teacher_secret_prefix)
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
///         Ok(HttpResponse { status: self.0, headers: self.1.clone(), body: self.2.as_bytes().to_vec().into() })
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
    let zoom = Arc::new(Zoom {
        transport,
        secrets,
        config,
        tokens: Arc::default(),
        refreshing: RefreshLocks::default(),
    });
    EndpointSpace::new()
        .bind(
            Exact::new("urn:meeting:zoom:schedule"),
            ZoomBackend {
                id: ZOOM_ID,
                zoom: zoom.clone(),
            },
        )
        .bind(
            Exact::new("urn:meeting:schedule"),
            ZoomBackend {
                id: FACADE_ID,
                zoom: zoom.clone(),
            },
        )
        .bind(
            UriTemplate::parse(MEETING_TEMPLATE).expect("a valid template"),
            MeetingById { zoom },
        )
}

/// The Zoom backend's scheduling door: exchange the credentials for a bearer token, then create
/// a scheduled meeting. Cloneable; every door of one [`space`] shares one configuration and one
/// token cache.
#[derive(Clone)]
pub struct ZoomBackend {
    id: &'static str,
    zoom: Arc<Zoom>,
}

/// `urn:meeting:id:{m}`: one meeting as a resource. Source reads it, Sink updates it, Delete
/// cancels it. `{m}` is provider-neutral and names its provider: `zoom-<Zoom's meeting id>`.
#[derive(Clone)]
pub struct MeetingById {
    zoom: Arc<Zoom>,
}

/// What every door shares: the injected transport and keystore, the configuration, the cache.
struct Zoom {
    transport: Arc<dyn HttpTransport>,
    secrets: Arc<dyn SecretReader>,
    config: ZoomConfig,
    tokens: Arc<TokenCache>,
    refreshing: RefreshLocks,
}

/// One async lock per teacher record with its own app. Zoom rotates the refresh token on every
/// refresh and refuses the old one, so two refreshes for one teacher must not both start from
/// the same token: the second waits, then starts from the record the first stored.
#[derive(Default)]
struct RefreshLocks(Mutex<HashMap<String, Arc<futures_util::lock::Mutex<()>>>>);

impl RefreshLocks {
    fn for_record(&self, record: &str) -> Arc<futures_util::lock::Mutex<()>> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(record.to_string())
            .or_default()
            .clone()
    }
}

/// One meeting as the caller asked for it — the validated inputs.
struct MeetingRequest {
    topic: String,
    start: String,
    duration: u32,
    timezone: Option<String>,
    agenda: Option<String>,
    attendees: Vec<String>,
    settings: Settings,
}

/// The meeting settings a caller may choose. `None` is "not given": a new meeting takes the
/// default ([`Settings::or_defaults`]); an update leaves Zoom's value as it is.
#[derive(Clone, Default, Debug, PartialEq)]
struct Settings {
    waiting_room: Option<bool>,
    join_before_host: Option<bool>,
    authenticated_join: Option<bool>,
    auto_recording: Option<String>,
}

/// The values `auto_recording` takes, Zoom's own words; the first is the default.
pub const AUTO_RECORDING: [&str; 3] = ["none", "local", "cloud"];

impl Settings {
    /// The settings given as arguments, each held to its type. There is no `captions`: Zoom's
    /// meeting API has no per-meeting captions field (captions are an account or user setting),
    /// so an argument for them could never be honored.
    fn parse(inv: &Invocation<'_>) -> Result<Settings> {
        let auto_recording = match optional(inv, "auto_recording")? {
            None => None,
            Some(value) if AUTO_RECORDING.contains(&value.as_str()) => Some(value),
            Some(_) => return Err(invalid("auto_recording", "not one of none, local, cloud")),
        };
        Ok(Settings {
            waiting_room: boolean(inv, "waiting_room")?,
            join_before_host: boolean(inv, "join_before_host")?,
            authenticated_join: boolean(inv, "authenticated_join")?,
            auto_recording,
        })
    }

    /// A new meeting's settings: the waiting room on, joining before the host off, any Zoom
    /// user may join, and no recording, unless the caller chose otherwise.
    fn or_defaults(self) -> Settings {
        Settings {
            waiting_room: Some(self.waiting_room.unwrap_or(true)),
            join_before_host: Some(self.join_before_host.unwrap_or(false)),
            authenticated_join: Some(self.authenticated_join.unwrap_or(false)),
            auto_recording: Some(
                self.auto_recording
                    .unwrap_or_else(|| AUTO_RECORDING[0].to_string()),
            ),
        }
    }

    /// Zoom's `settings` object: the given settings and nothing else.
    fn json(&self) -> serde_json::Map<String, Value> {
        let mut out = serde_json::Map::new();
        if let Some(on) = self.waiting_room {
            out.insert("waiting_room".into(), json!(on));
        }
        if let Some(on) = self.join_before_host {
            out.insert("join_before_host".into(), json!(on));
        }
        if let Some(on) = self.authenticated_join {
            out.insert("meeting_authentication".into(), json!(on));
        }
        if let Some(mode) = &self.auto_recording {
            out.insert("auto_recording".into(), json!(mode));
        }
        out
    }
}

/// Which token a cached entry is: the endpoint that minted it and the account and client it was
/// minted for. The client secret is not part of it: a token minted before a rotation is still the
/// account's token, and if Zoom refuses it the `401` path drops it. A teacher's own app is keyed
/// by its record's name in place of an account id.
#[derive(Clone, PartialEq, Eq, Hash)]
struct TokenKey {
    token_url: String,
    account_id: String,
    client_id: String,
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

/// The three shared S2S credentials, read from the keystore.
struct Credentials {
    account_id: String,
    client_id: String,
    client_secret: String,
}

/// A teacher's own user-level OAuth app, from their record. The refresh token changes when Zoom
/// rotates it, so it sits behind a lock: a second refresh in one call uses the new one.
struct OwnApp {
    record: String,
    fields: serde_json::Map<String, Value>,
    client_id: String,
    client_secret: String,
    refresh_token: Mutex<String>,
}

/// How an account's token is obtained.
enum Grant {
    /// The shared Server-to-Server app, acting for a user in its account.
    Shared(Credentials),
    /// A teacher's own user-level OAuth app.
    Own(OwnApp),
}

/// The account a call acts in, the API URL it calls and the user it acts for, all already
/// authorized.
struct Account {
    grant: Grant,
    url: String,
    /// `GET` this for the Zoom user the account acts for: its `id` must be a meeting's
    /// `host_id` before the meeting is changed, canceled or its host details read.
    user_url: String,
    /// Who the call acts for, in a refusal's words: the teacher, or the configured user.
    acting: String,
}

impl Account {
    fn key(&self, token_url: &str) -> TokenKey {
        match &self.grant {
            Grant::Shared(creds) => TokenKey {
                token_url: token_url.to_string(),
                account_id: creds.account_id.clone(),
                client_id: creds.client_id.clone(),
            },
            Grant::Own(app) => TokenKey {
                token_url: token_url.to_string(),
                account_id: format!("record:{}", app.record),
                client_id: app.client_id.clone(),
            },
        }
    }

    /// Every credential value, for [`scrub`].
    fn secrets(&self) -> Vec<String> {
        match &self.grant {
            Grant::Shared(c) => vec![
                c.account_id.clone(),
                c.client_id.clone(),
                c.client_secret.clone(),
            ],
            Grant::Own(app) => vec![
                app.client_id.clone(),
                app.client_secret.clone(),
                app.refresh_token
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone(),
            ],
        }
    }
}

/// What a call reaches: the create-meeting endpoint of the account's user, or one meeting.
enum Target<'a> {
    Create,
    Meeting(&'a str),
}

/// One call against a meeting.
///
/// Every call on an existing meeting except a read without a teacher first proves the account
/// hosts it: the account's user `id` must be the meeting's `host_id`. Otherwise one teacher's
/// grants would reach another teacher's meeting through the shared account's token.
enum Call<'a> {
    Create(&'a MeetingRequest),
    /// A read; `host_only` when a teacher is named, so their host link is theirs.
    Get {
        host_only: bool,
    },
    Update(&'a Value),
    Cancel,
}

impl Call<'_> {
    fn what(&self) -> &'static str {
        match self {
            Call::Create(_) => "create-meeting",
            Call::Get { .. } => "get-meeting",
            Call::Update(_) => "update-meeting",
            Call::Cancel => "cancel-meeting",
        }
    }
}

/// What a call came back with: the meeting, a cancelation, or a `401` for a token the cache
/// supplied, which the caller answers by minting a fresh one and trying once more.
enum Outcome {
    Meeting(Meeting),
    Canceled,
    TokenRefused,
}

impl Outcome {
    fn into_meeting(self) -> Result<Meeting> {
        match self {
            Outcome::Meeting(meeting) => Ok(meeting),
            _ => Err(Error::Endpoint(
                "zoom: the call answered no meeting".to_string(),
            )),
        }
    }
}

#[async_trait]
impl Endpoint for ZoomBackend {
    async fn invoke(&self, inv: &Invocation<'_>) -> Result<Representation> {
        // Inputs are held to their declared classes: a value outside the class is a typed
        // `InvalidArgument` that names the input and never echoes the value.
        let topic = required(inv, "topic")?;
        let start = required(inv, "start")?.trim().to_string();
        let (timezone, zone) = timezone_arg(inv)?;
        let start = start_value(&start, zone)?;
        let duration = duration_arg(inv)?.unwrap_or(DEFAULT_MINUTES);
        let attendees = attendees_arg(inv)?.unwrap_or_default();
        // Pipeline citizenship: a named `agenda` wins; the piped `content` is the fallback.
        let agenda = optional(inv, "agenda")?;
        let content = optional(inv, "content")?;
        let agenda = agenda.or(content);
        let face = face_arg(inv)?;
        let settings = Settings::parse(inv)?.or_defaults();
        let teacher = teacher_arg(inv)?;

        // Authority first: a typed `Denied` before any secret is read or any socket opens.
        let account = self.zoom.account(inv, teacher.as_deref(), Target::Create)?;
        let request = MeetingRequest {
            topic,
            start,
            duration,
            timezone,
            agenda,
            attendees,
            settings,
        };
        let meeting = self
            .zoom
            .run(inv, &account, &Call::Create(&request))
            .await?
            .into_meeting()?;
        let host = may_host(inv, &meeting, teacher.as_deref());
        Ok(meeting.face(&face, host))
    }

    // Each door has its own published id; the catalog and description must agree.
    fn name(&self) -> &str {
        self.id
    }

    fn describe(&self) -> Description {
        let mut description = Description::new(self.id)
            .summary(
                "Schedule one Zoom meeting (never a recurring series). Returns the join URL \
                 (text), the shareable graph (as=text/turtle — ical:conference), or the full \
                 envelope (as=application/json; the host start URL only for the presenter). \
                 A piped value is the agenda.",
            )
            .verb(Verb::Sink)
            .requires(CAP_NET)
            .requires(CAP_SECRET_READ)
            .input(
                ArgSpec::new("topic")
                    .summary("the meeting title")
                    .class(XSD_STRING),
            )
            .input(start_spec())
            .input(
                duration_spec()
                    .default_value(DEFAULT_MINUTES.to_string())
                    .optional(),
            )
            .input(timezone_spec())
            .input(attendees_spec())
            .input(agenda_spec())
            .input(content_spec());
        for spec in setting_specs(true) {
            description = description.input(spec);
        }
        description
            .input(teacher_spec())
            .input(face_spec())
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

#[async_trait]
impl Endpoint for MeetingById {
    async fn invoke(&self, inv: &Invocation<'_>) -> Result<Representation> {
        let m = meeting_name(inv)?;
        match inv.request.verb {
            Verb::Source => {
                let face = face_arg(inv)?;
                let teacher = teacher_arg(inv)?;
                let account = self
                    .zoom
                    .account(inv, teacher.as_deref(), Target::Meeting(&m))?;
                let meeting = self
                    .zoom
                    .run(
                        inv,
                        &account,
                        &Call::Get {
                            host_only: teacher.is_some(),
                        },
                    )
                    .await?
                    .into_meeting()?;
                let host = may_host(inv, &meeting, teacher.as_deref());
                Ok(meeting.face(&face, host))
            }
            Verb::Sink => {
                let topic = match inv.inline_str("topic") {
                    Ok(topic) if topic.trim().is_empty() => {
                        return Err(invalid("topic", "empty; a meeting needs a title"))
                    }
                    Ok(topic) => Some(topic.to_string()),
                    Err(Error::MissingArgument(_)) => None,
                    Err(_) => return Err(invalid("topic", "expected an inline UTF-8 value")),
                };
                let start = optional(inv, "start")?;
                let (timezone, zone) = timezone_arg(inv)?;
                let start = start.map(|s| start_value(&s, zone)).transpose()?;
                let duration = duration_arg(inv)?;
                let attendees = attendees_arg(inv)?;
                let agenda = optional(inv, "agenda")?;
                let content = optional(inv, "content")?;
                let agenda = agenda.or(content);
                let face = face_arg(inv)?;
                let settings = Settings::parse(inv)?;
                let teacher = teacher_arg(inv)?;

                // What changed, and nothing else: an update never resets a setting it was not
                // given, and never makes the meeting anything but one scheduled meeting.
                let mut body = serde_json::Map::new();
                if let Some(topic) = topic {
                    body.insert("topic".into(), json!(topic));
                }
                if let Some(start) = start {
                    body.insert("start_time".into(), json!(start));
                }
                if let Some(duration) = duration {
                    body.insert("duration".into(), json!(duration));
                }
                if let Some(timezone) = timezone {
                    body.insert("timezone".into(), json!(timezone));
                }
                if let Some(agenda) = agenda {
                    body.insert("agenda".into(), json!(agenda));
                }
                let mut zoom_settings = settings.json();
                if let Some(attendees) = attendees {
                    zoom_settings.insert("meeting_invitees".into(), invitees(&attendees));
                }
                if !zoom_settings.is_empty() {
                    body.insert("settings".into(), Value::Object(zoom_settings));
                }
                if body.is_empty() {
                    return Err(Error::MissingArgument(
                        "something to change: topic, start, duration, timezone, agenda, \
                         attendees, waiting_room, join_before_host, authenticated_join or \
                         auto_recording"
                            .to_string(),
                    ));
                }
                let body = Value::Object(body);
                let account = self
                    .zoom
                    .account(inv, teacher.as_deref(), Target::Meeting(&m))?;
                let meeting = self
                    .zoom
                    .run(inv, &account, &Call::Update(&body))
                    .await?
                    .into_meeting()?;
                let host = may_host(inv, &meeting, teacher.as_deref());
                Ok(meeting.face(&face, host))
            }
            Verb::Delete => {
                let teacher = teacher_arg(inv)?;
                let account = self
                    .zoom
                    .account(inv, teacher.as_deref(), Target::Meeting(&m))?;
                self.zoom.run(inv, &account, &Call::Cancel).await?;
                Ok(repr(
                    "text/plain",
                    format!("Canceled meeting {MEETING_PREFIX}zoom-{m}.").into_bytes(),
                ))
            }
            verb => Err(Error::Unsupported {
                verb: Some(verb),
                detail: "a meeting is read (Source), updated (Sink) or canceled (Delete)"
                    .to_string(),
            }),
        }
    }

    fn name(&self) -> &str {
        MEETING_ID
    }

    fn describe(&self) -> Description {
        let binding = || {
            ArgSpec::new("m")
                .summary("the meeting: its provider and that provider's id, as zoom-<digits>")
                .class(XSD_STRING)
                .binding()
        };
        let fails = |action: ActionSpec| {
            action
                .requires(CAP_NET)
                .requires(CAP_SECRET_READ)
                .fails(FailureClass::Transient)
                .fails(FailureClass::RetryAfter)
                .fails(FailureClass::OverBudget)
                .fails(FailureClass::Refused)
                .fails(FailureClass::Unknown)
        };
        let mut update = ActionSpec::new(Verb::Sink)
            .summary(
                "Move or change the meeting: only what is given is sent to Zoom, and the answer \
                 is the meeting Zoom then holds. Give at least one change. Only the meeting's \
                 host's account can.",
            )
            .input(binding())
            .input(
                ArgSpec::new("topic")
                    .summary("the new title")
                    .class(XSD_STRING)
                    .optional(),
            )
            .input(start_spec().optional())
            .input(duration_spec().optional())
            .input(timezone_spec())
            .input(attendees_spec())
            .input(agenda_spec())
            .input(content_spec());
        for spec in setting_specs(false) {
            update = update.input(spec);
        }
        let update = update
            .input(teacher_spec())
            .input(face_spec())
            .output(FACES[0])
            .output(FACES[1])
            .output(FACES[2]);
        Description::new(MEETING_ID)
            .summary(
                "One meeting as a resource: read its details, move or change it, or cancel it. \
                 The host start URL is only for the presenter.",
            )
            .action(fails(
                ActionSpec::new(Verb::Source)
                    .summary(
                        "The meeting's details: the join URL (text), the shareable graph \
                         (as=text/turtle) or the full envelope (as=application/json).",
                    )
                    .input(binding())
                    .input(teacher_spec())
                    .input(face_spec())
                    .output(FACES[0])
                    .output(FACES[1])
                    .output(FACES[2]),
            ))
            .action(fails(update))
            .action(fails(
                ActionSpec::new(Verb::Delete)
                    .summary(
                        "Cancel the meeting. Only its host's account can; a canceled meeting is \
                         then not found.",
                    )
                    .input(binding())
                    .input(teacher_spec())
                    .output(FACES[0]),
            ))
    }
}

impl Zoom {
    /// The account a call acts in, with every rule checked before a socket opens: the token
    /// endpoint's and the call's hosts must be granted (port-aware), and every keystore name the
    /// call reads must be readable. With no teacher named, the shared Server-to-Server account
    /// is used and every refusal precedes every secret read. With a teacher named, their record
    /// is read first (under its own grant, `urn:cap:secret:read:<record>`), and what it selects
    /// is checked before any other secret is read: the shared credentials for a teacher acting
    /// through the shared account, or `urn:cap:secret:write:<record>` for a teacher's own app,
    /// whose rotated refresh token must be kept.
    fn account(
        &self,
        inv: &Invocation<'_>,
        teacher: Option<&str>,
        target: Target<'_>,
    ) -> Result<Account> {
        let url_for = |user: &str| match target {
            Target::Create => self.config.meetings_url(user),
            Target::Meeting(id) => self.config.meeting_url(id),
        };
        gate_net(inv, &self.config.token_url())?;
        let Some(teacher) = teacher else {
            let url = url_for(&self.config.user_id);
            let user_url = self.config.user_url(&self.config.user_id);
            gate_net(inv, &url)?;
            gate_net(inv, &user_url)?;
            self.gate_shared(inv)?;
            return Ok(Account {
                grant: Grant::Shared(self.credentials()?),
                url,
                user_url,
                acting: format!("the configured Zoom user `{}`", self.config.user_id),
            });
        };
        let record = self.config.teacher_secret(teacher);
        let acting = format!("teacher `{teacher}`");
        gate_secret(inv, "read", &record)?;
        match self.teacher_record(&record)? {
            TeacherRecord::Shared { user_id } => {
                let user = path_segment(&user_id);
                let url = url_for(&user);
                let user_url = self.config.user_url(&user);
                gate_net(inv, &url)?;
                gate_net(inv, &user_url)?;
                self.gate_shared(inv)?;
                Ok(Account {
                    grant: Grant::Shared(self.credentials()?),
                    url,
                    user_url,
                    acting,
                })
            }
            TeacherRecord::Own(app) => {
                let url = url_for("me");
                let user_url = self.config.user_url("me");
                gate_net(inv, &url)?;
                gate_net(inv, &user_url)?;
                gate_secret(inv, "write", &record)?;
                Ok(Account {
                    grant: Grant::Own(app),
                    url,
                    user_url,
                    acting,
                })
            }
        }
    }

    /// Every shared credential name must be readable.
    fn gate_shared(&self, inv: &Invocation<'_>) -> Result<()> {
        for name in self.config.secret_names() {
            gate_secret(inv, "read", name)?;
        }
        Ok(())
    }

    /// The three shared credentials, each a trimmed UTF-8 string.
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

    /// A teacher's record: a JSON object naming the Zoom user the shared account acts for
    /// (`user_id`), or the teacher's own app (`client_id`, `client_secret`, `refresh_token`).
    /// The error names the record and the form it takes, never its contents.
    fn teacher_record(&self, name: &str) -> Result<TeacherRecord> {
        let malformed = || {
            Error::Endpoint(format!(
                "secret `{name}` is not a teacher record: store a JSON object with `user_id` \
                 (a Zoom user in the shared account), or with `client_id`, `client_secret` and \
                 `refresh_token` (the teacher's own Zoom app)"
            ))
        };
        let fields = match serde_json::from_slice::<Value>(&self.secrets.read(name)?) {
            Ok(Value::Object(fields)) => fields,
            _ => return Err(malformed()),
        };
        let text = |key: &str| {
            fields
                .get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_string)
        };
        if let Some(refresh_token) = text("refresh_token") {
            let (Some(client_id), Some(client_secret)) = (text("client_id"), text("client_secret"))
            else {
                return Err(malformed());
            };
            return Ok(TeacherRecord::Own(OwnApp {
                record: name.to_string(),
                client_id,
                client_secret,
                refresh_token: Mutex::new(refresh_token),
                fields,
            }));
        }
        match text("user_id") {
            Some(user_id) => Ok(TeacherRecord::Shared { user_id }),
            None => Err(malformed()),
        }
    }

    /// Run one call with the account's token: a cached one while the invocation's clock says
    /// it is still good, else a fresh one (kept when there is a clock to judge it by). A `401`
    /// for a cached token drops it and tries once more with a fresh one. The same clock
    /// measures a `Retry-After` given as an HTTP-date.
    async fn run(
        &self,
        inv: &Invocation<'_>,
        account: &Account,
        call: &Call<'_>,
    ) -> Result<Outcome> {
        let now = inv.now();
        let token_url = self.config.token_url();
        let key = account.key(&token_url);
        if let Some(token) = now.and_then(|now| self.tokens.get(&key, now.as_millis())) {
            match self.call(account, &token, call, true, now).await? {
                Outcome::TokenRefused => self.tokens.evict(&key, &token),
                done => return Ok(done),
            }
        }
        let token = self.fresh_token(&token_url, account, &key, now).await?;
        match self.call(account, &token, call, false, now).await? {
            // Only a call made with a cached token answers `TokenRefused`; after a fresh token
            // a `401` is the `Refused` it would otherwise have been.
            Outcome::TokenRefused => {
                Err(Error::Refused(format!("zoom {} returned 401", call.what())))
            }
            done => Ok(done),
        }
    }

    /// A token minted now, kept in the cache when the invocation has a clock to judge it by.
    async fn fresh_token(
        &self,
        token_url: &str,
        account: &Account,
        key: &TokenKey,
        now: Option<Time>,
    ) -> Result<String> {
        let (token, expires_in) = match &account.grant {
            Grant::Shared(creds) => {
                let answer = self
                    .token_exchange(
                        token_url,
                        &creds.client_id,
                        &creds.client_secret,
                        token_form(&creds.account_id),
                        &account.secrets(),
                        now,
                    )
                    .await?;
                (answer.access_token, answer.expires_in)
            }
            Grant::Own(app) => {
                // One refresh per teacher at a time. A call that waited finds the token the
                // other minted (when there is a clock to keep it by), or else starts from the
                // record as the other stored it, never from the refresh token Zoom just retired.
                let lock = self.refreshing.for_record(&app.record);
                let _held = lock.lock().await;
                if let Some(token) = now.and_then(|now| self.tokens.get(key, now.as_millis())) {
                    return Ok(token);
                }
                let latest = match self.teacher_record(&app.record)? {
                    TeacherRecord::Own(latest) => latest,
                    TeacherRecord::Shared { .. } => {
                        return Err(Error::Endpoint(format!(
                            "secret `{}` changed during the call from a teacher's own Zoom app \
                             to a shared-account user; try again",
                            app.record
                        )))
                    }
                };
                let answer = self.refresh(token_url, &latest, account, now).await?;
                *app.refresh_token.lock().unwrap_or_else(|e| e.into_inner()) = latest
                    .refresh_token
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                if let (Some(now), Some(expires_in)) = (now, answer.1) {
                    self.tokens
                        .put(key.clone(), &answer.0, now.as_millis(), expires_in);
                }
                return Ok(answer.0);
            }
        };
        if let (Some(now), Some(expires_in)) = (now, expires_in) {
            self.tokens
                .put(key.clone(), &token, now.as_millis(), expires_in);
        }
        Ok(token)
    }

    /// A teacher's own app: exchange the refresh token for an access token. Zoom answers a new
    /// refresh token, and the next refresh must use it, so the record is stored with it before
    /// the token is used. A keystore that cannot store it fails the call and says what to do.
    async fn refresh(
        &self,
        token_url: &str,
        app: &OwnApp,
        account: &Account,
        now: Option<Time>,
    ) -> Result<(String, Option<u64>)> {
        let current = app
            .refresh_token
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let form = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("grant_type", "refresh_token")
            .append_pair("refresh_token", &current)
            .finish()
            .into_bytes();
        let answer = self
            .token_exchange(
                token_url,
                &app.client_id,
                &app.client_secret,
                form,
                &[account.secrets(), vec![current.clone()]].concat(),
                now,
            )
            .await?;
        if let Some(rotated) = answer.refresh_token.filter(|r| *r != current) {
            let mut fields = app.fields.clone();
            fields.insert("refresh_token".into(), json!(rotated));
            let bytes = serde_json::to_vec(&Value::Object(fields)).unwrap_or_default();
            self.secrets.store(&app.record, &bytes).map_err(|_| {
                Error::Endpoint(format!(
                    "zoom oauth: Zoom issued a new refresh token, but the keystore could not \
                     store it in `{}`; the teacher must authorize the Zoom app again",
                    app.record
                ))
            })?;
            *app.refresh_token.lock().unwrap_or_else(|e| e.into_inner()) = rotated;
        }
        Ok((answer.access_token, answer.expires_in))
    }

    /// `POST {oauth_base}/oauth/token`, HTTP Basic `client_id:client_secret`, with `form` as an
    /// `application/x-www-form-urlencoded` body — RFC 6749's token request. For the shared
    /// account the form is `grant_type=account_credentials&account_id=…` (§4.4.2; Zoom's own
    /// examples put those in the QUERY, and the body keeps the account id, a credential, out of
    /// every URL a host transport might log); for a teacher's own app it is
    /// `grant_type=refresh_token&refresh_token=…` (§6). An error answer is typed by
    /// [`origin_error`] (so a `401` for the client credentials is [`Error::Refused`]), a
    /// transport failure by [`no_answer`].
    async fn token_exchange(
        &self,
        token_url: &str,
        client_id: &str,
        client_secret: &str,
        form: Vec<u8>,
        credentials: &[String],
        now: Option<Time>,
    ) -> Result<TokenAnswer> {
        // The shared RFC 7617 builder (ledger #1857). Its refusal names the broken rule and the
        // stable code, never the client id or secret, so the error text is safe as it stands.
        let auth = ikigai_http::auth::basic(client_id, client_secret).map_err(|e| {
            Error::Endpoint(format!(
                "zoom oauth: the client credentials cannot form a Basic header ({}): {e}",
                e.code()
            ))
        })?;
        let mut secrets: Vec<&str> = credentials.iter().map(String::as_str).collect();
        secrets.push(auth.credentials());
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
                body: form,
            })
            .await
            .map_err(|e| no_answer(&e, token_url, "oauth", &secrets))?;
        if resp.status >= 400 {
            return Err(origin_error(resp, now, "oauth", &secrets).await);
        }
        let v: Value = serde_json::from_slice(&read_body(resp.body, "oauth").await?)
            .map_err(|_| Error::Endpoint("zoom oauth: response not JSON".to_string()))?;
        let access_token = v["access_token"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| {
                Error::Endpoint("zoom oauth: no access_token in response".to_string())
            })?;
        // Zoom states `expires_in` as a number of seconds; accept a string too, defensively.
        let expires_in = v["expires_in"]
            .as_u64()
            .or_else(|| v["expires_in"].as_str().and_then(|s| s.trim().parse().ok()));
        let refresh_token = v["refresh_token"]
            .as_str()
            .filter(|r| !r.is_empty())
            .map(str::to_string);
        Ok(TokenAnswer {
            access_token,
            expires_in,
            refresh_token,
        })
    }

    /// One call against Zoom's meeting API, with a bearer token. Create is `POST
    /// /users/{user}/meetings` (`type: 2`, one scheduled meeting; the attendees as
    /// `settings.meeting_invitees`); read is `GET /meetings/{id}`; update is `PATCH
    /// /meetings/{id}` (answered `204`), then a read, so the answer is the meeting Zoom now
    /// holds; cancel is `DELETE /meetings/{id}` (answered `204`). A `401` for a token the cache
    /// supplied (`cached`) is [`Outcome::TokenRefused`]: nothing was changed, so the caller may
    /// mint a fresh token and try once more. Every other error answer is typed by
    /// [`origin_error`] (a `404` is [`Error::NotFound`]), a transport failure by [`no_answer`].
    async fn call(
        &self,
        account: &Account,
        token: &str,
        call: &Call<'_>,
        cached: bool,
        now: Option<Time>,
    ) -> Result<Outcome> {
        let mut secrets = account.secrets();
        secrets.push(token.to_string());
        macro_rules! send {
            ($method:expr, $body:expr, $what:expr, $cached:expr) => {
                send!($method, &account.url, $body, $what, $cached)
            };
            ($method:expr, $url:expr, $body:expr, $what:expr, $cached:expr) => {
                self.exchange($method, $url, token, $body, $what, &secrets, $cached, now)
            };
        }
        match call {
            Call::Create(request) => {
                let body = create_body(request);
                let Some(v) = send!(Method::Post, Some(&body), call.what(), cached).await? else {
                    return Ok(Outcome::TokenRefused);
                };
                let mut meeting = parse_meeting(&v, call.what())?;
                meeting.attendees = request.attendees.clone();
                Ok(Outcome::Meeting(meeting))
            }
            Call::Get { host_only: false } => {
                match send!(Method::Get, None, call.what(), cached).await? {
                    None => Ok(Outcome::TokenRefused),
                    Some(v) => Ok(Outcome::Meeting(parse_meeting(&v, call.what())?)),
                }
            }
            _ => {
                // The account must host the meeting. Who it acts for comes first, so only that
                // read can meet a stale cached token; nothing has been changed by then.
                let Some(user) =
                    send!(Method::Get, &account.user_url, None, "get-user", cached).await?
                else {
                    return Ok(Outcome::TokenRefused);
                };
                let user_id = response_string(&user, "get-user", "id")?;
                let current = send!(Method::Get, None, "get-meeting", false)
                    .await?
                    .ok_or_else(|| Error::Refused("zoom get-meeting returned 401".to_string()))?;
                let host_id = response_string(&current, "get-meeting", "host_id")?;
                if host_id != user_id {
                    let id = parse_meeting(&current, "get-meeting")?.id;
                    return Err(Error::Denied(
                        format!(
                            "{} does not host meeting {MEETING_PREFIX}zoom-{id}: another Zoom \
                             user does, so only that meeting's own teacher can read its host \
                             details, change it or cancel it",
                            account.acting
                        )
                        .into(),
                    ));
                }
                self.act(account, call, current, token, &secrets, now).await
            }
        }
    }

    /// A call on a meeting its account hosts, `current` being the meeting as just read.
    async fn act(
        &self,
        account: &Account,
        call: &Call<'_>,
        current: Value,
        token: &str,
        secrets: &[String],
        now: Option<Time>,
    ) -> Result<Outcome> {
        macro_rules! send {
            ($method:expr, $body:expr, $what:expr) => {
                self.exchange(
                    $method,
                    &account.url,
                    token,
                    $body,
                    $what,
                    secrets,
                    false,
                    now,
                )
            };
        }
        match call {
            Call::Create(_) | Call::Get { host_only: false } => Err(Error::Endpoint(
                "zoom: not a call on an existing meeting".to_string(),
            )),
            Call::Get { host_only: true } => {
                Ok(Outcome::Meeting(parse_meeting(&current, "get-meeting")?))
            }
            Call::Update(body) => {
                send!(Method::Patch, Some(body), call.what())
                    .await?
                    .ok_or_else(|| {
                        Error::Refused("zoom update-meeting returned 401".to_string())
                    })?;
                let v = send!(Method::Get, None, "get-meeting")
                    .await?
                    .ok_or_else(|| Error::Refused("zoom get-meeting returned 401".to_string()))?;
                Ok(Outcome::Meeting(parse_meeting(&v, "get-meeting")?))
            }
            Call::Cancel => {
                send!(Method::Delete, None, call.what())
                    .await?
                    .ok_or_else(|| {
                        Error::Refused("zoom cancel-meeting returned 401".to_string())
                    })?;
                Ok(Outcome::Canceled)
            }
        }
    }

    /// One request with a bearer token: the answer's JSON (`Value::Null` for an empty body), or
    /// `None` for a `401` when `cached` says the token came from the cache.
    #[allow(clippy::too_many_arguments)]
    async fn exchange(
        &self,
        method: Method,
        url: &str,
        token: &str,
        body: Option<&Value>,
        what: &str,
        secrets: &[String],
        cached: bool,
        now: Option<Time>,
    ) -> Result<Option<Value>> {
        let secrets: Vec<&str> = secrets.iter().map(String::as_str).collect();
        let mut headers = vec![("Authorization".to_string(), format!("Bearer {token}"))];
        if body.is_some() {
            headers.push(("Content-Type".to_string(), "application/json".to_string()));
        }
        let resp = self
            .transport
            .send(HttpRequest {
                method,
                url: url.to_string(),
                headers,
                body: body
                    .map(|b| serde_json::to_vec(b).unwrap_or_default())
                    .unwrap_or_default(),
            })
            .await
            .map_err(|e| no_answer(&e, url, what, &secrets))?;
        if resp.status == 401 && cached {
            return Ok(None);
        }
        if resp.status >= 400 {
            return Err(origin_error(resp, now, what, &secrets).await);
        }
        let bytes = read_body(resp.body, what).await?;
        if bytes.iter().all(u8::is_ascii_whitespace) {
            return Ok(Some(Value::Null));
        }
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|_| Error::Endpoint(format!("zoom {what}: response not JSON")))
    }
}

/// What a teacher's record selects.
enum TeacherRecord {
    Shared { user_id: String },
    Own(OwnApp),
}

/// A token endpoint's answer.
struct TokenAnswer {
    access_token: String,
    expires_in: Option<u64>,
    refresh_token: Option<String>,
}

/// The create-meeting body: one scheduled meeting (`type: 2`), never a recurring series (`type:
/// 8`) — one meeting per occurrence, the caller expanding any recurrence itself.
fn create_body(request: &MeetingRequest) -> Value {
    let mut settings = request.settings.json();
    if !request.attendees.is_empty() {
        settings.insert("meeting_invitees".into(), invitees(&request.attendees));
    }
    let mut body = json!({
        "topic": request.topic,
        "type": 2,
        "start_time": request.start,
        "duration": request.duration,
        "settings": settings,
    });
    if let Some(tz) = &request.timezone {
        body["timezone"] = json!(tz);
    }
    if let Some(agenda) = &request.agenda {
        body["agenda"] = json!(agenda);
    }
    body
}

/// Zoom's `meeting_invitees`: a list of `{"email": …}`.
fn invitees(attendees: &[String]) -> Value {
    attendees
        .iter()
        .map(|email| json!({ "email": email }))
        .collect()
}

/// A meeting from a Zoom answer. Required fields that are missing, null or malformed are
/// reported, never replaced with request values. The attendees are Zoom's `meeting_invitees`,
/// those that are addresses [`is_email`] accepts.
fn parse_meeting(v: &Value, what: &str) -> Result<Meeting> {
    // Zoom returns the meeting id as a NUMBER; accept a string too, defensively.
    let id = v["id"]
        .as_i64()
        .map(|n| n.to_string())
        .or_else(|| v["id"].as_str().map(str::to_string))
        .filter(|id| is_zoom_id(id))
        .ok_or_else(|| response_field(what, "id"))?;
    let join_url = response_string(v, what, "join_url")?;
    let start_url = response_string(v, what, "start_url")?;
    let topic = response_string(v, what, "topic")?;
    let start = response_string(v, what, "start_time")?;
    let start_time = utc_datetime(&start).ok_or_else(|| response_field(what, "start_time"))?;
    let duration = v["duration"]
        .as_u64()
        .filter(|n| (1..=1440).contains(n))
        .ok_or_else(|| response_field(what, "duration"))? as u32;
    // A meeting need not have a passcode. Preserve absence rather than inventing one.
    let passcode = match v.get("password") {
        None | Some(Value::Null) => None,
        Some(Value::String(value)) => Some(value.clone()),
        _ => return Err(response_field(what, "password")),
    };
    let attendees = v["settings"]["meeting_invitees"]
        .as_array()
        .map(|list| {
            list.iter()
                .filter_map(|i| i["email"].as_str())
                .filter(|e| is_email(e))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    Ok(Meeting {
        id,
        join_url,
        start_url,
        passcode,
        topic,
        start_time,
        duration,
        attendees,
    })
}

/// A Zoom meeting id: decimal digits, not all zero, at most 20 of them.
fn is_zoom_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 20
        && id.bytes().all(|b| b.is_ascii_digit())
        && id.bytes().any(|b| b != b'0')
}

/// The meeting a `urn:meeting:id:{m}` names: `{m}` is `zoom-<digits>`; Zoom's id is answered.
fn meeting_name(inv: &Invocation<'_>) -> Result<String> {
    inv.bindings
        .get("m")
        .and_then(|m| m.strip_prefix("zoom-"))
        .filter(|id| is_zoom_id(id))
        .map(str::to_string)
        .ok_or_else(|| {
            invalid(
                "m",
                "not a meeting name (zoom- and the Zoom meeting id's digits)",
            )
        })
}

/// May this caller see the host link? Only the presenter: a holder of
/// `urn:cap:meeting:host:id:{m}` for this meeting, or of `urn:cap:meeting:host:teacher:{t}` for
/// the teacher whose account hosts it. Root holds both.
fn may_host(inv: &Invocation<'_>, meeting: &Meeting, teacher: Option<&str>) -> bool {
    inv.capability
        .allows(&format!("{CAP_HOST_MEETING}zoom-{}", meeting.id))
        || teacher.is_some_and(|t| inv.capability.allows(&format!("{CAP_HOST_TEACHER}{t}")))
}

/// A secret grant, checked before the secret is read or stored: `action` is `read` or `write`.
fn gate_secret(inv: &Invocation<'_>, action: &str, name: &str) -> Result<()> {
    let scope = format!("urn:cap:secret:{action}:{name}");
    if inv.capability.allows(&scope) {
        return Ok(());
    }
    let doing = if action == "read" {
        "reading"
    } else {
        "storing"
    };
    Err(Error::Denied(
        format!("capability does not allow {doing} secret `{name}` (needs {scope})").into(),
    ))
}

/// A URL path segment: ASCII letters, digits and `-._~` pass; every other byte is
/// percent-encoded, so a user id such as an email address is one segment.
fn path_segment(value: &str) -> String {
    /// Every byte but RFC 3986's unreserved characters.
    const SEGMENT: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'.')
        .remove(b'_')
        .remove(b'~');
    percent_encoding::utf8_percent_encode(value, SEGMENT).to_string()
}

/// An optional `xsd:boolean`: `true`, `false`, `1` or `0`.
fn boolean(inv: &Invocation<'_>, name: &str) -> Result<Option<bool>> {
    match optional(inv, name)?.as_deref() {
        None => Ok(None),
        Some("true" | "1") => Ok(Some(true)),
        Some("false" | "0") => Ok(Some(false)),
        Some(_) => Err(invalid(name, "not an xsd:boolean (true or false)")),
    }
}

/// The `teacher` whose account the call acts in: a name that is safe inside a keystore name.
fn teacher_arg(inv: &Invocation<'_>) -> Result<Option<String>> {
    match optional(inv, "teacher")? {
        None => Ok(None),
        Some(t)
            if !t.is_empty()
                && t.len() <= 64
                && t.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
                && !t.starts_with('.') =>
        {
            Ok(Some(t))
        }
        Some(_) => Err(invalid(
            "teacher",
            "not a teacher name (1 to 64 ASCII letters, digits, `-`, `_` or `.`, not starting \
             with `.`)",
        )),
    }
}

/// The `timezone`, held to the IANA zones.
fn timezone_arg(inv: &Invocation<'_>) -> Result<(Option<String>, Option<chrono_tz::Tz>)> {
    let timezone = optional(inv, "timezone")?;
    let zone = timezone
        .as_deref()
        .map(|name| {
            name.parse::<chrono_tz::Tz>()
                .map_err(|_| invalid("timezone", "not a recognized IANA time zone"))
        })
        .transpose()?;
    Ok((timezone, zone))
}

/// A `start`, held to `xsd:dateTime` and resolved to the instant Zoom is sent.
fn start_value(start: &str, zone: Option<chrono_tz::Tz>) -> Result<String> {
    let start = start.trim();
    if !is_xsd_datetime(start) {
        return Err(invalid(
            "start",
            "not an xsd:dateTime (YYYY-MM-DDThh:mm:ss, optionally .fff and Z or ±hh:mm)",
        ));
    }
    normalize_start(start, zone)
}

/// The `duration` in whole minutes, from 1 through 1440.
fn duration_arg(inv: &Invocation<'_>) -> Result<Option<u32>> {
    match optional(inv, "duration")? {
        None => Ok(None),
        Some(minutes) => match minutes.parse::<u32>() {
            Ok(n) if (1..=1440).contains(&n) => Ok(Some(n)),
            _ => Err(invalid(
                "duration",
                "not a positive xsd:integer of minutes, at most one day",
            )),
        },
    }
}

/// The `attendees` list.
fn attendees_arg(inv: &Invocation<'_>) -> Result<Option<Vec<String>>> {
    match optional(inv, "attendees")? {
        None => Ok(None),
        Some(list) => parse_attendees(&list).map(Some).ok_or_else(|| {
            invalid(
                "attendees",
                "not a list of email addresses (separated by commas, semicolons or whitespace)",
            )
        }),
    }
}

/// The face `as=` selects.
fn face_arg(inv: &Invocation<'_>) -> Result<String> {
    let face = optional(inv, "as")?.unwrap_or_else(|| FACES[0].to_string());
    if !FACES.contains(&face.as_str()) {
        return Err(invalid(
            "as",
            "not one of text/plain, text/turtle, application/json",
        ));
    }
    Ok(face)
}

fn start_spec() -> ArgSpec {
    ArgSpec::new("start")
        .summary(
            "start time: RFC 3339 with an offset, or local time with `timezone`; \
             DST gaps and overlaps require an offset; answers use UTC",
        )
        .class(XSD_DATETIME)
}

fn duration_spec() -> ArgSpec {
    ArgSpec::new("duration")
        .summary("length in minutes, an integer from 1 through 1440 (default 30)")
        .class(XSD_INTEGER)
}

fn timezone_spec() -> ArgSpec {
    ArgSpec::new("timezone")
        .summary("IANA zone for local start times (e.g. America/Los_Angeles); omitted uses the Zoom account zone")
        .class(XSD_STRING)
        .optional()
}

fn attendees_spec() -> ArgSpec {
    ArgSpec::new("attendees")
        .summary(
            "who the meeting is for: email addresses, separated by commas, semicolons \
             or whitespace (Zoom's meeting invitees; ical:attendee in the graph)",
        )
        .class(XSD_STRING)
        .optional()
}

fn agenda_spec() -> ArgSpec {
    ArgSpec::new("agenda")
        .summary("a longer description")
        .class(XSD_STRING)
        .optional()
}

fn content_spec() -> ArgSpec {
    ArgSpec::new("content")
        .summary("the agenda, when piped or given as the sink's body — a named `agenda` wins")
        .class(XSD_STRING)
        .optional()
}

fn teacher_spec() -> ArgSpec {
    ArgSpec::new("teacher")
        .summary(
            "the teacher whose Zoom account hosts the meeting: their record in the keystore \
             (zoom-teacher-<teacher>); omitted uses the shared account's configured user",
        )
        .class(XSD_STRING)
        .optional()
}

fn face_spec() -> ArgSpec {
    ArgSpec::new("as")
        .summary("the face: the join URL, the shareable graph, or the full envelope")
        .class(XSD_STRING)
        .one_of(FACES)
        .default_value(FACES[0])
        .optional()
}

/// The meeting settings as inputs. A new meeting states their defaults; an update has none,
/// since a setting not given is left as it is.
fn setting_specs(new: bool) -> Vec<ArgSpec> {
    let flag = |name: &str, summary: &str, default: bool| {
        let spec = ArgSpec::new(name)
            .summary(summary.to_string())
            .class(XSD_BOOLEAN)
            .optional();
        if new {
            spec.default_value(default.to_string())
        } else {
            spec
        }
    };
    let recording = ArgSpec::new("auto_recording")
        .summary("record automatically: none, local or cloud (default none)")
        .class(XSD_STRING)
        .one_of(AUTO_RECORDING)
        .optional();
    vec![
        flag(
            "waiting_room",
            "hold people in a waiting room until the host admits them (default true)",
            true,
        ),
        flag(
            "join_before_host",
            "let people join before the host (default false)",
            false,
        ),
        flag(
            "authenticated_join",
            "only signed-in Zoom users may join (default false)",
            false,
        ),
        if new {
            recording.default_value(AUTO_RECORDING[0])
        } else {
            recording
        },
    ]
}

/// An error ANSWER from Zoom, typed by ikigai-http's one status table
/// ([`ikigai_http::status::response_error`], ledger #1546): the variant is the table's row for
/// the status (`401`/`403` [`Error::Refused`], `429` [`Error::RetryAfter`] with the `Retry-After`
/// delay measured on `now`, `500` [`Error::Endpoint`], ...), and the detail is this module's
/// [`server_error`] text, scrubbed of every credential. `what` names the call (`oauth`,
/// `create-meeting`). Only called for a status of 400 or more; a success handed in anyway is
/// still reported as a failure, never a panic. The body is read within [`MAX_ZOOM_BODY`]; one
/// that cannot be read (too long, or the transport failed mid-body) only loses the server's
/// `message`, never the status's row in the table.
async fn origin_error(
    mut resp: HttpResponse,
    now: Option<Time>,
    what: &str,
    secrets: &[&str],
) -> Error {
    let body = std::mem::take(&mut resp.body)
        .collect(MAX_ZOOM_BODY)
        .await
        .ok();
    let detail = scrub(server_error(what, resp.status, body.as_deref()), secrets);
    response_error(&resp, now, detail.clone()).unwrap_or(Error::Endpoint(detail))
}

/// A success answer's body, read within [`MAX_ZOOM_BODY`]. Past the bound it is
/// [`Error::OverBudget`] naming the call; a transport that fails mid-body is ikigai-http's
/// [`Error::Timeout`] or [`Error::Unavailable`], as for one that never answered.
async fn read_body(body: HttpBody, what: &str) -> Result<Vec<u8>> {
    body.collect(MAX_ZOOM_BODY).await.map_err(|e| match e {
        Error::OverBudget { .. } => Error::OverBudget {
            detail: format!("zoom {what}: response body exceeds {MAX_ZOOM_BODY} bytes"),
        },
        other => other,
    })
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
    match inv.inline_str(name) {
        Ok(value) => Ok(value.to_string()),
        Err(Error::MissingArgument(_)) => Err(Error::MissingArgument(name.to_string())),
        Err(_) => Err(invalid(name, "expected an inline UTF-8 value")),
    }
}

/// An optional by-value input. Only absence selects a default; decoding errors propagate.
fn optional(inv: &Invocation<'_>, name: &str) -> Result<Option<String>> {
    match inv.inline_str(name) {
        Ok(value) => Ok(Some(value.trim().to_string())),
        Err(Error::MissingArgument(_)) => Ok(None),
        Err(_) => Err(invalid(name, "expected an inline UTF-8 value")),
    }
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
        Err(Error::Denied(
            format!("capability does not allow reaching `{host}` (needs urn:cap:net:{host})")
                .into(),
        ))
    }
}

/// What a server's error response contributes to an error: its status and, when the body is
/// JSON, its `message` / `reason` / `error` string — never the body itself, which a server may
/// fill with the request it refused (headers included). `None` is a body that could not be read.
fn server_error(what: &str, status: u16, body: Option<&[u8]>) -> String {
    let Some(body) = body else {
        return format!("zoom {what} returned {status} (body unreadable)");
    };
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
/// fractional second, an optional `Z` or `±hh:mm`. Calendar ranges are checked separately.
fn has_datetime_shape(s: &str) -> bool {
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

/// A valid date and clock time, with either an explicit offset or no offset.
fn is_xsd_datetime(s: &str) -> bool {
    has_datetime_shape(s) && (utc_datetime(s).is_some() || local_datetime(s).is_some())
}

fn local_datetime(s: &str) -> Option<NaiveDateTime> {
    NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f").ok()
}

/// Zoom documents UTC `Z` on the wire; numeric offsets are converted without changing the instant.
fn utc_datetime(s: &str) -> Option<String> {
    if !has_datetime_shape(s) {
        return None;
    }
    let parsed = DateTime::parse_from_rfc3339(s).ok()?;
    // xsd:dateTime's timezone range is -14:00 through +14:00.
    if parsed.offset().local_minus_utc().unsigned_abs() > 14 * 3600 {
        return None;
    }
    Some(format_utc(parsed.with_timezone(&Utc), s))
}

/// Offsets change whole seconds only. Carry the original fractional digits unchanged rather
/// than rounding to the date library's nanosecond precision.
fn format_utc(time: DateTime<Utc>, source: &str) -> String {
    let mut out = time.to_rfc3339_opts(SecondsFormat::Secs, true);
    if source.as_bytes().get(19) == Some(&b'.') {
        let digits = source[20..].bytes().take_while(u8::is_ascii_digit).count();
        out.insert_str(out.len() - 1, &source[19..20 + digits]);
    }
    out
}

fn normalize_start(s: &str, zone: Option<chrono_tz::Tz>) -> Result<String> {
    if let Some(utc) = utc_datetime(s) {
        return Ok(utc);
    }
    let local =
        local_datetime(s).ok_or_else(|| invalid("start", "not a valid calendar date and time"))?;
    match zone {
        Some(zone) => zone
            .from_local_datetime(&local)
            .single()
            .map(|dt| format_utc(dt.with_timezone(&Utc), s))
            .ok_or_else(|| {
                invalid(
                    "start",
                    "local time is in a DST gap or overlap; provide an explicit UTC offset",
                )
            }),
        // Zoom's documented default is the account's zone, which this module cannot inspect.
        None => Ok(s.to_string()),
    }
}

fn response_field(what: &str, name: &str) -> Error {
    Error::Endpoint(format!(
        "zoom {what}: missing or invalid {name} in response"
    ))
}

fn response_string(v: &Value, what: &str, name: &str) -> Result<String> {
    v[name]
        .as_str()
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| response_field(what, name))
}

/// A scheduled meeting, provider-neutral.
struct Meeting {
    id: String,
    join_url: String,
    /// The host URL — carries a start token, so it is NEVER in the shareable turtle face.
    start_url: String,
    passcode: Option<String>,
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
        s.push_str(&format!("{} a ical:Vevent ;\n", ttl_iri(&self.name())));
        s.push_str("    schema:provider \"zoom\" ;\n");
        s.push_str(&format!("    dcterms:identifier {} ;\n", ttl_str(&self.id)));
        s.push_str(&format!("    ical:summary {} ;\n", ttl_str(&self.topic)));
        s.push_str(&format!(
            "    ical:conference {} ;\n",
            ttl_iri(&self.join_url)
        ));
        if let Some(passcode) = &self.passcode {
            if !passcode.is_empty() {
                s.push_str(&format!("    ik:passcode {} ;\n", ttl_str(passcode)));
            }
        }
        for attendee in &self.attendees {
            s.push_str(&format!(
                "    ical:attendee {} ;\n",
                ttl_iri(&format!("mailto:{attendee}"))
            ));
        }
        s.push_str(&format!(
            "    ical:dtstart {}^^xsd:dateTime ;\n",
            ttl_str(&self.start_time)
        ));
        s.push_str(&format!(
            "    ical:duration \"PT{}M\"^^xsd:duration .\n",
            self.duration
        ));
        s.into_bytes()
    }

    /// The FULL envelope: the meeting's resource name and Zoom's fields. The host `start_url`
    /// (a URL that carries a host token) is in it only when `host` says the caller is the
    /// presenter ([`may_host`]); otherwise the key is absent.
    fn json_envelope(&self, host: bool) -> Vec<u8> {
        let mut envelope = json!({
            "meeting": self.name(),
            "provider": "zoom",
            "id": self.id,
            "join_url": self.join_url,
            "passcode": self.passcode,
            "topic": self.topic,
            "start_time": self.start_time,
            "duration": self.duration,
            "attendees": self.attendees,
        });
        if host {
            envelope["start_url"] = json!(self.start_url);
        }
        serde_json::to_vec(&envelope).unwrap_or_default()
    }

    /// The meeting's resource name, `urn:meeting:id:zoom-<id>`: what Source, Sink and Delete
    /// address, and the graph's subject.
    fn name(&self) -> String {
        format!("{MEETING_PREFIX}zoom-{}", self.id)
    }

    /// The face `as=` selected: default `text/plain` = the join URL (what an invite needs);
    /// `text/turtle` = the SHAREABLE graph (join-safe, `ical:conference`, never the host URL);
    /// `application/json` = the full envelope, with the host URL for the presenter only.
    fn face(self, face: &str, host: bool) -> Representation {
        match face {
            "text/turtle" => repr("text/turtle", self.turtle()),
            "application/json" => repr("application/json", self.json_envelope(host)),
            _ => repr("text/plain", self.join_url.into_bytes()),
        }
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
                r#"{"id":88899900011,"join_url":"https://zoom.us/j/88899900011?pwd=abc","start_url":"https://zoom.us/s/88899900011?zak=SECRET","password":"a1b2c3","topic":"Intro call","duration":30,"start_time":"2026-07-30T22:00:00Z"}"#
            } else {
                return Err(format!("unexpected url {}", request.url));
            };
            self.seen.lock().unwrap().push(request);
            Ok(HttpResponse {
                status: 200,
                headers: vec![],
                body: body.as_bytes().to_vec().into(),
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
            out.contains("<urn:meeting:id:zoom-88899900011> a ical:Vevent"),
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
                body: body.as_bytes().to_vec().into(),
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

    /// A transport that streams every answer (ikigai-http 0.4's `HttpBody::from_reader`), the
    /// way a host adapter hands over a body it has not read: `status` and `body` for the token
    /// endpoint, FakeZoom's meeting for the create call.
    struct Streamed(u16, Vec<u8>);

    #[async_trait]
    impl HttpTransport for Streamed {
        async fn send(&self, request: HttpRequest) -> std::result::Result<HttpResponse, String> {
            let (status, body) = if request.url.contains("/oauth/token") {
                (self.0, self.1.clone())
            } else {
                (
                    200,
                    br#"{"id":1,"join_url":"https://zoom.us/j/1","start_url":"https://zoom.us/s/1","topic":"t","duration":30,"start_time":"2026-07-30T22:00:00Z"}"#.to_vec(),
                )
            };
            Ok(HttpResponse {
                status,
                headers: vec![],
                body: HttpBody::from_reader(futures::io::Cursor::new(body)),
            })
        }
    }

    /// A Zoom answer is read within `MAX_ZOOM_BODY` (ikigai-http 0.4 hands the body over
    /// unread): a streamed answer is read whole; a success body past the bound is `OverBudget`
    /// (a declared class) rather than buffered; an error body past it loses only the server's
    /// message, never the status's row in the table.
    #[test]
    fn a_zoom_body_is_read_within_its_bound() {
        let spec = declared();
        let run = |transport: Streamed| {
            let k = Kernel::new(Arc::new(space(
                Arc::new(transport),
                Arc::new(FakeSecrets),
                ZoomConfig::default(),
            )));
            let req = Request::new(Verb::Sink, Iri::parse("urn:meeting:zoom:schedule").unwrap())
                .with_arg("topic", ArgRef::Inline(b"t".to_vec()))
                .with_arg("start", ArgRef::Inline(b"2026-07-30T22:00:00Z".to_vec()));
            block_on(k.issue(req, &zoom_cap())).map(|r| String::from_utf8(r.bytes).unwrap())
        };
        let token = br#"{"access_token":"tok","expires_in":3600}"#.to_vec();
        assert_eq!(run(Streamed(200, token)).unwrap(), "https://zoom.us/j/1");

        let mut long = br#"{"access_token":"tok","pad":""#.to_vec();
        long.resize(MAX_ZOOM_BODY + 1, b' ');
        let err = run(Streamed(200, long.clone())).unwrap_err();
        assert!(matches!(err, Error::OverBudget { .. }), "{err:?}");
        assert!(
            err.to_string()
                .contains("zoom oauth: response body exceeds"),
            "{err}"
        );
        assert_eq!(spec.admits_failure(err.class()), Some(true));

        let err = run(Streamed(401, long)).unwrap_err();
        assert!(matches!(err, Error::Refused(_)), "{err:?}");
        assert!(
            err.to_string()
                .contains("zoom oauth returned 401 (body unreadable)"),
            "{err}"
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
            server_error("create-meeting", 400, Some(echo)),
            "zoom create-meeting returned 400: bad request"
        );
        assert_eq!(
            server_error(
                "oauth",
                401,
                Some(
                    br#"{"reason":"Invalid client_id or client_secret","error":"invalid_client"}"#
                )
            ),
            "zoom oauth returned 401: Invalid client_id or client_secret"
        );
        assert_eq!(
            server_error("oauth", 502, Some(b"<html>Bearer tok</html>")),
            "zoom oauth returned 502 (23 bytes, not JSON)"
        );
        assert_eq!(
            server_error("oauth", 503, None),
            "zoom oauth returned 503 (body unreadable)"
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
