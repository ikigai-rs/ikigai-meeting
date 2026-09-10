# ikigai-meeting

Video-conference scheduling as [ikigai](https://ikigai-rs.dev) ROC resources.

`urn:meeting:schedule` is a provider-agnostic **facade** that dispatches to pluggable backends
`urn:meeting:<provider>:schedule` — the same shape as `urn:llm:ask`. **Slice 0** ships a **Zoom**
backend over Server-to-Server OAuth.

The scheduled meeting's join URL is emitted as **`ical:conference`** (RFC 7986's CONFERENCE
property), so one value flows into the `.ics`, the calendar event, and — later — an org entry,
without re-plumbing at each destination.

## Faces

`Sink urn:meeting:zoom:schedule topic=… start=… [duration=30] [timezone=…] [agenda=…] [as=…]`

Every input is typed in the manifold (`topic` `xsd:string`, `start` `xsd:dateTime`, `duration`
`xsd:integer`, …) and held to its type: a garbled value is a typed `InvalidArgument` that names
the input and never echoes the value. A piped value — or a top-level `sink`'s body — arrives as
`content` and is the **agenda** (a named `agenda` wins).

- default (`text/plain`) → the **join URL** (what an invite needs)
- `as=text/turtle` → the **shareable graph** (join-safe: an `ical:Vevent` with `ical:conference`,
  `dcterms:identifier`, `schema:provider`, the passcode — the host start URL is deliberately
  withheld)
- `as=application/json` → the **full envelope** for the trusted caller, including the host
  `start_url` (which carries a start token)

## Injection, not dependencies

Like ikigai-llm, no HTTP client and no keystore are baked in. The host injects:

- an `ikigai_http::HttpTransport` (ureq/reqwest natively, `fetch` in the browser), and
- a `SecretReader` — the host resolves `urn:secret:<name>` under a capability and hands the bytes
  in, so the Zoom credentials stay in the OS keystore and this crate never links it.

## Capabilities

The Sink declares `urn:cap:net:*` and `urn:cap:secret:read:*`, so the kernel refuses a caller
holding no grant under either family before the endpoint runs. The backend's own rules then
require the two Zoom hosts (`urn:cap:net:zoom.us`, `urn:cap:net:api.zoom.us` — port-aware) and
each credential's name (`urn:cap:secret:read:zoom-account-id`, `…:zoom-client-id`,
`…:zoom-client-secret`). Every refusal is a typed `Denied` issued **before any secret is read and
before any socket opens**; the injected `SecretReader` is never asked.

An error never carries a credential or the request's text: a server's 4xx contributes only its
`message`, a transport's text is scrubbed of every credential value, an invalid input is named,
not echoed.

## Zoom setup (one-time, host side)

1. Create a **Server-to-Server OAuth** app at [marketplace.zoom.us](https://marketplace.zoom.us)
   (Develop → Build App), add **Meeting** read/write scopes, and activate it.
2. Store the three credentials in the OS keystore under the names the backend reads
   (`zoom-account-id`, `zoom-client-id`, `zoom-client-secret`) — on macOS:
   ```
   security add-generic-password -U -s ikigai-secret -a zoom-client-secret -w
   ```

The logic is covered by hermetic tests against a fake transport, so no live credentials are needed
to build or test — only to place a real call.

## Conformance

`tests/conformance.rs` runs [`ikigai-conformance`](https://github.com/ikigai-rs/ikigai-conformance)
over the module's kernel against a loopback Zoom (the suite fires the Sink; nothing reaches the
real API), and pins by hand what the suite cannot see: the denial precedes every read and every
socket, a schedule is never cached, every declared face is the face served, no error carries a
credential or request text. One line remains in the report — `ik:passcode`, the term the shared
vocabulary does not define yet — and the id `urn:meeting:zoom:schedule` is a live MCP tool name,
renamed in wave two.

## Status

Slice 0: `urn:meeting:zoom:schedule` (schedule only). Next: cancel/reschedule, Google Meet / Teams
backends, and the org-entry scheduling path.
