---
name: dvb-preflight
description: How to add or change a preflight check with its code, fix hint and test. Use when changing the run's preflight in dvb-run.
---

# Adding a preflight check

The preflight runs inside the real unit as the first step of every run, before any
model spend. It lives in `crates/dvb-run/src/preflight.rs`.

1. Add the code to `PreflightCode` in `crates/dvb-core/src/event.rs` (a `[contract]`
   PR; run `scripts/gen-contracts.sh`), to the test list `ALL` in `preflight.rs`, and its row with the
   fix hint to `prompts/preflight.md`. The hint reaches the owner: a failed check's
   detail ends with `Fix: <hint>`.
2. Write the check as a `fn ..._probe(...) -> Result<String, String>` (`Ok` is
   the detail of a pass, `Err` the detail of a failure) and call it from
   `preflight::run_in`. A check that reads the world takes what it reads as
   arguments (`Ambient` for the process, a path, a flag) so a test can break it
   without a worker user or a unit.
3. A check that depends on another (the model list needs the CLI and a login) passes
   with a "skipped" detail when its dependency failed, so one broken thing is one
   code.
4. Add a test that breaks exactly that check and sees exactly that code (see the
   `pf_*` tests: a hermetic run directory with stub CLIs, then one change). The test
   `every_code_has_a_hint_in_the_table` fails if the table lacks the row.
5. A failure ends the run with `run.ended {preflight: <code>}`.
