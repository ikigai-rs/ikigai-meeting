# ikigai-meeting

Video-conference scheduling as [ikigai](https://ikigai-rs.dev) ROC resources.

`urn:meeting:schedule` is a provider-agnostic **facade** that dispatches to pluggable backends
`urn:meeting:<provider>:schedule` — the same shape as `urn:llm:ask`. **Slice 0** ships a **Zoom**
backend over Server-to-Server OAuth.

The scheduled meeting's join URL is emitted as **`ical:conference`** (RFC 7986's CONFERENCE
property), so one value flows into the `.ics`, the calendar event, and — later — an org entry,
without re-plumbing at each destination.

## Faces

`Sink urn:meeting:zoom:schedule topic=… start=… [duration=30] [timezone=…] [agenda=…]`

- default (`text/plain`) → the **join URL** (what an invite needs)
- `as=text/turtle` → the **shareable graph** (join-safe: `ical:conference`, passcode, id — the host
  start URL is deliberately withheld)
- `as=application/json` → the **full envelope** for the trusted caller, including the host
  `start_url` (which carries a start token)

## Injection, not dependencies

Like ikigai-llm, no HTTP client and no keystore are baked in. The host injects:

- an `ikigai_http::HttpTransport` (ureq/reqwest natively, `fetch` in the browser), and
- a `SecretReader` — the host resolves `urn:secret:<name>` under a capability and hands the bytes
  in, so the Zoom credentials stay in the OS keystore and this crate never links it.

## Capabilities

The backend refuses before the socket unless the caller holds the net grant for the Zoom hosts
(`urn:cap:net:zoom.us`, `urn:cap:net:api.zoom.us`); the meeting action is `urn:cap:meeting:zoom:schedule`.

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

## Status

Slice 0: `urn:meeting:zoom:schedule` (schedule only). Next: cancel/reschedule, Google Meet / Teams
backends, and the org-entry scheduling path.
