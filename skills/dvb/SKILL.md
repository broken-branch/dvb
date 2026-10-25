---
name: dvb
description: Drive dvb from a shell - state a goal, approve its plan, follow its tasks, read reviews, retry or drop work - with the dvb CLI's JSON output and exit codes. Use when asked to get work done through dvb or to check on a dvb goal or task.
---

# Driving dvb

dvb takes a goal through planning, building, review and merge. You state the
goal and answer its questions; dvb's own team does the rest. In Claude Code
or Codex, use `--json` with `status`, `watch`, `wait`, `goal`, `goal drop`,
`goal after`, `answer`, `retry`, `resume`, `stop`, `pause`, `task diff`,
`task verdict`, `project add` and `project list` to get JSON on stdout. Every JSON shape is in the
[command reference](https://github.com/broken-branch/dvb/blob/main/docs/commands.md).

Errors go to stderr and exit 1. A refusal, and a name or id with nothing
behind it, has one form: `dvb: refused: <CODE> (<detail>)`, for example
`dvb: refused: NOT_FOUND (no project example)`; read the code, not the words.
Exit 2 is only a usage error (a missing or unknown argument), for every
command. `dvb wait` has its own codes, below.

To install this skill, copy `skills/dvb` from the dvb repository to
`~/.claude/skills/dvb` for Claude Code, or to `~/.codex/skills/dvb`
(`$CODEX_HOME/skills/dvb`) for Codex.

## State a goal and approve its plan

```sh
dvb project list --json  # {"projects": [{"id", "name", ...}]}
dvb goal <project> "<what you want, with how to check it is done>" --json
# {"goal": "<goal-id>"}: keep the id
dvb wait <goal-id> --until plan --timeout 30 --json
```

`dvb wait` exits with what happened, so you never poll:

| Exit | Meaning |
|---|---|
| 0 | `--until plan`: the plan question is open, or the plan is already approved. `--until done` (the default): the goal is Done. |
| 1 | dvb could not read the engine (a refusal or an unreadable reply). |
| 2 | Usage error only (a missing or unknown argument); nothing was checked. |
| 3 | A task of this goal failed: `dvb retry <task-id>`. |
| 4 | `--timeout` (minutes) elapsed. `--timeout 0` is a single check: 4 means nothing above holds yet. |
| 5 | The goal does not exist or was dropped. |
| 6 | Nothing goes on until you act: a task of this goal is stopped (`dvb resume <task-id>`), its plan is on hold (`dvb resume <goal-id>`), or its project is paused (`dvb resume <project>`). |
| 7 | A question of this goal waits for an answer. |

With `--json` it prints one object with the same exit code: `outcome` (`done`,
`plan_question`, `plan_approved`, `question`, `failed`, `dropped`, `stopped`,
`plan_parked` or `project_paused`), `goal` and `state` (`stopped`, not
`failed`, when the goal's tasks ended with stopped ones and none failed), and
with it `question` (its `id`, `subject`, numbered `options` with `id`, `text`
and `effect`, and `recommended`), `task` (`id`, `title`, `reason`, and the
`command` that continues it), `command` for a plan on hold or a paused project,
or `merged` (`task`, `pr`, `commit`) when the goal is Done. A timeout or an
error prints nothing on stdout.

Answer with an option's id, or in words to change the plan:

```sh
dvb answer <question-id> <n> --json
dvb answer <question-id> --reply "Split the parser into its own task" --json
```

Answering a plan question "Not yet" puts the plan on hold: nothing runs and
nothing asks again until `dvb resume <goal-id>` asks the same question again.
It comes back under a new question id: take it from `dvb resume <goal-id>
--json` (`{"goal": {...}, "question": {"id", "options", ...}}`) or from
`dvb wait <goal-id> --until plan --json`. Answering the old id is refused with
`ALREADY_TERMINAL`, and its detail names the new question.

## Follow the tasks

`dvb wait <goal-id> --timeout 120 --json` blocks until the goal is Done (0),
has a failed task (3), has a stopped task, a plan on hold or a paused project
(6), or needs an answer (7). Reviews do not wait for you and do not end the
wait: dvb's own reviewers judge each change. Read a review after it is made
with `dvb task verdict` and `dvb task diff` (below).
For the events in between, read the feed and keep the last `id` you printed:

```sh
dvb watch --json --once --project <project>                 # what happened so far
dvb watch --json --once --project <project> --after <last-id>  # only what is new
```

Each line is one event: `id`, `kind`, `time`, `payload` and, when it has one,
`text`. Task states, roles (`role`, and its slug in `role_name`), PR numbers
and reasons are in `tasks` of `dvb status <project> --json`; `project.paused`
says whether the project is paused (`dvb resume <project>` unpauses it). A task
in `retry_pending` is retried by dvb itself; its `reason` says why and which
attempt comes next, so wait rather than retry it.

When a goal is Done, dvb may open a "what next?" question proposing the next
goal, usually from the repository's open issues. It blocks nothing: answer it
to start that goal (or reply naming another), or leave it open and state your
own goal.

## When something needs you

`needs_you_items` in `dvb status [<project>] --json` lists each item, with its
`project` and `project_name`, and the `command` that continues it:

- A failed task: `dvb retry <task-id>`, or `dvb retry <task-id> "<what was
  missing>"` to give its builder more to go on.
- A stopped task (`reason` says why): `dvb resume <task-id>`. Retry with text,
  `dvb retry <task-id> "<text>"`, only when the task has a PR or kept work to
  build on; otherwise resume it.
- A plan on hold (`"kind": "plan"`, its `id` the goal's): `dvb resume
  <goal-id>`.
- A paused project (`"kind": "project"`): `dvb resume <project>`.
- A question: `dvb answer <question-id> <n>`.

`retry`, `resume`, `stop` and `pause` take `--json`: a task verb prints
`{"task": {...}}` (the task as in `tasks` of `dvb status <project> --json`),
`pause` and `resume <project>` print `{"project": {...}}` with `paused`, and
`resume <goal-id>` prints `{"goal": {...}, "question": {...}}`. `dvb resume`
with an id that is none of these is refused:
`NOT_FOUND (no task, goal or project <id>)`.

## Stop and pause

- `dvb stop <task-id> --reason "<why>" --json` stops one task in any state; its
  branch, workspace and results are kept, and the reason is the task's
  `reason`. With `--reason` dvb's orchestrator is not woken to ask why; without
  it, it is, and may ask you a question. `dvb resume <task-id>` carries on.
- `dvb pause <project> --json` starts nothing new in the project (what is
  running carries on) until `dvb resume <project>`. `dvb wait` on its goals
  ends with 6 and `outcome` `project_paused`. `pause` has no `--reason`.

## Read a review

```sh
dvb task verdict <task-id> --json  # role_name, summary, open_findings, findings with file, line, severity
dvb task diff <task-id> --json     # base, head and the unified diff
```

## Drop a goal

`dvb goal drop <goal-id> --json` cancels its planned tasks and closes its
questions; merged work stays. It prints the goal with `"state": "dropped"`, its
text in `text` as in `dvb status --json`.
