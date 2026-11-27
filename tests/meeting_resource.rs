//! A meeting as a resource (ledger #4519): `urn:meeting:id:{m}` answers Source (details),
//! Sink (update) and Delete (cancel); per-teacher accounts read from ikigai-secret; meeting
//! settings are arguments, never hard-coded; one meeting per occurrence; the host link is
//! sealed to the presenter. Hermetic: a fake Zoom routed by method and path, recording every
//! request, and a keystore that records every read and every store.
use async_trait::async_trait;
use futures::executor::block_on;
use ikigai_core::{ArgRef, Capability, Error, Iri, Kernel, Representation, Request, Verb};
use ikigai_http::{HttpRequest, HttpResponse, HttpTransport, Method};
use ikigai_meeting::{space, SecretReader, ZoomConfig};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

const MEETING: &str = "urn:meeting:id:zoom-88899900011";
const HOST_URL: &str = "https://zoom.us/s/88899900011?zak=HOST-SECRET";

/// One request as the fake received it.
#[derive(Clone, Debug)]
struct Seen {
    method: Method,
    url: String,
    authorization: String,
    body: Vec<u8>,
}

impl Seen {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap()
    }
    fn form(&self) -> HashMap<String, String> {
        url::form_urlencoded::parse(&self.body)
            .into_owned()
            .collect()
    }
}

/// Zoom as this module speaks to it. The meeting it holds changes on PATCH and is gone after
/// DELETE, so a read after an update answers the update.
struct Zoom {
    seen: Mutex<Vec<Seen>>,
    meeting: Mutex<Option<Value>>,
}

impl Zoom {
    fn new() -> Arc<Zoom> {
        Arc::new(Zoom {
            seen: Mutex::default(),
            meeting: Mutex::new(Some(json!({
                "id": 88899900011u64, "type": 2, "topic": "Week 1",
                "start_time": "2026-12-01T17:00:00Z", "duration": 60,
                "timezone": "America/New_York", "agenda": "Intro",
                "join_url": "https://zoom.us/j/88899900011?pwd=abc", "start_url": HOST_URL,
                "password": "a1b2c3",
                "settings": {"waiting_room": true, "join_before_host": false}
            }))),
        })
    }
    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
    fn api(&self) -> Vec<Seen> {
        self.seen()
            .into_iter()
            .filter(|s| !s.url.ends_with("/oauth/token"))
            .collect()
    }
}

fn answer(status: u16, body: Option<Value>) -> HttpResponse {
    HttpResponse {
        status,
        headers: vec![],
        body: body
            .map(|v| serde_json::to_vec(&v).unwrap())
            .unwrap_or_default()
            .into(),
    }
}

#[async_trait]
impl HttpTransport for Zoom {
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse, String> {
        let authorization = request
            .headers
            .iter()
            .find(|(k, _)| k == "Authorization")
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        self.seen.lock().unwrap().push(Seen {
            method: request.method,
            url: request.url.clone(),
            authorization,
            body: request.body.clone(),
        });
        let path = request.url.split('?').next().unwrap().to_string();
        let mut meeting = self.meeting.lock().unwrap();
        Ok(match (request.method, path.as_str()) {
            (Method::Post, p) if p.ends_with("/oauth/token") => {
                let form: HashMap<String, String> = url::form_urlencoded::parse(&request.body)
                    .into_owned()
                    .collect();
                let mut body = json!({"access_token": "tok-1", "expires_in": 3600});
                if form.get("grant_type").map(String::as_str) == Some("refresh_token") {
                    body["access_token"] = json!("user-tok-1");
                    body["refresh_token"] = json!("refresh-2");
                }
                answer(200, Some(body))
            }
            (Method::Post, p) if p.ends_with("/meetings") => {
                let sent: Value = serde_json::from_slice(&request.body).unwrap();
                let mut created = meeting.clone().unwrap();
                for key in ["topic", "start_time", "duration"] {
                    created[key] = sent[key].clone();
                }
                answer(201, Some(created))
            }
            (method, "https://api.zoom.us/v2/meetings/88899900011") => match &mut *meeting {
                None => answer(
                    404,
                    Some(json!({"code": 3001, "message": "Meeting does not exist: 88899900011."})),
                ),
                Some(current) => match method {
                    Method::Get => answer(200, Some(current.clone())),
                    Method::Patch => {
                        let sent: Value = serde_json::from_slice(&request.body).unwrap();
                        for (key, value) in sent.as_object().unwrap() {
                            if key != "settings" {
                                current[key] = value.clone();
                            }
                        }
                        answer(204, None)
                    }
                    Method::Delete => {
                        *meeting = None;
                        answer(204, None)
                    }
                    _ => answer(405, None),
                },
            },
            (_, "https://api.zoom.us/v2/meetings/77700000000") => answer(
                404,
                Some(json!({"code": 3001, "message": "Meeting does not exist: 77700000000."})),
            ),
            _ => answer(404, Some(json!({"message": "no such path"}))),
        })
    }
}

/// The keystore: the shared Server-to-Server credentials and two teachers' records, one acting
/// through the shared account and one with their own user-level OAuth app.
#[derive(Default)]
struct Keystore {
    reads: Mutex<Vec<String>>,
    stored: Mutex<Vec<(String, Vec<u8>)>>,
}

impl SecretReader for Keystore {
    fn read(&self, name: &str) -> ikigai_core::Result<Vec<u8>> {
        self.reads.lock().unwrap().push(name.to_string());
        if let Some((_, value)) = self
            .stored
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|(n, _)| n == name)
        {
            return Ok(value.clone());
        }
        Ok(match name {
            "zoom-account-id" => b"acct-1".to_vec(),
            "zoom-client-id" => b"cid-1".to_vec(),
            "zoom-client-secret" => b"csec-1".to_vec(),
            "zoom-teacher-ada" => br#"{"user_id":"ada@example.invalid"}"#.to_vec(),
            "zoom-teacher-bea" => {
                br#"{"client_id":"bea-cid","client_secret":"bea-csec","refresh_token":"refresh-1"}"#
                    .to_vec()
            }
            other => return Err(Error::NotFound(format!("no secret `{other}`"))),
        })
    }

    fn store(&self, name: &str, value: &[u8]) -> ikigai_core::Result<()> {
        self.stored
            .lock()
            .unwrap()
            .push((name.to_string(), value.to_vec()));
        Ok(())
    }
}

fn kernel(zoom: &Arc<Zoom>, keys: &Arc<Keystore>) -> Kernel {
    Kernel::new(Arc::new(space(
        zoom.clone(),
        keys.clone(),
        ZoomConfig::default(),
    )))
}

fn issue(
    k: &Kernel,
    verb: Verb,
    iri: &str,
    args: &[(&str, &str)],
    cap: &Capability,
) -> ikigai_core::Result<Representation> {
    let mut req = Request::new(verb, Iri::parse(iri).unwrap());
    for (name, value) in args {
        req = req.with_arg(*name, ArgRef::Inline(value.as_bytes().to_vec()));
    }
    block_on(k.issue(req, cap))
}

fn text(r: Representation) -> String {
    String::from_utf8(r.bytes).unwrap()
}

/// The grants a teacher-facing door would hold: both Zoom hosts and the shared credentials.
fn shared_grants() -> Vec<String> {
    [
        "urn:cap:net:zoom.us",
        "urn:cap:net:api.zoom.us",
        "urn:cap:secret:read:zoom-account-id",
        "urn:cap:secret:read:zoom-client-id",
        "urn:cap:secret:read:zoom-client-secret",
    ]
    .map(str::to_string)
    .to_vec()
}

fn grants(extra: &[&str]) -> Capability {
    let mut scopes = shared_grants();
    scopes.extend(extra.iter().map(|s| s.to_string()));
    Capability::scoped(scopes)
}

#[test]
fn source_reads_the_meeting_by_its_provider_neutral_name() {
    let (zoom, keys) = (Zoom::new(), Arc::default());
    let k = kernel(&zoom, &keys);
    let url = text(issue(&k, Verb::Source, MEETING, &[], &grants(&[])).expect("reads"));
    assert_eq!(url, "https://zoom.us/j/88899900011?pwd=abc");
    let graph = text(
        issue(
            &k,
            Verb::Source,
            MEETING,
            &[("as", "text/turtle")],
            &grants(&[]),
        )
        .expect("reads"),
    );
    assert!(
        graph.contains(&format!("<{MEETING}> a ical:Vevent")),
        "{graph}"
    );
    assert!(graph.contains("ical:summary \"Week 1\""), "{graph}");
    assert!(!graph.contains("HOST-SECRET"), "{graph}");
    let api = zoom.api();
    assert!(api.iter().all(|s| s.method == Method::Get), "{api:?}");
    assert_eq!(api[0].url, "https://api.zoom.us/v2/meetings/88899900011");
}

#[test]
fn the_host_link_is_sealed_to_the_presenter() {
    let (zoom, keys) = (Zoom::new(), Arc::default());
    let k = kernel(&zoom, &keys);
    let json_face = [("as", "application/json")];
    let read = |cap: &Capability| -> Value {
        serde_json::from_slice(
            &issue(&k, Verb::Source, MEETING, &json_face, cap)
                .expect("reads")
                .bytes,
        )
        .unwrap()
    };
    let learner = read(&grants(&[]));
    assert!(learner.get("start_url").is_none(), "{learner}");
    assert_eq!(learner["join_url"], "https://zoom.us/j/88899900011?pwd=abc");
    assert_eq!(learner["meeting"], MEETING);
    let presenter = read(&grants(&["urn:cap:meeting:host:id:zoom-88899900011"]));
    assert_eq!(presenter["start_url"], HOST_URL);
    // Another meeting's host grant does not reach this one: the parameter goes last.
    let other = read(&grants(&["urn:cap:meeting:host:id:zoom-1"]));
    assert!(other.get("start_url").is_none(), "{other}");

    // Scheduling: the host link is the teacher's, sealed the same way.
    let schedule = |cap: &Capability| -> Value {
        let args = [
            ("topic", "Week 2"),
            ("start", "2026-12-08T17:00:00Z"),
            ("teacher", "ada"),
            ("as", "application/json"),
        ];
        serde_json::from_slice(
            &issue(&k, Verb::Sink, "urn:meeting:schedule", &args, cap)
                .expect("schedules")
                .bytes,
        )
        .unwrap()
    };
    let ada = ["urn:cap:secret:read:zoom-teacher-ada"];
    assert!(schedule(&grants(&ada)).get("start_url").is_none());
    let presenter = schedule(&grants(&[ada[0], "urn:cap:meeting:host:teacher:ada"]));
    assert_eq!(presenter["start_url"], HOST_URL);
}

#[test]
fn sink_moves_the_meeting_and_answers_the_meeting_zoom_now_holds() {
    let (zoom, keys) = (Zoom::new(), Arc::default());
    let k = kernel(&zoom, &keys);
    let out = issue(
        &k,
        Verb::Sink,
        MEETING,
        &[
            ("start", "2026-12-02T12:00:00"),
            ("timezone", "America/New_York"),
            ("duration", "45"),
            ("topic", "Week 1 (moved)"),
            ("as", "application/json"),
        ],
        &grants(&[]),
    )
    .expect("updates");
    let v: Value = serde_json::from_slice(&out.bytes).unwrap();
    assert_eq!(v["start_time"], "2026-12-02T17:00:00Z");
    assert_eq!(v["duration"], 45);
    assert_eq!(v["topic"], "Week 1 (moved)");
    let api = zoom.api();
    assert_eq!(api[0].method, Method::Patch, "{api:?}");
    let sent = api[0].json();
    assert_eq!(sent["start_time"], "2026-12-02T17:00:00Z");
    assert_eq!(sent["timezone"], "America/New_York");
    // An update sends what changed and nothing else: no settings were given, so none are
    // reset to the creation defaults, and the meeting stays a single scheduled meeting.
    assert!(sent.get("settings").is_none(), "{sent}");
    assert!(sent.get("type").is_none(), "{sent}");
    assert!(sent.get("recurrence").is_none(), "{sent}");
    assert!(sent.get("agenda").is_none(), "{sent}");
    assert_eq!(api[1].method, Method::Get);
}

#[test]
fn an_update_with_nothing_to_change_is_refused_before_any_request() {
    let (zoom, keys) = (Zoom::new(), Arc::<Keystore>::default());
    let k = kernel(&zoom, &keys);
    let err = issue(&k, Verb::Sink, MEETING, &[], &grants(&[])).unwrap_err();
    assert!(matches!(err, Error::MissingArgument(_)), "{err:?}");
    assert!(zoom.seen().is_empty());
    assert!(keys.reads.lock().unwrap().is_empty());
}

#[test]
fn delete_cancels_and_a_cancelled_meeting_is_not_found() {
    let (zoom, keys) = (Zoom::new(), Arc::default());
    let k = kernel(&zoom, &keys);
    let out = text(issue(&k, Verb::Delete, MEETING, &[], &grants(&[])).expect("cancels"));
    assert!(out.contains(MEETING), "{out}");
    assert_eq!(zoom.api()[0].method, Method::Delete);
    for verb in [Verb::Source, Verb::Delete] {
        let err = issue(&k, verb, MEETING, &[], &grants(&[])).unwrap_err();
        assert!(matches!(err, Error::NotFound(_)), "{verb:?}: {err:?}");
    }
    let err = issue(&k, Verb::Sink, MEETING, &[("topic", "x")], &grants(&[])).unwrap_err();
    assert!(matches!(err, Error::NotFound(_)), "{err:?}");
}

#[test]
fn a_malformed_meeting_name_is_invalid_before_any_request() {
    let (zoom, keys) = (Zoom::new(), Arc::<Keystore>::default());
    let k = kernel(&zoom, &keys);
    for bad in [
        "urn:meeting:id:88899900011",
        "urn:meeting:id:zoom-",
        "urn:meeting:id:zoom-12ab",
        "urn:meeting:id:zoom-0",
        "urn:meeting:id:teams-123",
    ] {
        let err = issue(&k, Verb::Source, bad, &[], &grants(&[])).unwrap_err();
        assert!(
            matches!(err, Error::InvalidArgument { ref name, .. } if name == "m"),
            "{bad}: {err:?}"
        );
        assert!(!err.to_string().contains("12ab"), "{err}");
    }
    assert!(zoom.seen().is_empty());
    assert!(keys.reads.lock().unwrap().is_empty());
}

#[test]
fn settings_are_arguments_with_safe_defaults() {
    let (zoom, keys) = (Zoom::new(), Arc::default());
    let k = kernel(&zoom, &keys);
    let base = [("topic", "Open class"), ("start", "2026-12-01T17:00:00Z")];
    issue(&k, Verb::Sink, "urn:meeting:schedule", &base, &grants(&[])).expect("schedules");
    let sent = zoom.api()[0].json();
    let settings = &sent["settings"];
    assert_eq!(settings["waiting_room"], true, "{sent}");
    assert_eq!(settings["join_before_host"], false, "{sent}");
    assert_eq!(settings["meeting_authentication"], false, "{sent}");
    assert_eq!(settings["auto_recording"], "none", "{sent}");
    // One meeting per occurrence: always a single scheduled meeting, never a series.
    assert_eq!(sent["type"], 2);
    assert!(sent.get("recurrence").is_none(), "{sent}");

    let mut args = base.to_vec();
    args.extend([
        ("waiting_room", "false"),
        ("join_before_host", "true"),
        ("authenticated_join", "true"),
        ("auto_recording", "cloud"),
    ]);
    issue(&k, Verb::Sink, "urn:meeting:schedule", &args, &grants(&[])).expect("schedules");
    let sent = zoom.api()[1].json();
    let settings = &sent["settings"];
    assert_eq!(settings["waiting_room"], false, "{sent}");
    assert_eq!(settings["join_before_host"], true, "{sent}");
    assert_eq!(settings["meeting_authentication"], true, "{sent}");
    assert_eq!(settings["auto_recording"], "cloud", "{sent}");

    // On an update, only the settings given are sent.
    issue(
        &k,
        Verb::Sink,
        MEETING,
        &[("waiting_room", "false")],
        &grants(&[]),
    )
    .expect("updates");
    let sent = zoom.api()[2].json();
    assert_eq!(sent, json!({"settings": {"waiting_room": false}}));
}

#[test]
fn a_setting_outside_its_type_is_named_and_not_echoed() {
    let (zoom, keys) = (Zoom::new(), Arc::<Keystore>::default());
    let k = kernel(&zoom, &keys);
    for (name, value) in [
        ("waiting_room", "maybe"),
        ("join_before_host", "yes-please"),
        ("authenticated_join", "2"),
        ("auto_recording", "always-on"),
    ] {
        let args = [
            ("topic", "t"),
            ("start", "2026-12-01T17:00:00Z"),
            (name, value),
        ];
        let err = issue(&k, Verb::Sink, "urn:meeting:schedule", &args, &grants(&[])).unwrap_err();
        assert!(
            matches!(err, Error::InvalidArgument { name: ref n, .. } if n == name),
            "{name}: {err:?}"
        );
        assert!(!err.to_string().contains(value), "{err}");
    }
    assert!(zoom.seen().is_empty());
}

#[test]
fn a_teacher_on_the_shared_account_is_scheduled_as_their_own_user() {
    let (zoom, keys) = (Zoom::new(), Arc::<Keystore>::default());
    let k = kernel(&zoom, &keys);
    let args = [
        ("topic", "Week 1"),
        ("start", "2026-12-01T17:00:00Z"),
        ("teacher", "ada"),
    ];
    let cap = grants(&["urn:cap:secret:read:zoom-teacher-ada"]);
    issue(&k, Verb::Sink, "urn:meeting:schedule", &args, &cap).expect("schedules");
    let api = zoom.api();
    assert_eq!(
        api[0].url,
        "https://api.zoom.us/v2/users/ada%40example.invalid/meetings"
    );
    assert_eq!(api[0].authorization, "Bearer tok-1");
    let token = &zoom.seen()[0];
    assert_eq!(token.form()["grant_type"], "account_credentials");
}

#[test]
fn a_teacher_with_their_own_app_refreshes_and_keeps_the_rotated_token() {
    let (zoom, keys) = (Zoom::new(), Arc::<Keystore>::default());
    let k = kernel(&zoom, &keys);
    let args = [
        ("topic", "Week 1"),
        ("start", "2026-12-01T17:00:00Z"),
        ("teacher", "bea"),
    ];
    let cap = Capability::scoped(
        [
            "urn:cap:net:zoom.us",
            "urn:cap:net:api.zoom.us",
            "urn:cap:secret:read:zoom-teacher-bea",
            "urn:cap:secret:write:zoom-teacher-bea",
        ]
        .map(str::to_string),
    );
    issue(&k, Verb::Sink, "urn:meeting:schedule", &args, &cap).expect("schedules");
    let seen = zoom.seen();
    let form = seen[0].form();
    assert_eq!(form["grant_type"], "refresh_token");
    assert_eq!(form["refresh_token"], "refresh-1");
    // Basic base64("bea-cid:bea-csec"): the teacher's own app, not the shared one.
    assert_eq!(seen[0].authorization, "Basic YmVhLWNpZDpiZWEtY3NlYw==");
    assert_eq!(seen[1].url, "https://api.zoom.us/v2/users/me/meetings");
    assert_eq!(seen[1].authorization, "Bearer user-tok-1");
    // The shared account's credentials were never read.
    assert_eq!(
        *keys.reads.lock().unwrap(),
        vec!["zoom-teacher-bea".to_string()]
    );
    let stored = keys.stored.lock().unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].0, "zoom-teacher-bea");
    let record: Value = serde_json::from_slice(&stored[0].1).unwrap();
    assert_eq!(record["refresh_token"], "refresh-2");
    assert_eq!(record["client_id"], "bea-cid");
}

#[test]
fn a_teacher_is_reached_only_through_their_own_grant() {
    let (zoom, keys) = (Zoom::new(), Arc::<Keystore>::default());
    let k = kernel(&zoom, &keys);
    let args = [
        ("topic", "Week 1"),
        ("start", "2026-12-01T17:00:00Z"),
        ("teacher", "bea"),
    ];
    // Ada's grant does not reach Bea's record.
    let err = issue(
        &k,
        Verb::Sink,
        "urn:meeting:schedule",
        &args,
        &grants(&["urn:cap:secret:read:zoom-teacher-ada"]),
    )
    .unwrap_err();
    assert!(matches!(err, Error::Denied(_)), "{err:?}");
    assert!(err.to_string().contains("zoom-teacher-bea"), "{err}");
    assert!(keys.reads.lock().unwrap().is_empty());
    // Bea's own app rotates her refresh token, so keeping it needs write authority, refused
    // before any socket opens.
    let err = issue(
        &k,
        Verb::Sink,
        "urn:meeting:schedule",
        &args,
        &grants(&["urn:cap:secret:read:zoom-teacher-bea"]),
    )
    .unwrap_err();
    assert!(matches!(err, Error::Denied(_)), "{err:?}");
    assert!(
        err.to_string()
            .contains("urn:cap:secret:write:zoom-teacher-bea"),
        "{err}"
    );
    assert!(zoom.seen().is_empty());
    // A teacher name that cannot be a secret name is invalid, and not echoed.
    let err = issue(
        &k,
        Verb::Sink,
        "urn:meeting:schedule",
        &[
            ("topic", "Week 1"),
            ("start", "2026-12-01T17:00:00Z"),
            ("teacher", "../root"),
        ],
        &grants(&[]),
    )
    .unwrap_err();
    assert!(
        matches!(err, Error::InvalidArgument { ref name, .. } if name == "teacher"),
        "{err:?}"
    );
    assert!(!err.to_string().contains("../root"), "{err}");
}

#[test]
fn an_unknown_meeting_is_not_found() {
    let (zoom, keys) = (Zoom::new(), Arc::default());
    let k = kernel(&zoom, &keys);
    let err = issue(
        &k,
        Verb::Source,
        "urn:meeting:id:zoom-77700000000",
        &[],
        &grants(&[]),
    )
    .unwrap_err();
    assert!(matches!(err, Error::NotFound(_)), "{err:?}");
}
