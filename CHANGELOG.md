# Changelog

## Unreleased

### Changed (BREAKING: the next release is 0.3.0, not a patch)

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
