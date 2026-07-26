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

#[async_trait]
impl Endpoint for ZoomBackend {
    async fn invoke(&self, inv: &Invocation<'_>) -> Result<Representation> {
        let topic = inv
            .inline_str("topic")
            .map_err(|_| Error::MissingArgument("topic".to_string()))?
            .to_string();
        let start = inv
            .inline_str("start")
            .map_err(|_| Error::MissingArgument("start".to_string()))?
            .trim()
            .to_string();
        // Minutes; a garbled value falls back to 30 rather than failing the schedule.
        let duration: u32 = inv
            .inline_str("duration")
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(30);
        let timezone = inv
            .inline_str("timezone")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let agenda = inv.inline_str("agenda").ok().map(str::to_string);

        // Credentials the host resolved from the keystore under a capability.
        let account_id = self.secret_str(&self.config.account_id_secret)?;
        let client_id = self.secret_str(&self.config.client_id_secret)?;
        let client_secret = self.secret_str(&self.config.client_secret_secret)?;

        let token = self
            .access_token(inv, &account_id, &client_id, &client_secret)
            .await?;
        let meeting = self
            .create_meeting(
                inv,
                &token,
                &topic,
                &start,
                duration,
                timezone.as_deref(),
                agenda.as_deref(),
            )
            .await?;

        // Faces: default text/plain = the join URL (what the invite needs); `as=text/turtle` = the
        // SHAREABLE graph (join-safe, `ical:conference`); `as=application/json` = the full envelope
        // for the trusted caller, INCLUDING the host `start_url` (a URL that carries a host token).
        let want = inv.inline_str("as").unwrap_or("");
        if want.contains("turtle") {
            Ok(repr("text/turtle", meeting.turtle()))
        } else if want.contains("json") {
            Ok(repr("application/json", meeting.json_envelope()))
        } else {
            Ok(repr("text/plain", meeting.join_url.into_bytes()))
        }
    }

    fn name(&self) -> &str {
        "meeting"
    }

    fn describe(&self) -> Description {
        Description::new("urn:meeting:zoom:schedule")
            .summary(
                "Schedule a Zoom meeting. Returns the join URL (text), the shareable graph \
                 (as=text/turtle — ical:conference), or the full envelope (as=application/json, \
                 which includes the host start URL).",
            )
            .verb(Verb::Sink)
            .input(ArgSpec::new("topic").summary("the meeting title"))
            .input(ArgSpec::new("start").summary(
                "start time, ISO 8601 (e.g. 2026-07-30T15:00:00Z, or a local time with `timezone`)",
            ))
            .input(
                ArgSpec::new("duration")
                    .summary("length in minutes (default 30)")
                    .optional(),
            )
            .input(
                ArgSpec::new("timezone")
                    .summary("IANA zone the start time is in (e.g. America/Los_Angeles)")
                    .optional(),
            )
            .input(
                ArgSpec::new("agenda")
                    .summary("a longer description")
                    .optional(),
            )
            .input(
                ArgSpec::new("as")
                    .summary("text/turtle or application/json")
                    .optional(),
            )
    }
}

impl ZoomBackend {
    /// A secret's bytes as a trimmed UTF-8 string.
    fn secret_str(&self, name: &str) -> Result<String> {
        let bytes = self.secrets.read(name)?;
        String::from_utf8(bytes)
            .map(|s| s.trim().to_string())
            .map_err(|_| Error::Endpoint(format!("secret `{name}` is not valid UTF-8")))
    }

    /// Refuse before the socket if the capability doesn't grant this host — so a declared
    /// `urn:cap:net:<host>` is enforced, not merely documented.
    fn gate_net(&self, inv: &Invocation<'_>, url: &str) -> Result<()> {
        let parsed = url::Url::parse(url).map_err(|e| Error::Endpoint(format!("bad url: {e}")))?;
        let host = parsed.host_str().unwrap_or("").to_string();
        if ikigai_http::net_allows(inv.capability, &host, parsed.path()) {
            Ok(())
        } else {
            Err(Error::Denied(format!(
                "capability does not allow reaching `{host}` (needs urn:cap:net:{host})"
            )))
        }
    }

    /// Exchange the S2S credentials for a short-lived bearer token.
    /// `POST {oauth_base}/oauth/token?grant_type=account_credentials&account_id=…`, HTTP Basic
    /// `client_id:client_secret`. (No caching in Slice 0 — a booking approval is rare; caching the
    /// ~1h token is a later optimization.)
    async fn access_token(
        &self,
        inv: &Invocation<'_>,
        account_id: &str,
        client_id: &str,
        client_secret: &str,
    ) -> Result<String> {
        let url = format!(
            "{}/oauth/token?grant_type=account_credentials&account_id={}",
            self.config.oauth_base.trim_end_matches('/'),
            account_id
        );
        self.gate_net(inv, &url)?;
        let basic = base64_encode(format!("{client_id}:{client_secret}").as_bytes());
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
            .map_err(|e| Error::Endpoint(format!("zoom oauth transport error: {e}")))?;
        if resp.status >= 400 {
            return Err(Error::Endpoint(format!(
                "zoom oauth returned {}: {}",
                resp.status,
                String::from_utf8_lossy(&resp.body)
            )));
        }
        let v: Value = serde_json::from_slice(&resp.body)
            .map_err(|e| Error::Endpoint(format!("zoom oauth: response not JSON: {e}")))?;
        v["access_token"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| Error::Endpoint("zoom oauth: no access_token in response".to_string()))
    }

    /// Create a scheduled meeting. `POST {api_base}/users/{user_id}/meetings`, Bearer token.
    /// Request fields per Zoom's create-meeting API (`type: 2` = scheduled); response fields
    /// `id` / `join_url` / `start_url` / `password`.
    #[allow(clippy::too_many_arguments)]
    async fn create_meeting(
        &self,
        inv: &Invocation<'_>,
        token: &str,
        topic: &str,
        start: &str,
        duration: u32,
        timezone: Option<&str>,
        agenda: Option<&str>,
    ) -> Result<Meeting> {
        let url = format!(
            "{}/users/{}/meetings",
            self.config.api_base.trim_end_matches('/'),
            self.config.user_id
        );
        self.gate_net(inv, &url)?;
        let mut body = json!({
            "topic": topic,
            "type": 2,
            "start_time": start,
            "duration": duration,
            "settings": { "join_before_host": true, "waiting_room": false }
        });
        if let Some(tz) = timezone {
            body["timezone"] = json!(tz);
        }
        if let Some(a) = agenda {
            body["agenda"] = json!(a);
        }
        let resp = self
            .transport
            .send(HttpRequest {
                method: Method::Post,
                url,
                headers: vec![
                    ("Authorization".to_string(), format!("Bearer {token}")),
                    ("Content-Type".to_string(), "application/json".to_string()),
                ],
                body: serde_json::to_vec(&body).unwrap_or_default(),
            })
            .await
            .map_err(|e| Error::Endpoint(format!("zoom create-meeting transport error: {e}")))?;
        if resp.status >= 400 {
            return Err(Error::Endpoint(format!(
                "zoom create-meeting returned {}: {}",
                resp.status,
                String::from_utf8_lossy(&resp.body)
            )));
        }
        let v: Value = serde_json::from_slice(&resp.body)
            .map_err(|e| Error::Endpoint(format!("zoom create-meeting: response not JSON: {e}")))?;
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
            topic: topic.to_string(),
            start_time: v["start_time"].as_str().unwrap_or(start).to_string(),
            duration,
        })
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
    /// The SHAREABLE graph — join-safe fields only. `ical:conference` is the RFC 7986 CONFERENCE
    /// property (a URI); an ICS generator maps it to a `CONFERENCE:` line + the event's link. The
    /// host `start_url` is deliberately withheld. (`ik:*` terms are module-local pending promotion
    /// into ikigai-vocab.)
    fn turtle(&self) -> Vec<u8> {
        let mut s = String::new();
        s.push_str("@prefix ik: <https://ikigai-rs.dev/ns#> .\n");
        s.push_str("@prefix ical: <http://www.w3.org/2002/12/cal/ical#> .\n\n");
        s.push_str(&format!("<urn:meeting:zoom:{}> a ik:Meeting ;\n", self.id));
        s.push_str("    ik:provider \"zoom\" ;\n");
        s.push_str(&format!("    ik:meetingId {} ;\n", ttl_str(&self.id)));
        s.push_str(&format!("    ical:summary {} ;\n", ttl_str(&self.topic)));
        s.push_str(&format!("    ical:conference <{}> ;\n", self.join_url));
        s.push_str(&format!("    ik:joinUrl <{}> ;\n", self.join_url));
        if !self.passcode.is_empty() {
            s.push_str(&format!("    ik:passcode {} ;\n", ttl_str(&self.passcode)));
        }
        s.push_str(&format!(
            "    ical:dtstart {} .\n",
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

    /// A capability that grants the two Zoom hosts (what the host would attenuate the handler to).
    fn zoom_cap() -> Capability {
        Capability::scoped(vec![
            "urn:cap:meeting:zoom:schedule".to_string(),
            "urn:cap:net:zoom.us".to_string(),
            "urn:cap:net:api.zoom.us".to_string(),
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
    }

    #[test]
    fn a_missing_net_capability_is_refused_before_the_socket() {
        // Holds the meeting cap but NOT the net grants → denied at gate_net, transport untouched.
        let cap = Capability::scoped(vec!["urn:cap:meeting:zoom:schedule".to_string()]);
        let transport = Arc::new(FakeZoom::default());
        let err = schedule(
            Arc::clone(&transport),
            &cap,
            &[("topic", "t"), ("start", "2026-07-30T22:00:00Z")],
        )
        .unwrap_err();
        assert!(matches!(err, Error::Denied(_)), "{err:?}");
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
