# Accessibility

What `ikigai-meeting` emits that people read, the rules that output follows, how it is tested,
and how to report a barrier. It is not a conformance claim. Adapted from the
[ACCESSIBILITY.md template](https://github.com/mgifford/ACCESSIBILITY.md) (MIT).

| Field | Value |
|---|---|
| Target | WCAG 2.2 Level AA, for any page that presents this output; this repository writes text only (a URL, Turtle and JSON), no HTML or SVG |
| Status | Evaluation in progress. No conformance claim. |
| Owner | the maintainers of this repository |
| Report a barrier | [a GitHub issue](https://github.com/ikigai-rs/ikigai-meeting/issues/new?labels=accessibility) with the `accessibility` label; anything security-sensitive through [private vulnerability reporting](https://github.com/ikigai-rs/ikigai-meeting/security/advisories/new) |
| Last reviewed | 2026-10-31 |

## What it emits that people read

- **The scheduled meeting** from `urn:meeting:schedule` and `urn:meeting:zoom:schedule`
  (`src/lib.rs`): the join URL alone as `text/plain` (the default), a shareable Turtle graph
  (`as=text/turtle`), or the full JSON envelope (`as=application/json`). People read the URL in
  an invite; the graph and JSON are read through other tools.
- **Refusals** (`src/lib.rs`): a missing input (`MissingArgument`, naming it), an input outside
  its type (`InvalidArgument`, naming it and saying the form it takes, for example `not a list
  of email addresses (separated by commas, semicolons or whitespace)`), a capability that does
  not reach a host or a credential (`Denied`, naming the host or secret and the capability it
  needs), and the outcome of a call to Zoom (`zoom <step> returned <status>…`, a body over its
  bound, a transport failure).

Not this repository's: the sentence each error kind is wrapped in (ikigai-core's `Display`);
the `message` a Zoom error answer carries, which is passed through as Zoom wrote it; a
transport's own error text, which is passed through with every credential replaced by
`[redacted]`; and anything a calendar, mail client or host shows the URL in.

## The rules that output follows

- A refusal names the input, host or credential at fault, and an invalid input says the form
  it takes. It never echoes the value given, and never carries a credential or the request's
  text.
- No color is written; meaning is in words.

These are followed by reading the source; the tests below pin the first rule.

## How it is tested

- `src/lib.rs` unit tests: `a_missing_topic_is_a_clear_error` (the error names `topic`),
  `inputs_are_held_to_their_declared_classes` (each invalid input is named and its value never
  echoed), and `a_transport_failure_is_transient_and_its_text_scrubbed`.
- `tests/conformance.rs`: `errors_carry_no_credential_and_no_request_text`.
- **No automated accessibility checker, browser or assistive technology has been used on this
  output.**

## Known limitations

Found by reading the source on 2026-10-31:

- Some failures give an HTTP status number without saying what it means: a token Zoom refuses
  ends as `zoom create-meeting returned 401`, and other Zoom answers as `returned
  <status>` plus Zoom's own message when it sends one.
- Some failures say what went wrong but not what to do (`zoom oauth: response not JSON`, `no
  access_token in response`), and `bad url: …` does not say which configured URL is bad.
- The default face is the join URL only: it does not restate the topic or the start time.
- All text is English, with no way to choose another language.

## Contributing

Read this file before changing any output listed above: the faces or the refusals. Keep meaning
in words, and keep refusals naming the input without echoing it. Say in the pull request what
was tested, with what, and what was not.

## Reporting a barrier

[Open an issue](https://github.com/ikigai-rs/ikigai-meeting/issues/new?labels=accessibility)
with the `accessibility` label (the link sets it when GitHub lets you add labels; otherwise a
maintainer adds it): what you were doing, what happened, and your terminal or other tool and any
assistive technology, if you are comfortable sharing them. Anything security-sensitive goes
through [private vulnerability reporting](https://github.com/ikigai-rs/ikigai-meeting/security/advisories/new).
**We acknowledge a report within 7 days**, and promise nothing further.

## Change history

| Date | Change |
|---|---|
| 2026-10-31 | First version. |
