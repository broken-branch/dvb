# Engine core

The core defines task decisions and durable events. The store persists those
events; the engine replays them into views, schedules work and recovers after
interruptions. API handlers, workers and forge effects use these shared types.

## Main modules

- `crates/dvb-core/src/state/`: task states, commands and pure `decide` / `apply`;
  `facts.rs` holds the task counters and folds projections and the property model share.
- `crates/dvb-core/src/event.rs`: event envelope, payloads and run assignments.
- `crates/dvb-core/src/wording.rs`, `wording.json`: event, task and goal wording source.
- `crates/dvb-core/src/api/`, `run_verbs.rs`: owner and worker wire types, including
  builder outcomes and plan `after` references.
- `crates/dvb-core/src/roles.rs`, `model.rs`: roles, hires and model selection.
- `crates/dvb-core/src/forge.rs`, `provider.rs`, `launcher.rs`: effect interfaces.
- `crates/dvb-core/src/caps.rs`, `refusal.rs`: release constants (round, retry and time
  limits) and refusal codes.
- `crates/dvb-core/src/clock.rs`, `git.rs`, `io.rs`, `text.rs`: shared clock, Git, file I/O
  and text clipping and cleaning.
- `crates/dvb-store/src/lib.rs`, `migrations/`: SQLite event log, backups and selective reads.
- `crates/dvb-engine/src/engine.rs`: configuration, construction-time readings
  (`EngineOptions`), model lookups, locks and checked appends.
- `crates/dvb-engine/src/proj/`: projections (`mod.rs`) and one module per view
  (projects, goals, tasks, runs, questions, roles and hires, settings).
- `crates/dvb-engine/src/dispatch/mod.rs`: task scheduling and step routing.
  `effects.rs` publication, review and effect retries; `checks.rs` CI polling;
  `merge.rs` merges and branch updates; `runs.rs` assignments and launch admission;
  `start.rs` task-start rules; `control.rs` stop, resume and retry; `outcomes.rs`
  incomplete builder results and completion answers.
- `crates/dvb-engine/src/wake.rs`: orchestrator wake selection, admission, holds and retries;
  `on_new_issue` wakes a waiting project once for a trusted issue opened after
  the run that asked its last what-next question started; `request_proposal`
  logs a proposal request and wakes the orchestrator (also for `propose_goal`).
- `crates/dvb-engine/src/launch.rs`: run preparation, start and the time-limit watcher.
- `crates/dvb-engine/src/owner.rs`: project pause/resume, goal redirect/drop, ordering and parked plans.
- `crates/dvb-engine/src/prompt.rs`: prompt rendering and recorded owner context.
- `crates/dvb-engine/src/reconcile/`, `units.rs`: restart recovery, launchers and login mounts.
- `crates/dvb-engine/tests/support/`: shared assignments, engines, events
  (`events::all` reads the whole log), launcher settings and readings.

## Start reading

Follow a command from `state::decide` to its events, then `Projections` and the
engine's checked append path. Follow `dispatch` for the resulting effects.
For startup, begin in `dvb-engine/src/lib.rs`, then recovery and `serve.rs`.
See [engine APIs](engine-rest.md) for request handling and publication and
[workers](run.md) for execution.
