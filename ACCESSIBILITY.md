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
| Last reviewed | 2026-11-12 |

## What it emits that people read

- **Documentation**: `README.md`, `AGENTS.md`, `CHANGELOG.md` and this file, read as
  Markdown on GitHub or in an editor.

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

The checks below record source review and tests of these rules separately from checks of
the tools people use to read the output.

## How it is tested

Every named check has a result: Pass, Fail or Blocked, with evidence. A Pass covers only
the stated check, not overall accessibility. The automated checks below ran on 2026-11-12,
before this documentation edit, on main commit
[`8e0c6e7`](https://github.com/ikigai-rs/ikigai-meeting/commit/8e0c6e71d365d21de875d56cb4d8f536ef5f4381).
Command: `cargo test --all-targets`, Cargo 1.99.1 and rustc 1.99.1; all 57 tests passed.
[CI](.github/workflows/ci.yml) repeats the Rust tests through the shared Rust workflow;
it does not repeat the source review or run browser or assistive technology checks.

| Check | Result | Evidence |
|---|---|---|
| Missing input is named | Pass | `src/lib.rs`: `a_missing_topic_is_a_clear_error`, run by the command above, checks the typed error names `topic`; does not evaluate spoken clarity. |
| Invalid input is named without echoing its value | Pass | `src/lib.rs`: `inputs_are_held_to_their_declared_classes`, run by the command above, covers invalid start, duration and media type. `tests/conformance.rs`: `errors_carry_no_credential_and_no_request_text` also covers invalid attendees. These are selected cases, not every possible input. |
| Transport failure text is scrubbed | Pass | `src/lib.rs`: `a_transport_failure_is_transient_and_its_text_scrubbed`, run by the command above, checks the failure class and credential redaction; does not evaluate a host's presentation. |
| Refusals exclude credentials and request text in tested cases | Pass | `tests/conformance.rs`: `errors_carry_no_credential_and_no_request_text`, run by the command above, covers denied host, transport failure, JSON and text server errors, and invalid inputs using a local HTTP stub, not live Zoom. |
| Output faces preserve their tested content | Pass | `src/lib.rs`: `schedule_returns_the_join_url_by_default`, `the_turtle_face_carries_ical_conference_but_not_the_host_url`; `tests/conformance.rs`: `declared_outputs_are_the_media_types_served`, `the_turtle_and_json_faces_are_term_equal`, all run by the command above. Checks bytes, media types and RDF terms; no rendering or assistive technology. |
| Meaning in words without emitted color | Pass | Source review of `src/lib.rs` on the main commit above, 2026-11-12: response serialization and error construction use text, no color styling. Does not assess colors or layout chosen by a consuming tool. This manual review is not repeated by CI. |
| HTTP failure explanations | Fail | Source review, 2026-11-12: `Created::into_meeting` emits `zoom create-meeting returned 401`; `server_error` can give a numeric status plus Zoom's message without explaining the status. |
| Recovery guidance and configured URL identification | Fail | Source review, 2026-11-12: `access_token` emits `response not JSON` or `no access_token in response` without a remedy; `gate_net` emits `bad url: …` without naming the configured URL. |
| Default output includes meeting context | Fail | `schedule_returns_the_join_url_by_default` and source review, 2026-11-12: the default face is only the join URL, without topic or start time. |
| Language selection | Fail | Source review of `src/lib.rs`, 2026-11-12: module-authored messages are English with no language selector; upstream text can differ. |
| Documentation rendering and browser presentation | Blocked | No browser rendering check was run for the Markdown or output in a consuming host; no browser evaluation was configured for this update. |
| Manual keyboard | Blocked | No person performed a keyboard pass in a consuming tool; no interactive evaluation session was available for this update. |
| Automated accessibility checker | Blocked | No axe or other automated accessibility checker was run on rendered documentation or output; no browser checker was configured. Rust tests are not an accessibility checker. |
| Screen reader | Blocked | No screen reader was used on this output; no screen reader session was available for this update. |
| Screen magnifier | Blocked | No screen magnifier was used on this output; no magnifier session was available for this update. |
| Speech input | Blocked | No speech-input software was used on this output; no speech-input session was available for this update. |
| Switch access | Blocked | No switch-access software was used on this output; no switch-access session was available for this update. |
| Braille display | Blocked | No braille display was used on this output; no braille display was available for this update. |

## Known limitations

The Fail rows above are limitations found by source review, not WCAG conformance verdicts.
The Blocked rows are gaps in evaluation, not evidence that the output works with those tools.
The tests do not assess the accessibility of Zoom, calendar or mail clients, the host's
interface, or the rendering of this repository's documentation.

## Contributing

Read this file before changing any output listed above: the faces or the refusals. Keep meaning
in words, and keep refusals naming the input without echoing it. Say in the pull request what
was tested, with what, and what was not. In the pull request’s accessibility section, give
each check a `Check | Result | Evidence` row with Pass, Fail or Blocked and the test or
command, tool and version, or why it could not run. Keep unrun checks as Blocked, including
assistive technology checks. Historical runs that CI does not repeat are Blocked with their
dates, not current Pass results.

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
| 2026-11-12 | Record scoped Pass, Fail and Blocked results, current main test evidence and unrun assistive technology checks. |
