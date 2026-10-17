//! The Server-to-Server token is cached in-process (ledger #174): reused, keyed by the account,
//! until `TOKEN_REFRESH_MARGIN_SECS` before its `expires_in`, judged on the invocation's clock.
//!
//! [`FakeZoom`] is a scripted token endpoint and create-meeting endpoint behind the injected
//! transport: it mints `tok-fake-1`, `tok-fake-2`, … (one per exchange, so a test can tell a reused
//! token from a fresh one), states whatever `expires_in` the test sets, and answers `401` for any
//! token the test marks refused. [`Keystore`] answers the three credential names with fake values,
//! its account id switchable, and counts reads. [`Clock`] is a settable clock. No network, no real
//! credential.

use async_trait::async_trait;
use ikigai_core::{ArgRef, Capability, Clock, Error, Iri, Kernel, Request, Time, Verb};
use ikigai_http::{HttpRequest, HttpResponse, HttpTransport};
use ikigai_meeting::{SecretReader, ZoomConfig, TOKEN_REFRESH_MARGIN_SECS};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// The Zoom backend's own entry, and the facade that reaches the same endpoint.
const ZOOM: &str = "urn:meeting:zoom:schedule";
const FACADE: &str = "urn:meeting:schedule";
const T0: u64 = 1_700_000_000_000;

/// A token endpoint and a create-meeting endpoint, scripted.
struct FakeZoom {
    minted: AtomicUsize,
    /// The `expires_in` each token response states; `Value::Null` states none.
    expires_in: Mutex<Value>,
    /// Tokens the create-meeting endpoint answers `401` for.
    refused: Mutex<Vec<String>>,
    /// The bearer token of every create-meeting request, in order.
    bearers: Mutex<Vec<String>>,
}

impl FakeZoom {
    fn new(expires_in: Value) -> Arc<FakeZoom> {
        Arc::new(FakeZoom {
            minted: AtomicUsize::new(0),
            expires_in: Mutex::new(expires_in),
            refused: Mutex::new(Vec::new()),
            bearers: Mutex::new(Vec::new()),
        })
    }

    fn minted(&self) -> usize {
        self.minted.load(Ordering::SeqCst)
    }

    fn bearers(&self) -> Vec<String> {
        self.bearers.lock().unwrap().clone()
    }

    fn refuse(&self, token: &str) {
        self.refused.lock().unwrap().push(token.to_string());
    }
}

fn response(status: u16, body: Value) -> HttpResponse {
    HttpResponse {
        status,
        headers: vec![],
        body: serde_json::to_vec(&body).unwrap(),
    }
}

#[async_trait]
impl HttpTransport for FakeZoom {
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse, String> {
        if request.url.ends_with("/oauth/token") {
            let n = self.minted.fetch_add(1, Ordering::SeqCst) + 1;
            let mut body =
                json!({ "access_token": format!("tok-fake-{n}"), "token_type": "bearer" });
            let expires_in = self.expires_in.lock().unwrap().clone();
            if !expires_in.is_null() {
                body["expires_in"] = expires_in;
            }
            return Ok(response(200, body));
        }
        if request.url.ends_with("/meetings") {
            let bearer = request
                .headers
                .iter()
                .find(|(k, _)| k == "Authorization")
                .and_then(|(_, v)| v.strip_prefix("Bearer "))
                .unwrap_or("")
                .to_string();
            self.bearers.lock().unwrap().push(bearer.clone());
            if self.refused.lock().unwrap().contains(&bearer) {
                return Ok(response(
                    401,
                    json!({ "code": 124, "message": "Invalid access token." }),
                ));
            }
            return Ok(response(
                200,
                json!({
                    "id": 88899900011u64,
                    "join_url": "https://zoom.us/j/88899900011?pwd=abc",
                    "start_url": "https://zoom.us/s/88899900011?zak=fake",
                    "password": "a1b2c3",
                    "start_time": "2026-01-01T00:00:00Z"
                }),
            ));
        }
        Err(format!("unscripted url {}", request.url))
    }
}

/// Fake credentials; the account id can be switched mid-test.
struct Keystore {
    account_id: Mutex<String>,
    reads: AtomicUsize,
}

impl Keystore {
    fn new() -> Arc<Keystore> {
        Arc::new(Keystore {
            account_id: Mutex::new("acct-fake-1".to_string()),
            reads: AtomicUsize::new(0),
        })
    }

    fn switch_account(&self, account_id: &str) {
        *self.account_id.lock().unwrap() = account_id.to_string();
    }

    fn reads(&self) -> usize {
        self.reads.load(Ordering::SeqCst)
    }
}

impl SecretReader for Keystore {
    fn read(&self, name: &str) -> ikigai_core::Result<Vec<u8>> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        Ok(match name {
            "zoom-account-id" => self.account_id.lock().unwrap().clone().into_bytes(),
            "zoom-client-id" => b"cid-fake".to_vec(),
            "zoom-client-secret" => b"csec-fake".to_vec(),
            other => return Err(Error::Endpoint(format!("no secret `{other}`"))),
        })
    }
}

/// A clock a test can move.
struct TestClock(AtomicU64);

impl TestClock {
    fn set(&self, millis: u64) {
        self.0.store(millis, Ordering::SeqCst);
    }
}

impl Clock for TestClock {
    fn now(&self) -> Time {
        Time::from_millis(self.0.load(Ordering::SeqCst))
    }
}

/// A kernel over the fake, with a clock at `T0` (or none).
fn kernel(
    zoom: &Arc<FakeZoom>,
    keystore: &Arc<Keystore>,
    clock: Option<&Arc<TestClock>>,
) -> Kernel {
    let kernel = Kernel::new(Arc::new(ikigai_meeting::space(
        zoom.clone(),
        keystore.clone(),
        ZoomConfig::default(),
    )));
    match clock {
        Some(clock) => kernel.with_clock(clock.clone()),
        None => kernel,
    }
}

fn clock() -> Arc<TestClock> {
    Arc::new(TestClock(AtomicU64::new(T0)))
}

fn schedule(kernel: &Kernel, iri: &str, capability: &Capability) -> Result<String, Error> {
    let request = Request::new(Verb::Sink, Iri::parse(iri).unwrap())
        .with_arg("topic", ArgRef::Inline(b"Intro call".to_vec()))
        .with_arg("start", ArgRef::Inline(b"2026-01-01T00:00:00Z".to_vec()));
    futures::executor::block_on(kernel.issue(request, capability))
        .map(|r| String::from_utf8(r.bytes).unwrap())
}

fn ok(kernel: &Kernel, iri: &str) {
    schedule(kernel, iri, &Capability::root()).unwrap_or_else(|e| panic!("{iri}: {e}"));
}

/// One exchange serves every schedule until `TOKEN_REFRESH_MARGIN_SECS` before the stated
/// expiry — through either entry, which share the backend — and the next one after that mints.
/// The credentials are still read on every call.
#[test]
fn a_token_is_reused_until_the_margin_before_it_expires() {
    let zoom = FakeZoom::new(json!(3600));
    let keystore = Keystore::new();
    let clock = clock();
    let kernel = kernel(&zoom, &keystore, Some(&clock));

    ok(&kernel, ZOOM);
    ok(&kernel, FACADE);
    let last_good = T0 + (3600 - TOKEN_REFRESH_MARGIN_SECS) * 1000 - 1;
    clock.set(last_good);
    ok(&kernel, ZOOM);
    assert_eq!(zoom.minted(), 1, "one exchange for three schedules");
    assert_eq!(zoom.bearers(), ["tok-fake-1"; 3]);
    assert_eq!(keystore.reads(), 9, "three reads per call, cached or not");

    clock.set(last_good + 1);
    ok(&kernel, ZOOM);
    ok(&kernel, ZOOM);
    assert_eq!(
        zoom.minted(),
        2,
        "at the margin the token is replaced, once"
    );
    assert_eq!(
        zoom.bearers(),
        [
            "tok-fake-1",
            "tok-fake-1",
            "tok-fake-1",
            "tok-fake-2",
            "tok-fake-2"
        ]
    );
}

/// A kernel with no clock has no time to judge a token by, so every call mints (the behavior
/// before the cache, and what the conformance fixture's footprint pins).
#[test]
fn without_a_clock_every_call_mints() {
    let zoom = FakeZoom::new(json!(3600));
    let kernel = kernel(&zoom, &Keystore::new(), None);
    ok(&kernel, ZOOM);
    ok(&kernel, ZOOM);
    assert_eq!(zoom.minted(), 2);
    assert_eq!(zoom.bearers(), ["tok-fake-1", "tok-fake-2"]);
}

/// The cache is keyed by account: another account id mints its own token, and switching back
/// finds the first one still cached.
#[test]
fn tokens_are_keyed_by_account() {
    let zoom = FakeZoom::new(json!(3600));
    let keystore = Keystore::new();
    let kernel = kernel(&zoom, &keystore, Some(&clock()));
    ok(&kernel, ZOOM);
    keystore.switch_account("acct-fake-2");
    ok(&kernel, ZOOM);
    keystore.switch_account("acct-fake-1");
    ok(&kernel, ZOOM);
    assert_eq!(zoom.minted(), 2, "one token per account");
    assert_eq!(zoom.bearers(), ["tok-fake-1", "tok-fake-2", "tok-fake-1"]);
}

/// A lifetime no longer than the margin, or none stated, is not cached; a lifetime stated as a
/// string is read.
#[test]
fn a_short_or_unstated_lifetime_is_not_cached() {
    for (expires_in, mints) in [
        (json!(TOKEN_REFRESH_MARGIN_SECS), 2),
        (json!(1), 2),
        (Value::Null, 2),
        (json!("not a number"), 2),
        (json!(TOKEN_REFRESH_MARGIN_SECS + 1), 1),
        (json!("3600"), 1),
    ] {
        let zoom = FakeZoom::new(expires_in.clone());
        let kernel = kernel(&zoom, &Keystore::new(), Some(&clock()));
        ok(&kernel, ZOOM);
        ok(&kernel, ZOOM);
        assert_eq!(zoom.minted(), mints, "expires_in = {expires_in}");
    }
}

/// A create-meeting `401` for a CACHED token drops it and tries once more with a fresh one; the
/// fresh one is then the cached one. A `401` for a fresh token is an error, with no further try.
#[test]
fn a_refused_cached_token_is_dropped_and_retried_once() {
    let zoom = FakeZoom::new(json!(3600));
    let kernel = kernel(&zoom, &Keystore::new(), Some(&clock()));
    ok(&kernel, ZOOM);
    zoom.refuse("tok-fake-1");
    ok(&kernel, ZOOM);
    ok(&kernel, ZOOM);
    assert_eq!(zoom.minted(), 2);
    assert_eq!(
        zoom.bearers(),
        ["tok-fake-1", "tok-fake-1", "tok-fake-2", "tok-fake-2"]
    );

    // Refuse the cached token AND the next one minted: one retry, then the error.
    zoom.refuse("tok-fake-2");
    zoom.refuse("tok-fake-3");
    let err = schedule(&kernel, ZOOM, &Capability::root()).unwrap_err();
    assert!(matches!(err, Error::Endpoint(_)), "{err:?}");
    assert!(err.to_string().contains("401"), "{err}");
    assert!(!err.to_string().contains("tok-fake"), "{err}");
    assert_eq!(zoom.minted(), 3, "one fresh token, no loop");
    assert_eq!(&zoom.bearers()[4..], ["tok-fake-2", "tok-fake-3"]);
}

/// A cached token skips only the exchange: the module's authority is still checked first, so a
/// caller without the Zoom hosts is refused before anything is sent.
#[test]
fn authority_is_checked_on_every_call_even_with_a_cached_token() {
    let zoom = FakeZoom::new(json!(3600));
    let keystore = Keystore::new();
    let kernel = kernel(&zoom, &keystore, Some(&clock()));
    ok(&kernel, ZOOM);
    let reads = keystore.reads();
    let other_host = Capability::scoped(
        [
            "urn:cap:net:example.invalid",
            "urn:cap:secret:read:zoom-account-id",
            "urn:cap:secret:read:zoom-client-id",
            "urn:cap:secret:read:zoom-client-secret",
        ]
        .map(str::to_string),
    );
    let err = schedule(&kernel, ZOOM, &other_host).unwrap_err();
    assert!(matches!(err, Error::Denied(_)), "{err:?}");
    assert_eq!(zoom.bearers().len(), 1, "nothing sent under the refusal");
    assert_eq!(keystore.reads(), reads, "nothing read under the refusal");
}
