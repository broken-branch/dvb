# Engine APIs and effects

`dvb-engine` serves the owner and worker APIs over separate Unix sockets. It
prepares prompts, publishes collected trees, observes reviews and checks, mirrors
work to GitHub, accounts for usage and installs verified updates.

## Main modules

Paths below are under `crates/dvb-engine/src/`.

- `lib.rs`, `engine.rs`, `serve.rs`: startup, shared state and socket serving.
- `api/mod.rs`, `run_api/mod.rs`: owner and worker route tables.
- `api/projects.rs`, `projects_add.rs`, `projects_repo.rs`, `projects_remove.rs`, `goals.rs`,
  `questions.rs`, `tasks.rs`: owner work commands, project addition, repointing and removal,
  goal issues and proposals, answers and task evidence.
- `api/roles.rs`, `hires.rs`, `models.rs`, `settings.rs`, `only.rs`: team, preferences and
  timed model selection; `runs_memory.rs` reads and sets the runs' slice limit.
- `api/history.rs`: shared history folds, restores and stale-edit checks.
- `api/github.rs`, `logins.rs`: forge setup and worker subscription logins.
- `api/events.rs`, `feed.rs`: bounded history reads and replay/live event streams.
- `api/runs.rs`, `status.rs`, `usage.rs`, `report.rs`, `update.rs`: run evidence,
  status counts and warnings, usage, bug and feature report links, and updates.
- `owner.rs`: pause, redirect, drop, goal ordering and parked plans.
- `run_api/orchestrator/`: the orchestrator/lead role check; `planning.rs` plans and goal
  binding, `tasks.rs` task start/stop/retry, `questions.rs` asks and autonomy,
  `hires.rs` hiring and retirement.
- `run_api/shared.rs`, `driver.rs`, `verdict.rs`: decisions, notes, issue reads and run reports.
- `issues.rs`: the issue trust rule, fenced issue text and the open-issue list.
- `prompt.rs`, `launch.rs`, `wake.rs`: prompt rendering, run launches and orchestrator wakes.
  Templates and shared verb/browser instructions live in `prompts/`.
- `publish/mod.rs`, `project.rs`: neutral commits, scanning, the publication identity and
  project setup.
- `publish/reconcile.rs`, `mirror.rs`: checks, merges, cleanup and the board mirror,
  which sends only items changed since their `github.mirror_sent` receipt;
  the mirror pass also calls `wake::on_new_issue` for each project. It takes
  the mirror lock per project and goal job; `mirror::lock_for_change` is how a
  project's remove or repoint waits for that job, and `/status` shows the
  running pass.
- `vendors.rs`, `only.rs`, `usage/`: vendor limits, timed selection and accounting.
- `units.rs`: systemd D-Bus unit control and the dev process launcher.
- `update/`: signed downloads, bounded drain/handover, update-stop recovery receipts,
  backups, rollback and local updates. Dispatch resumes marked update stops once.
- `bin/openapi.rs`: generates the owner contract from the router.

## Start reading

Find the request in the route table, then its handler and shared core request
shape. Writes validate shared state inside checked event appends; projections
provide the read views. Worker requests also validate the run's role,
assignment and publication head.

For a task's path to merge, read `dispatch/` with `publish/` and the core state
machine. For prompts, start at `prompt::context`, the role template in `prompts/`
and `prompt::render`; `prompts/fragments.toml` holds prompt context, while
`wording.json` owns question subjects and options. For updates, start at
`update/mod.rs`, then `fetch.rs`, `protocol.rs` and `handover.rs`.

Some CLI behaviour has an API or run-verb form the command reference leaves out:
`GET`/`PUT /settings/publication` carry `dvb settings publication --as-owner` as
`as_owner`, and without it an empty name or email is dvb's default; `POST /hires`
takes `routine` as `dvb hire --routine` does; the orchestrator marks routine tasks
with `"routine": true` in `plan` or `task start` and retries a task after the
owner's yes with `dvb run task retry`; runs read issues with `dvb run issue
<number>` and `dvb run issues`.

See [core](core.md), [forge/CLI/desk](forge.md) and [workers](run.md) for
neighboring entry points.
