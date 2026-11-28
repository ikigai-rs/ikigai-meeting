# ikigai-meeting

Video-conference scheduling as [ikigai](https://ikigai-rs.dev) ROC resources.

`urn:meeting:schedule` is a provider-agnostic **facade** that dispatches to pluggable backends
`urn:meeting:<provider>:schedule` — the same shape as `urn:llm:ask`. **One backend ships today:
Zoom**, over Server-to-Server OAuth. Adding another means adding an endpoint under its own
provider IRI; the facade needs no change.

Each meeting is then a resource, **`urn:meeting:id:{m}`**: Source reads it, Sink moves or
changes it, Delete cancels it, so a moved session moves its meeting. `{m}` is provider-neutral
and names its provider: `zoom-88899900011` is Zoom meeting 88899900011.

The scheduled meeting's join URL is emitted as **`ical:conference`** (RFC 7986's CONFERENCE
property), so one value flows into the `.ics`, the calendar event, and — later — an org entry,
without re-plumbing at each destination.

## Faces

`Sink urn:meeting:zoom:schedule topic=… start=… [duration=30] [timezone=…] [attendees=…] [agenda=…] [teacher=…] [settings…] [as=…]`

Every input is typed in the manifold (`topic` `xsd:string`, `start` `xsd:dateTime`, `duration`
`xsd:integer`, …) and held to its type: a garbled value is a typed `InvalidArgument` that names
the input and never echoes the value. A piped value — or a top-level `sink`'s body — arrives as
`content` and is the **agenda** (a named `agenda` wins). `attendees` lists who the meeting is
for, as email addresses separated by commas, semicolons or whitespace; Zoom receives them as the
meeting's invitees.

- default (`text/plain`) → the **join URL** (what an invite needs)
- `as=text/turtle` → the **shareable graph** (join-safe: an `ical:Vevent` with `ical:conference`,
  `dcterms:identifier`, `schema:provider`, start time, duration, the passcode, one `ical:attendee <mailto:…>` per
  attendee — the host start URL is deliberately withheld)
- `as=application/json` → the **full envelope**: the meeting's resource name (`meeting`) and
  Zoom's fields. The host `start_url` (which carries a start token) is in it **only for the
  presenter**: a caller holding `urn:cap:meeting:host:id:{m}` for that meeting, or
  `urn:cap:meeting:host:teacher:{t}` for the teacher named in `teacher`. For anyone else the
  key is absent.

Every schedule makes **one meeting** (Zoom `type: 2`), never a recurring series: one meeting
per occurrence, the caller expanding any recurrence itself.

### Settings are arguments

| Argument | Type | Default for a new meeting | Zoom field |
| --- | --- | --- | --- |
| `waiting_room` | `xsd:boolean` | `true` | `settings.waiting_room` |
| `join_before_host` | `xsd:boolean` | `false` | `settings.join_before_host` |
| `authenticated_join` | `xsd:boolean` | `false` | `settings.meeting_authentication` |
| `auto_recording` | `none`, `local` or `cloud` | `none` | `settings.auto_recording` |

A boolean is `true`, `false`, `1` or `0`. A value outside its type is an `InvalidArgument` that
names the setting. There is no `captions` argument: Zoom's meeting API has no per-meeting
captions field, and applies the host's account or user captions setting. A caller that needs
captions sets them there.

### One meeting as a resource

- `Source urn:meeting:id:{m} [teacher=…] [as=…]`: the meeting's details from Zoom
  (`GET /meetings/{id}`), in the same three faces as a schedule. A read is never cached: Zoom's
  copy can change without this module.
- `Sink urn:meeting:id:{m} [topic=…] [start=…] [duration=…] [timezone=…] [agenda=…] [attendees=…] [settings…] [teacher=…] [as=…]`:
  sends only what is given (`PATCH /meetings/{id}`), then answers the meeting Zoom now holds.
  A setting not given is left as it is; the defaults above apply only to a new meeting. Giving
  nothing to change is a `MissingArgument`, before any request.
- `Delete urn:meeting:id:{m} [teacher=…]`: cancels the meeting (`DELETE /meetings/{id}`) and
  answers `Canceled meeting urn:meeting:id:{m}.` as `text/plain`.

**Only the meeting's host's account reaches it.** Before an update, a cancel, or a read with
`teacher=<t>`, the module reads who the account acts for (`GET /users/{user_id}` for a teacher
on the shared account, `GET /users/me` for a teacher's own app or for no teacher) and the
meeting, and refuses with `Denied` unless that user's `id` is the meeting's `host_id`. The
denial names the teacher (or the configured user) and the meeting; it does not name the
meeting's host, whose Zoom identity is not the caller's to see. So teacher `bea` cannot read
the host link of, change or cancel a meeting teacher `ada` hosts, even through the shared
account's token, and a call that names no teacher changes only the configured user's
meetings. A read without `teacher` is not checked: it answers no host link without
`urn:cap:meeting:host:id:{m}`.

A `{m}` that is not `zoom-` and a Zoom meeting id's digits is an `InvalidArgument` naming `m`.
A meeting Zoom does not hold (never made, or canceled) is `NotFound`. Zoom documents a rate
limit of 100 update requests a day.

`start` must be a real calendar date and time. An explicit `Z` or numeric offset fixes the
instant; it is sent to Zoom in UTC with fractional digits preserved. A time without an offset
is resolved in the supplied IANA `timezone`. Unknown zones and local times in a daylight saving
gap or overlap are refused; use an explicit offset to select an instant during an overlap.
Without either an offset or `timezone`, Zoom uses the account's timezone, so this module cannot
check that zone's daylight saving transitions. The JSON and Turtle answers report Zoom's
returned start time in UTC, including when Zoom adjusts a past start to the current time.
These wire forms follow the [Zoom Meetings API](https://developers.zoom.us/docs/api/meetings/).

`duration` is whole minutes from 1 through 1440; the module's default is 30. Only an absent
optional argument selects a default. Invalid UTF-8 and blank duration, timezone, attendees,
or face values are refused. A blank named agenda is an explicit empty agenda and wins over
piped content.

The answer's topic, duration, and start time come from the created meeting's response.
Missing, null, or malformed required response fields are reported as endpoint errors rather
than replaced with request values. A missing or null Zoom `password` is JSON `passcode: null`;
a returned string retains its spelling, including leading zeros. No schedule answer is cached:
every invocation creates a meeting. `ttl_str` in the implementation escapes Turtle strings;
it does not calculate a cache lifetime.

## Injection, not dependencies

Like ikigai-llm, no HTTP client and no keystore are baked in. The host injects:

- an `ikigai_http::HttpTransport` (ureq/reqwest natively, `fetch` in the browser) from the
  ikigai-http 0.6 line (on ikigai-core 0.3), the same line the host serves `urn:http:*` from.
  The transport may stream the body (`HttpBody::from_reader`); this crate reads at most 1 MiB of
  a Zoom answer and answers a longer one `OverBudget`, and
- a `SecretReader` — the host resolves `urn:secret:<name>` under a capability and hands the bytes
  in, so the Zoom credentials stay in the OS keystore and this crate never links it. Its
  `store` method has a default that refuses; a host whose teachers use their own Zoom app
  implements it (see "Per-teacher accounts").

## Per-teacher accounts

`teacher=<t>` makes the call act in teacher `t`'s Zoom account. Without it, the call uses the
shared Server-to-Server account and the configured `user_id` (`me` by default), as before.
Each teacher's record is a secret in the keystore, named `zoom-teacher-<t>`
(`ZoomConfig::teacher_secret_prefix`). It is a JSON object of one of two shapes:

- `{"user_id": "teacher@example.org"}`: the shared Server-to-Server app acts for this Zoom user
  in its account. Meetings are created at `/users/<user_id>/meetings`.
- `{"client_id": "…", "client_secret": "…", "refresh_token": "…"}`: the teacher's own
  user-level OAuth app, for a teacher with their own Zoom account. The access token comes from
  the refresh token (`grant_type=refresh_token`). Zoom answers each refresh with a new refresh
  token, and the next refresh must use it, so the module stores the record again with the new
  token before it uses the access token. If the keystore cannot store it, the call fails and
  says that the teacher must authorize the app again.

A teacher name is 1 to 64 ASCII letters, digits, `-`, `_` or `.`, and does not start with `.`.
A record of another shape is an endpoint error that names the record and the shapes it can take,
and never quotes it.

## The token is cached

The Server-to-Server token (about an hour) is kept in-process, keyed by account, and reused until
a minute before its `expires_in`. Time is the kernel's clock: a host that installs none gets a
fresh token on every call. A cached token Zoom refuses (`401`) is dropped and the call is tried
once more with a fresh one.

## Capabilities

Every action declares `urn:cap:net:*` and `urn:cap:secret:read:*`, so the kernel refuses a caller
holding no grant under either family before the endpoint runs. The backend's own rules then
require the two Zoom hosts (`urn:cap:net:zoom.us`, `urn:cap:net:api.zoom.us` — port-aware) and
each credential's name (`urn:cap:secret:read:zoom-account-id`, `…:zoom-client-id`,
`…:zoom-client-secret`). Every refusal is a typed `Denied` issued **before any secret is read and
before any socket opens**; the injected `SecretReader` is never asked.

With `teacher=<t>`, the teacher's record is read only under `urn:cap:secret:read:zoom-teacher-<t>`,
so one teacher's grant never reaches another teacher's account. What the record selects is then
checked before any other secret is read and before any socket opens: the three shared names for a
teacher acting through the shared account, or `urn:cap:secret:write:zoom-teacher-<t>` for a
teacher's own app, whose rotated refresh token must be stored.

The host link is sealed to the presenter by `urn:cap:meeting:host:id:{m}` (one meeting) or
`urn:cap:meeting:host:teacher:{t}` (every meeting in the teacher's account). The meeting or
teacher goes last, so one meeting's grant never reaches another.

An error never carries a credential or the request's text: a server's 4xx contributes only its
`message`, a transport's text is scrubbed of every credential value, an invalid input is named,
not echoed.

## How a schedule can fail

Zoom's answers are typed by ikigai-http's one status table (`ikigai_http::status`), so they mean
the same thing here as an origin's answer anywhere else in the ecosystem: a `401`/`403` is
`Error::Refused` (Zoom refused the credential this module presented; `Denied` is only ever the
capability floor's word), a `429` is `Error::RetryAfter` with the `Retry-After` delay, a `500` is
`Error::Endpoint` (unknown), a `502`/`503` `Error::Unavailable` and a `408`/`504` `Error::Timeout`
(transient), and a transport that got no answer is a `Timeout` or an `Unavailable`. The Sink
declares those classes in its description, so the manifold says how it can fail before anyone
calls it, and the conformance walk holds it to them.

## Zoom setup (one-time, host side)

1. Create a **Server-to-Server OAuth** app at [marketplace.zoom.us](https://marketplace.zoom.us)
   (Develop → Build App), add **Meeting** read/write scopes, and activate it.
2. Store the three credentials in the OS keystore under the names the backend reads
   (`zoom-account-id`, `zoom-client-id`, `zoom-client-secret`) — on macOS:
   ```
   security add-generic-password -U -s ikigai-secret -a zoom-client-secret -w
   ```
3. For each teacher, store their record as `zoom-teacher-<t>` (see "Per-teacher accounts"): the
   Zoom user the shared app acts for, or the client id, client secret and refresh token of the
   teacher's own user-level OAuth app.

The logic is covered by hermetic tests against a fake transport, so no live credentials are needed
to build or test — only to place a real call.

## Conformance

`tests/conformance.rs` runs [`ikigai-conformance`](https://github.com/ikigai-rs/ikigai-conformance)
over the module's kernel against a loopback Zoom (the suite fires the Sink; nothing reaches the
real API), and pins by hand what the suite cannot see: the denial precedes every read and every
socket, a schedule is never cached, every declared face is the face served, no error carries a
credential or request text. One line remains in the report — `ik:passcode`, the term the shared
vocabulary does not define yet.

## Naming

Each schedule endpoint has its own stable description id and matching catalog label:

| Bound IRI | Description id / catalog label |
| --- | --- |
| `urn:meeting:schedule` (facade) | `meeting-schedule` |
| `urn:meeting:zoom:schedule` (Zoom backend) | `meeting-zoom-schedule` |
| `urn:meeting:id:{m}` (one meeting) | `meeting-id` |

These short kebab-case ids are also the names used by the MCP projection and by arrangement
`door` declarations. Since 0.7.0, each id identifies exactly one endpoint when both are bound in
one kernel. Consumers matching the old shared id `meeting` must use the appropriate new id.
The bound IRIs are unchanged, and the facade and backend still share configuration and the
token cache. See `CHANGELOG.md` for the earlier naming history.

## Status

**`urn:meeting:zoom:schedule` schedules, and `urn:meeting:id:{m}` reads, moves and cancels.** For
them to work a host must bind this crate's space *and* supply a `SecretReader`, with the three
Zoom credentials stored under the names "Zoom setup" gives, and a record per teacher who uses
their own account. The Zoom request and response shapes follow Zoom's published API and are
covered by hermetic tests; no live Zoom call has confirmed the update, cancel and refresh
shapes. Google Meet and Teams backends, and the org-entry
scheduling path, do not exist yet.

## Accessibility

The meeting faces and refusals this crate writes, the rules they follow, how they are tested and their known limitations are in [ACCESSIBILITY.md](ACCESSIBILITY.md). Report a barrier with an issue carrying the `accessibility` label.
