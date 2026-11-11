//! Hermetic Zoom contract audit: an injected stub records wire requests and scripts replies.
use async_trait::async_trait;
use futures::executor::block_on;
use ikigai_core::{
    ArgRef, Capability, Clock, Error, Expiry, Iri, Kernel, Representation, Request, Time, Verb,
};
use ikigai_http::{HttpRequest, HttpResponse, HttpTransport};
use ikigai_meeting::{space, SecretReader, ZoomConfig};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

struct Secrets;
impl SecretReader for Secrets {
    fn read(&self, _: &str) -> ikigai_core::Result<Vec<u8>> {
        Ok(b"fake".to_vec())
    }
}
struct Stub {
    calls: AtomicUsize,
    reply: Value,
    seen: Mutex<Vec<Value>>,
}
#[async_trait]
impl HttpTransport for Stub {
    async fn send(&self, req: HttpRequest) -> Result<HttpResponse, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let body = if req.url.ends_with("/oauth/token") {
            json!({"access_token":"fake-token", "expires_in":3600})
        } else {
            self.seen
                .lock()
                .unwrap()
                .push(serde_json::from_slice(&req.body).unwrap());
            self.reply.clone()
        };
        Ok(HttpResponse {
            status: 200,
            headers: vec![],
            body: serde_json::to_vec(&body).unwrap().into(),
        })
    }
}
fn reply() -> Value {
    json!({"id":123, "join_url":"https://example.invalid/j/123?pwd=encrypted", "start_url":"https://example.invalid/s/123?zak=host", "password":"00123", "topic":"Zoom title", "start_time":"2026-07-30T22:00:00Z", "duration":45})
}
fn setup(reply: Value) -> (Kernel, Arc<Stub>) {
    let stub = Arc::new(Stub {
        calls: AtomicUsize::new(0),
        reply,
        seen: Mutex::default(),
    });
    let kernel = Kernel::new(Arc::new(space(
        stub.clone(),
        Arc::new(Secrets),
        ZoomConfig::default(),
    )));
    (kernel, stub)
}
fn issue(k: &Kernel, args: &[(&str, &[u8])]) -> ikigai_core::Result<Representation> {
    let mut req = Request::new(Verb::Sink, Iri::parse("urn:meeting:zoom:schedule").unwrap())
        .with_arg("topic", ArgRef::Inline(b"Requested title".to_vec()))
        .with_arg("start", ArgRef::Inline(b"2026-07-30T22:00:00Z".to_vec()))
        .with_arg("as", ArgRef::Inline(b"application/json".to_vec()));
    for (name, value) in args {
        req = req.with_arg(*name, ArgRef::Inline(value.to_vec()));
    }
    block_on(k.issue(req, &Capability::root()))
}
fn invalid_before_http(name: &str, value: &[u8], extra: &[(&str, &[u8])]) {
    let (k, stub) = setup(reply());
    let mut args = extra.to_vec();
    args.push((name, value));
    let result = issue(&k, &args);
    assert!(
        matches!(result, Err(Error::InvalidArgument {name:ref n,..}) if n == name),
        "{name}: {result:?}"
    );
    assert_eq!(stub.calls.load(Ordering::SeqCst), 0);
}
#[test]
fn invalid_calendar_dates_and_offsets_are_refused() {
    for start in [
        "2026-02-29T22:00:00Z",
        "2026-13-01T00:00:00Z",
        "2026-07-30T25:00:00Z",
        "2026-07-30T22:60:00Z",
        "2026-07-30T22:00:00+24:00",
        "2026-07-30T22:00:00+01:60",
    ] {
        invalid_before_http("start", start.as_bytes(), &[]);
    }
}
#[test]
fn unknown_timezones_are_refused() {
    invalid_before_http("timezone", b"America/Not_A_Zone", &[]);
}
#[test]
fn dst_gaps_and_overlaps_require_an_explicit_offset() {
    for start in ["2026-03-08T02:30:00", "2026-11-01T01:30:00"] {
        invalid_before_http(
            "start",
            start.as_bytes(),
            &[("timezone", b"America/Los_Angeles")],
        );
    }
}
#[test]
fn starts_are_sent_as_unambiguous_instants() {
    for (start, zone, expected) in [
        (
            "2026-07-30T15:00:00-07:00",
            "America/Los_Angeles",
            "2026-07-30T22:00:00Z",
        ),
        (
            "2026-07-30T22:00:00Z",
            "America/Los_Angeles",
            "2026-07-30T22:00:00Z",
        ),
        (
            "2026-07-30T15:00:00",
            "America/Los_Angeles",
            "2026-07-30T22:00:00Z",
        ),
        (
            "2026-07-30T15:00:00.125-07:00",
            "America/Los_Angeles",
            "2026-07-30T22:00:00.125Z",
        ),
        (
            "2026-11-01T01:30:00-07:00",
            "America/Los_Angeles",
            "2026-11-01T08:30:00Z",
        ),
        (
            "2026-11-01T01:30:00-08:00",
            "America/Los_Angeles",
            "2026-11-01T09:30:00Z",
        ),
    ] {
        let (k, stub) = setup(reply());
        issue(
            &k,
            &[("start", start.as_bytes()), ("timezone", zone.as_bytes())],
        )
        .unwrap();
        let seen = stub.seen.lock().unwrap();
        assert_eq!(seen[0]["start_time"], expected);
        assert_eq!(seen[0]["timezone"], zone);
    }
}
#[test]
fn duration_is_bounded_by_the_zoom_contract() {
    for duration in [
        "0",
        "-1",
        "1.5",
        "1441",
        "4294967295",
        "999999999999999999999",
    ] {
        invalid_before_http("duration", duration.as_bytes(), &[]);
    }
}
#[test]
fn invalid_present_optional_values_do_not_become_defaults() {
    for name in [
        "duration",
        "timezone",
        "attendees",
        "agenda",
        "content",
        "as",
    ] {
        invalid_before_http(name, &[0xff], &[]);
    }
    for name in ["duration", "timezone", "attendees", "as"] {
        invalid_before_http(name, b"  ", &[]);
    }
}
#[test]
fn the_answer_uses_the_created_meetings_fields() {
    let (k, _) = setup(reply());
    let result: Value = serde_json::from_slice(&issue(&k, &[]).unwrap().bytes).unwrap();
    assert_eq!(result["topic"], "Zoom title");
    assert_eq!(result["duration"], 45);
    assert_eq!(result["start_time"], "2026-07-30T22:00:00Z");
    assert_eq!(result["passcode"], "00123");
}
#[test]
fn missing_null_and_mistyped_response_fields_are_reported() {
    for field in [
        "id",
        "join_url",
        "start_url",
        "topic",
        "start_time",
        "duration",
    ] {
        for bad in [None, Some(Value::Null), Some(json!([]))] {
            let mut body = reply();
            match bad {
                None => {
                    body.as_object_mut().unwrap().remove(field);
                }
                Some(v) => body[field] = v,
            }
            let (k, _) = setup(body);
            let result = issue(&k, &[]);
            assert!(
                matches!(result, Err(Error::Endpoint(ref text)) if text.contains(field)),
                "{field}: {result:?}"
            );
        }
    }
}
#[test]
fn invalid_response_start_time_is_reported() {
    let mut body = reply();
    body["start_time"] = json!("2026-02-30T00:00:00Z");
    let (k, _) = setup(body);
    assert!(matches!(issue(&k, &[]), Err(Error::Endpoint(_))));
}
#[test]
fn an_absent_passcode_is_reported_as_absent() {
    let mut body = reply();
    body.as_object_mut().unwrap().remove("password");
    let (k, _) = setup(body);
    let result: Value = serde_json::from_slice(&issue(&k, &[]).unwrap().bytes).unwrap();
    assert_eq!(result["passcode"], Value::Null);
}
#[test]
fn duration_default_and_boundaries_reach_the_wire() {
    for (args, expected) in [
        (vec![], 30),
        (vec![("duration", b"1".as_slice())], 1),
        (vec![("duration", b"1440".as_slice())], 1440),
    ] {
        let (k, stub) = setup(reply());
        issue(&k, &args).unwrap();
        assert_eq!(stub.seen.lock().unwrap()[0]["duration"], expected);
    }
}
struct At(u64);
impl Clock for At {
    fn now(&self) -> Time {
        Time::from_millis(self.0)
    }
}
#[test]
fn schedule_answers_are_never_cached_for_any_start_or_zone() {
    for start in [
        "2000-01-01T00:00:00Z",
        "2026-07-30T22:00:00Z",
        "9999-01-01T00:00:00+09:00",
    ] {
        let (k, stub) = setup(reply());
        let k = k.with_clock(Arc::new(At(1_785_448_800_000)));
        for _ in 0..2 {
            let result = issue(
                &k,
                &[("start", start.as_bytes()), ("timezone", b"Asia/Tokyo")],
            )
            .unwrap();
            assert_eq!(result.expiry, Expiry::Always);
        }
        assert_eq!(stub.seen.lock().unwrap().len(), 2);
    }
}

#[test]
fn the_answer_uses_the_created_duration() {
    let (k, _) = setup(reply());
    let result: Value = serde_json::from_slice(&issue(&k, &[]).unwrap().bytes).unwrap();
    assert_eq!(result["duration"], 45);
}
#[test]
fn present_invalid_required_values_are_not_reported_as_missing() {
    for name in ["topic", "start"] {
        invalid_before_http(name, &[0xff], &[]);
    }
}
#[test]
fn malformed_ids_and_passcodes_are_refused() {
    for (field, value) in [
        ("id", json!("")),
        ("id", json!(-1)),
        ("id", json!("abc")),
        ("password", json!(123)),
    ] {
        let mut body = reply();
        body[field] = value;
        let (k, _) = setup(body);
        assert!(
            matches!(issue(&k, &[]), Err(Error::Endpoint(ref text)) if text.contains(field)),
            "{field}"
        );
    }
}
#[test]
fn a_blank_named_agenda_wins_over_content() {
    let (k, stub) = setup(reply());
    issue(&k, &[("agenda", b""), ("content", b"fallback")]).unwrap();
    assert_eq!(stub.seen.lock().unwrap()[0]["agenda"], "");
}

#[test]
fn local_time_without_a_zone_uses_the_zoom_account_default() {
    let (k, stub) = setup(reply());
    issue(&k, &[("start", b"2026-07-30T15:00:00.5")]).unwrap();
    let seen = stub.seen.lock().unwrap();
    assert_eq!(seen[0]["start_time"], "2026-07-30T15:00:00.5");
    assert!(seen[0].get("timezone").is_none());
}

#[test]
fn normalization_preserves_fractional_digits_and_crosses_day_boundaries() {
    for (start, expected) in [
        ("2026-07-30T22:00:00.5Z", "2026-07-30T22:00:00.5Z"),
        (
            "2026-07-30T22:00:00.123456789123-07:00",
            "2026-07-31T05:00:00.123456789123Z",
        ),
        ("2026-07-30T01:00:00+14:00", "2026-07-29T11:00:00Z"),
        (
            "2026-07-30T15:00:00.123456789123",
            "2026-07-30T22:00:00.123456789123Z",
        ),
    ] {
        let (k, stub) = setup(reply());
        issue(
            &k,
            &[
                ("start", start.as_bytes()),
                ("timezone", b"America/Los_Angeles"),
            ],
        )
        .unwrap();
        assert_eq!(stub.seen.lock().unwrap()[0]["start_time"], expected);
    }
}

#[test]
fn response_start_times_use_utc_even_for_a_named_zone() {
    let mut body = reply();
    body["start_time"] = json!("2026-07-30T15:00:00-07:00");
    let (k, _) = setup(body);
    let result = issue(&k, &[("timezone", b"America/Los_Angeles")]).unwrap();
    let result: Value = serde_json::from_slice(&result.bytes).unwrap();
    assert_eq!(result["start_time"], "2026-07-30T22:00:00Z");
}

#[test]
fn null_and_empty_passcodes_are_distinguished() {
    for password in [Value::Null, json!("")] {
        let mut body = reply();
        body["password"] = password.clone();
        let (k, _) = setup(body);
        let result: Value = serde_json::from_slice(&issue(&k, &[]).unwrap().bytes).unwrap();
        assert_eq!(result["passcode"], password);
    }
}
