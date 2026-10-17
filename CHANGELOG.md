# Changelog

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
