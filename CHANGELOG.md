# Changelog

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
