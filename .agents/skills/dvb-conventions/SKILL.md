---
name: dvb-conventions
description: The rules every change to dvb follows. Use before writing or reviewing any dvb code.
---

# dvb conventions

- **Matches that classify are exhaustive.** Clippy denies wildcard arms
  (`wildcard_enum_match_arm`, `match_wildcard_for_single_variants`); a filter
  that acts on a few kinds uses `if let`, or `_ =>` under
  `#[expect(clippy::wildcard_enum_match_arm, reason = "…")]`. No
  `#[non_exhaustive]` on internal types.
- **Reasons are codes.** A refusal is a `dvb_core::RefusalCode`, never prose to match
  on. A new refusal is a new code in a `[contract]` PR.
- **One clock.** Read the time only through `dvb_core::Clock`, once per decision.
  Clippy `disallowed_methods` rejects SystemTime/Instant and chrono Utc/Local
  `now`; only `dvb-core/src/clock.rs` has a reasoned exception.
- **No engine terms in the UI** (lease, grant, revision, heartbeat); state and event words come
  from the generated `contracts/wording.json` catalogue.
- **Tests use the Fake provider and Fake forge**: no model call, no network.

- Never drop an error.
- A fixed set of values is an enum.
- No trait, builder or wrapper for one use.
- A comment says only why.
- See each new test fail once; no sleeps in tests.
- Search before naming; one thing, one name.
