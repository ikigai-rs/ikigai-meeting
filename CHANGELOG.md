# Changelog

## 0.4.0

### Changed (BREAKING: a minor release, not a patch)

- **ikigai-http 0.2.1 → 0.3.0, ikigai-core 0.1.86 → 0.1.108** (ledger #1546, #1917).
  `ikigai_http::HttpTransport` is in `space()`'s signature, so a host hands this crate a transport
  from the ikigai-http 0.3 line, the same line it serves `urn:http:*` from; a host still on 0.2
  cannot pass its transport to this release. That is the breaking change, and why this is a minor.
- **Zoom's error answers are typed by ikigai-http's one status table** (`ikigai_http::status`)
  instead of this crate's own `>= 400 → Endpoint` rule. What a caller sees, before → after:
  - a `401`/`403` from the token endpoint or the create-meeting call: `Error::Endpoint` →
    **`Error::Refused`** (class `refused`; display `refused: zoom oauth returned 401: …`). It
    was never `Error::Denied` here: `Denied` stays the capability floor's word, for a caller whose
    grant lacks a Zoom host or a credential name.
  - a `429`: `Error::Endpoint` → **`Error::RetryAfter`**, carrying the `Retry-After` delay when the
    header parses (delay-seconds, or an HTTP-date measured on the kernel's clock), so an HTTP edge
    answers 429 with `Retry-After` and a budget-holding overlay can wait.
  - a `502`, and a `503` without a usable `Retry-After`: `Error::Endpoint` →
    **`Error::Unavailable`**; a `408`/`504`: → **`Error::Timeout`**. Both transient, so
    ikigai-throttle's Retry, CircuitBreaker and Failover now engage in front of this module.
  - a `503` with `Retry-After`: → **`Error::RetryAfter`**; a `413`/`414`/`431`: →
    **`Error::OverBudget`**; a `404`/`410`: → **`Error::NotFound`**; a `409`/`412`: →
    **`Error::Conflict`**; a `501`: → **`Error::Unsupported { verb: None, .. }`**.
  - a `500`, every other 5xx and every other 4xx: `Error::Endpoint`, unchanged (class `unknown`:
    an origin's 500 is no promise that a repeat helps).
  - a transport that got no answer: `Error::Endpoint` → **`Error::Timeout`** when its words say it
    timed out, **`Error::Unavailable`** otherwise (ikigai-http reads the words with the URL removed,
    so a host named `timeout.example` cannot type its own refusals).
  The detail text is unchanged in substance: this module's words, the body's `message` kept and
  its echo dropped, every credential scrubbed. A `401` for a CACHED token still drops it and
  tries once more with a fresh one; the second `401` is the `Refused` above, terminal.
- **The Sink declares its failure classes** (`Description::fails`, core 0.1.108): `transient`,
  `retry-after`, `over-budget`, `refused` and `unknown`, with `permanent` and (beside the two
  `requires`) `denied` implied. The action's contract IRI changes with the declaration.
- **ikigai-conformance 0.8.0 → 0.10.0** (dev-dependency): the walk opts into FAILURES
  (`Suite::check_failures`) and two `fails_under` probes hold the Sink to `retry-after` under a
  stub answering `429` and `refused` under one answering `401`.
- **The conformance test's private base64 encoder is gone**: the Basic header it checks for is
  built by `ikigai_http::auth::basic`, the shared RFC 7617 builder (ledger #1857).

## 0.3.1

### Changed

- **The Zoom OAuth call builds its `Authorization: Basic` header with ikigai-http's shared
  builder, `ikigai_http::auth::basic`** (ledger #1857), instead of a hand-rolled base64 encoder.
  The private `base64_encode` and its RFC 4648 test are gone: the shared vectors in ikigai-http
  (which the Python and Deno faces pin too) cover the encoding now. The header bytes for a valid
  client id and secret are unchanged.
- **A `client_id` containing `:` is now refused** (RFC 7617 §2: the first colon separates the user
  id from the password, so such an id cannot be encoded). So is a control character in the client
  id or secret. The refusal happens before any request is sent, and its text names the rule and
  its stable code (`basic-user-id-colon`, `basic-user-id-control`, `basic-password-control`),
  never the id or the secret. Before, such an id was encoded as given, and Zoom would have read
  the wrong id.
- **ikigai-http floor 0.2.0 → 0.2.1**, the release that carries `ikigai_http::auth`. Still the
  0.2 line, so a host's transport passes through unchanged and this is a patch release.

## 0.3.0

### Changed (BREAKING: a minor release, not a patch)

- **ikigai-http 0.1.7 → 0.2.0** (the kebab-case rename, ledger #11). `ikigai_http::HttpTransport`
  is in this crate's public API: `space()` takes an `Arc<dyn HttpTransport>`. A host builds ONE
  transport and hands it both to ikigai-http's `space()` (for `urn:http:get` and its siblings) and
  to this one, so it must name the same ikigai-http line for both. A host on ikigai-http 0.2 cannot
  pass its transport to a meeting built on 0.1, and a host still on 0.1 cannot pass its transport
  to this one. That is a breaking change for consumers, so the release is a minor bump
  (0.2.0 → 0.3.0), not a patch.
- **ikigai-core floor 0.1.47 → 0.1.86.** This crate's own code still compiles against 0.1.47, but
  ikigai-http 0.2.0 pins `ikigai-core = "0.1.86"`, so no graph holding it resolves lower. The new
  floor is checked by a fresh resolve at `=0.1.86` beside `ikigai-http = "=0.2.0"`.
- No old http names were in use here: this crate calls Zoom through the transport and never
  resolves `urn:httpGet`-style IRIs or matches on the renamed ids.

## 0.2.0

### Renamed (BREAKING for anyone matching on ids or MCP tool names)

The endpoint's description id is now a short kebab-case noun, per ikigai-core's naming
conventions (ledger #11). The id is not an IRI, so nothing resolves by it, but it is what the MCP
projection turns into an agent's tool name and what selection, catalog labels and conformance
fixtures key on.

| what | old | new |
| --- | --- | --- |
| `Description::id` | `urn:meeting:zoom:schedule` | `meeting` |
| catalog label (`Endpoint::name`) | `meeting` | `meeting` (unchanged; it now equals the id) |
| bound IRIs | `urn:meeting:schedule`, `urn:meeting:zoom:schedule` | unchanged |

`ikigai-conformance`'s `NAMES` check now runs over this module and is clean.

### Added

- `attendees`: who the meeting is for, sent to Zoom as the meeting's invitees and stated as
  `ical:attendee <mailto:…>` in the Turtle face.
- The Server-to-Server token is cached in-process until a minute before its `expires_in`.

## 0.1.2

- The Zoom account id is sent in the token request's form body, not its URL query.
- ikigai-core floor back to the library's true minimum.

## 0.1.1

- Adopt ikigai-conformance: typed inputs, declared capabilities gated first, `content`, scrubbed
  errors.

## 0.1.0

- First release: `urn:meeting:schedule` (facade) and `urn:meeting:zoom:schedule` (Zoom backend).
