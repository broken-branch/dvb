# Event catalogue

Every event kind dvb records, one line each, with its payload. Feed and state
wording comes from `crates/dvb-core/src/wording.json`, generated into [wording.json](wording.json). The `EventKind` enum in `crates/dvb-core/src/event.rs`
has exactly these kinds; a test in `dvb-core` fails when the two differ. A new kind is
added to both in a `[contract]` PR.

Every event also has `id`, `at`, `actor` (`owner`, `engine` or `{run: <id>}`),
`project` and `evidence` (`head`, `pr`, `verdict`, where they exist). Payloads never
carry tokens, logins or the owner's identity. Wording placeholders are filled
from the payload or the records it names; null entries are hidden from the feed.

| Kind | Payload |
|---|---|
| `engine.started` | version |
| `engine.reconciled` | task, state |
| `engine.updated` | from, to, snapshot |
| `engine.update_failed` | from, to, reason |
| `project.added` | id, name, repo, default_branch, ci: `{expected: [names]}` or `"no_ci"` |
| `repository.created` | name, repo, repository_id |
| `repository.setup_prepared` | repo, branch, head, ci |
| `goal.issue_closed` | goal, issue |
| `proposal.requested` | — (the owner asked for the next goal to be proposed from the open issues; no goal exists until one is approved) |
| `project.paused` | — |
| `project.resumed` | — |
| `project.seen` | — (the owner opened the project; its events after the latest `project.seen` are new to the owner) |
| `project.brief_set` | brief (the owner's private context for the project; kept only in the event log) |
| `project.issues_found` | open, trusted, labels: [{label, open}] (the open issues when the project was added) |
| `project.ruleset_missing` | — (GitHub allows no ruleset on the repository, a private one on GitHub Free; dvb goes on without it; recorded once when it starts) |
| `project.ruleset_installed` | — (a project that had no ruleset got dvb's when its checks were set) |
| `project.repo_changed` | from, to (the owner pointed the project at its repository's new name, the same repository; earlier events keep the old name) |
| `project.repo_unreachable` | repo (GitHub gave no token for the repository by its saved name, HTTP 422; recorded once until a mirror write succeeds or the project is repointed) |
| `project.removed` | name, repo (the owner removed the project from this install; it leaves every view and takes no goal, its earlier events stay and nothing changes on GitHub) |
| `role.added` | role record: id, slug, name, job, lane, lead, reports_to, reviewed_by, review_instructions |
| `role.changed` | role record, restored_from? (the event whose value it restores) |
| `role.removed` | role record |
| `hire.added` | hire record: id, role, model, effort, status, routine? (the role's routine hire; absent means its default kind) |
| `hire.changed` | hire record, restored_from? |
| `hire.retired` | hire record |
| `settings.changed` | key, old, new, restored_from? |
| `goal.stated` | goal, text, from_issue, issues?: [{number, url, text}], after? |
| `plan.proposed` | goal, tasks: [{task, title, role, review: `code` or `design`, summary, after?: [task id], routine? (routine work for the role's routine hire)}] |
| `plan.cancelled` | goal, tasks |
| `goal.approved` | goal |
| `goal.done` | goal |
| `goal.dropped` | goal |
| `goal.after_set` | goal, after (the goal it now waits for before planning; null clears it) |
| `question.opened` | kind?: ordinary/orchestrator_failed/planning_hold/completion (task, run, result event id), task?, question, subject, format, options: [{id, text, effect, approves_plan, no_effect? (choosing it wakes no orchestrator), retries_task?}], recommended, lapses_on: `{goal_version: <event id>}` or `{value_version: <event id>}`, main_ci?: [{name, status, run, conclusion, url}] |
| `question.closed` | question, task |
| `question.answered` | question, answer: `{choice: <option id>}` or `{reply: <text>}` |
| `decision.recorded` | by (role), title, options, swot, choice, reason |
| `command.refused` | command, code (and the code's fields), detail |
| `task.started` | task, run, resume, branch_repair? (the run's `run.started` has the rest) |
| `task.waiting` | task, model, effort, reason, routine? (the start marked the task routine) (the orchestrator's start, refused for the cap on concurrent runs; the engine starts it as asked when a run ends) |
| `task.stopped` | task, update? (true resumes this stop once after update or rollback), reason (the owner's `dvb stop --reason`, or the engine's; an owner reason wakes no orchestrator for the stop) |
| `task.resumed` | task, reason (a retry of a failed task: "retried after it failed") |
| `task.cancelled` | task, reason |
| `task.done` | task, reason |
| `task.failed` | task, reason |
| `task.retry_text_set` | task, text (optional; null clears earlier retry context), atomic with accepted retry |
| `task.reason` | task, run, reason (nullable; current run’s projected status only) |
| `task.state` | task, from, to |
| `run.started` | run, task (absent for an orchestrator wake), goal_version (persisted orchestrator binding; absent in historical logs and task runs), role, hire, model, vendor, effort, reason, review_stage (`code` or `design`, absent in historical logs/builders), reviews (the head, for a reviewer run), base (the commit the run's workspace starts from; absent in old logs), routine? (a builder round of a routine task) |
| `run.preflight` | run, checks: [{code, ok, detail}], cli_version, boundary |
| `run.model` | run, requested, served: `{confirmed: <id>}`, `"unconfirmed"` or `{mismatch: <id>}` |
| `run.output` | run, item: `{text}`, `{tool_call: {name}}` or `{note}` (at most 4 KiB) |
| `run.usage` | run, uncached_in, cached_in, out, est_cost |
| `run.collected` | run, head?, summary?, outcome?: `{status: complete}` or `{status: partial, reason: out_of_role/completion_unmet/other, detail}` or `{status: blocked, prerequisite}` |
| `run.stop_requested` | run (durable taskless stop intent, retried across restart until its end is confirmed) |
| `run.survived_restart` | run (startup observed its unit active; normal completion survives replay) |
| `run.ended` | run, reason: `completed`, `stopped`, `crashed`, `oom`, `interrupted`, `mismatch`, `{preflight: <code>}`, `parse_error` or `time_limit` (stopped at its time limit); `detail`, absent unless a `crashed` run's worker left one: its exit code and one-line cause, as `exit 4: exec: the session exited with code 1`; `stderr`, absent unless a `crashed` run's session CLI wrote to stderr: its last lines, newline-separated, each line the secret scan matches replaced by `[line withheld by dvb's secret scan: <rule>]`, at most 400 bytes |
| `run.launched` | run (its unit is about to start; the run's time limit counts from here, or from `run.started` when absent) |
| `run.time_limit_reached` | run, task (absent for an orchestrator), kind: `orchestrator`, `builder` or `reviewer`, limit_minutes (the engine stops the unit; the run ends `time_limit`) |
| `run.rerouted` | run, task?, role, from_model, to_model, vendor, effort (what it runs on now), limited? (the vendor at its limit; absent for only), resets_at? |
| `only.started` | models (ordered), ends_at |
| `only.ended` | started (start event id) |
| `vendor.limited` | vendor, resets_at |
| `vendor.resumed` | vendor |
| `branch.published` | task, round, branch, head |
| `branch.updated` | task, branch, base, from, head, same_change (the engine's neutral merge of `base` into `from` after a merge refused it as behind; with the same patch id the clean verdict is copied to `head` as `verdict.recorded`, else review again; CI runs again) |
| `scan.hit` | task, file, line, rule |
| `pr.opened` | task, pr, url |
| `verdict.recorded` | verdict, task, head, run, role, model, vendor, review_stage (`code` or `design`, absent in historical logs; from the reviewer run's record), findings: [{severity, file, line, title, detail, open}], summary |
| `checks.pending` | task, head, checks: required check names currently Pending; timestamps start new waits on replay (quiet) |
| `checks.observed` | task, head, status: `missing`, `pending`, `unreadable`, `failed` or `success`, names, retry_question?, retry_runs? |
| `checks.stuck` | task, head, check, url (its link, when the forge gives one), limit_minutes (a required check on the head stayed Pending this long; told once per check and head, the task keeps waiting) |
| `merge.refused` | task, reason |
| `pr.merged` | task, pr, sha |
| `effect.retry` | task, effect, attempt, at, rate_limit? ({reset?}; absent historically; budget waits do not spend attempts) |
| `github.mirror_observed` | goal, task?, mirrored, detail? |

Project check replacements use owner `settings.changed` events scoped to the
project, with key `project_checks` and old/new values in the existing `CiMode`
shape. Replay replaces that project's saved choice. Historical `project.added`
payloads remain unchanged; check history includes their initial choice.
