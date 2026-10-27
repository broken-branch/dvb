# Command reference

See the [README](../README.md) for requirements, installation and setup.
This page covers the main commands and what they do; `dvb --help` lists every
command, and `dvb <command> --help` its arguments and flags.
Use a project name or id wherever a command takes `<project>`.

- [Everyday use](#everyday-use)
- [Setup](#setup)
- [Goals](#goals)
- [Following work](#following-work)
- [Team and models](#team-and-models)
- [Install-wide](#install-wide)
- [Other commands](#other-commands)
- [Scripting](#scripting)

## Everyday use

State a goal from an issue and follow the project's events:

```sh
dvb goal example --issue 12
dvb watch --project example
```

`--issue 12` states a goal from the repository's issue #12
(see [Goals from issues](#goals-from-issues)).

`dvb status example --all` includes finished work. See the
[README quick start](../README.md#quick-start) for goal → wait → answer.

## Setup

### Setup check

`dvb doctor` prints what setup still needs, from the status the engine serves
the app's setup screens: one line per item, `ok` when it is met and `missing`
with the command that fixes it when it is not, then a summary line. The items
are the git name and email `dvb github connect` records (stored by an earlier
App, else the owner's global git config; unmet names the two `git config
--global` commands), the GitHub App (`dvb github connect` creates your own private App or installs it), once it is
installed the repositories it reaches (unmet access names `dvb github connect`
and the GitHub access page), the identity dvb publishes commits as, and the
Claude and Codex logins
(`dvb login claude`, `dvb login codex`). A missing login is unmet only when the
default hires need its vendor, as on the app's sign-in step. It exits 0 when
everything is met, and 1 when anything is not or the engine cannot be read.

```text
ok       Git identity: Example Owner <owner@example.invalid>
ok       GitHub App: connected to example-org
ok       GitHub repository access: example-org/app
ok       dvb publishes commits as dvb <dvb@dvb.invalid> (the default)
missing  Claude login: not signed in; the default hires need it. Fix: dvb login claude
ok       Codex login: signed in
1 setup item needs you.
```

`dvb doctor --json` prints the same as one object, with the same exit codes:
`{"items": [{"met": true, "text": "Git identity: Example Owner <owner@example.invalid>"}, …],
"unmet": 1}`.

### GitHub connection and project addition

`dvb github connect --owner <org-or-user> [--repos a,b]` connects GitHub with no
app window, through the same engine setup routes the app's wizard uses. It prints
the Create App link: a page this command serves on 127.0.0.1 that posts the App
manifest to GitHub, since GitHub creates an App only from a form. Open it in a
browser on the machine running the cockpit (the command starts it as `dvb app`
does); GitHub returns to the cockpit's callback. That browser needs no dvb
session: after Create App the callback goes straight on to GitHub's installation
page, and after installation it says `Connected to <owner>. You can close this
window.` It shows the failure page only when the engine refused or recorded a
failed step. The command then prints the installation link, where the owner chooses the repositories the App may access, and waits,
reading the setup status every 3 seconds, until GitHub reports the App installed.
With `--repos` (a bare name is in the connected account) it also waits until the
App reaches each one, printing the access settings link if one is missing. It
ends with `Connected to <owner>; repositories: …` and exits 0. Already
connected, it prints that line at once. Without `--owner` it continues the
account already being set up. After 15 minutes, or when GitHub reports a failed
step, it prints `dvb doctor`'s lines for the App and its repository access and
exits 1. `--json` prints one line per link, `{"link": "create_app" |
"install_app" | "repository_access", "url"}`, then the `dvb doctor --json` object
with `owner` and `repositories`.

The App is private and owned by that organization or account, not by the dvb
authors; there is no hosted service and you need no account with them. The engine keeps the
App's id and key in its state directory and owns callback
validation and installation verification. The wizard takes an
organization or a personal account's login; the engine tells them apart from
GitHub's public read of the account and creates the App under it. dvb copies goals
and tasks to GitHub as milestones, a project board and pull requests. A personal
account's App has no project board permission, so its goals and tasks are mirrored
to milestones and pull requests without a board. A login GitHub does not know is
refused before any App exists: "GitHub has no personal account or organization
named <login>."

In an organization, `--create` makes a new private repository
(`dvb project add example --repo your-org/example --create`; for a private
repository on GitHub Free, see the plan note below).
A new repository has no CI yet, so `--create` without `--check` means no CI. dvb
cannot create a repository in a personal account: create it on GitHub, give the dvb
App access to it, and add it without `--create`. For an existing repository
with CI, name its required checks:

```sh
dvb project add example --repo your-org/example --check <name>
```

Use the actual check names from that repository's CI, or choose `--no-ci` if it
has none.

`dvb project add example --repo owner/repository` discovers check runs on
the default branch and prints each detected name. Pending and failed checks still
supply names; those names must succeed on a task's head before merge. If no names
can be inferred, choose `--check <name>` (repeatable) or explicitly `--no-ci`.
Discovery errors require retry or an explicit choice, and never imply no CI.
`--create` without `--check` means no CI, the same as `--no-ci`, because the new
repository has no checks yet; `--create --check <name>` names its checks.
Required repository checks are combined with the chosen checks, including when
the owner chooses no CI. A public repository works on any plan. On GitHub's Free
plan, which allows no ruleset on a private repository, a private one is added
without dvb's ruleset, existing or created with `--create`: dvb reads no
repository rules for required checks, records the chosen checks and still merges
only when they pass. `dvb status` then warns "<project>: No GitHub ruleset on the
default branch: GitHub Free allows none on a private repository; dvb still merges
only when the checks pass" until a later `dvb project checks <project> <check>…`
or `--no-ci` finds that GitHub allows a ruleset and installs it. In a public
repository, PR titles, PR bodies, commits and milestones are public; a milestone shows the goal's first line and its task
titles, not the rest of the goal.
On a personal account, `--create` is refused before anything is written.
After adding, it prints what dvb
found among the open issues, for example
`Open issues found: 8 (8 trusted, 3 labelled enhancement)`.
When the publication named by the default branch's newest dvb squash merge
(`Reviewed publication <sha>.`) that the clone has was authored by another
identity than dvb publishes as, it adds one line naming both and, unless that
identity is the owner's, the exact `dvb settings publication` command to
switch; the project is added either way.

`dvb project add … --json` prints only the project as the engine returns it, with
no detected-check lines: `{"id", "name", "repo", "default_branch", "ci",
"paused", …}`, with `issues_found` and, when there is one,
`publication_mismatch`, the facts the lines above print.

### Project checks

| Command | Action |
|---|---|
| `dvb project checks <project>` | Print expected check names, one per line, or `no CI`. |
| `dvb project checks <project> <check>…` | Replace the entire expected check list. |
| `dvb project checks <project> --no-ci` | Disable dvb's CI gate for this project. |
| `dvb history checks <project>` | List saved choices, including project setup, newest first. |

Use check names or `--no-ci` when changing the choice. Unmerged PRs use the
latest saved choice at their next check observation.
Repository rules can require checks even when dvb is configured with no CI.

```sh
dvb project checks example test lint
dvb history checks example
```

## Goals

### Stating and dropping a goal

`dvb goal <project> <text>` states a goal and prints `Goal <id>: <text>`; the
orchestrator plans it and asks for your yes (see [Waiting on a goal](#waiting-on-a-goal)).
`dvb goal drop <goal-id>` drops it: its planned tasks are cancelled, its
questions closed, and merged work stays.

With `--json`, `dvb goal <project> … --json` prints only the new goal's id,
`{"goal": "01M5…"}`. `dvb goal drop <goal-id> --json` and
`dvb goal after <goal-id> <other|none> --json` print the goal as the engine
returns it, its whole text in `text` as in `dvb status --json`:

```json
{"id": "01M5…", "project": "01M4…", "text": "Add a note\nMore detail",
 "state": "dropped", "plan_parked": false, "waits_for": "01M3…"}
```

`state` is `stated`, `approved`, `in_progress`, `done`, `failed` or `dropped`;
`waits_for`, the goal still holding its planning, is absent when there is none.

### Goals from issues

`dvb goal <project> --issue <n> [--issue <m>] [<text>]` attaches issues of the
project's repository: any issue of a private one, and in a public one only issues
by the owner, a member or a collaborator. The text may be left out; the goal then reads
"Resolve issue #n.". The goal carries each issue's text and comments; its last
PR says `Closes #n`, and dvb closes the issues when the goal is done. A goal's
text that mentions `#n` gives the orchestrator that issue's text.

When a goal is done and no other goal is stated or in progress, the
orchestrator proposes the next goal from the open issues, in an order it
explains, in one question: approve (the goal is stated from those issues, as with
`--issue`), reply naming a different issue, or skip. It asks you to name the next
goal only when no issue is open. Issues opened meanwhile are included.

Runs read issues through dvb; they hold no GitHub credentials, and dvb reads issues with a token limited to `issues: read`
and `metadata: read` on the project's repository. In a public or internal
repository, including one that turns public, dvb withholds every issue and
comment not written by the owner, a member or a collaborator, and refuses such
an issue with `ISSUE_UNTRUSTED`.

### Goal dependencies

`dvb goal <project> --after <goal-id> <text>` waits for another goal in the same
project to finish or be dropped before planning starts. Status shows the goal it
waits for; the dependency survives restart and redirect.

`dvb goal after <goal-id> <other-goal-id>` changes the order of a stated goal
with no started task: it is planned only after the other goal, in the same
project, is Done or Dropped. `dvb goal after <goal-id> none` clears the order, so
the goal is planned when nothing else is in progress. A dropped goal still
counts as done for the goals waiting on it, so point them elsewhere before
dropping it. `dvb watch` shows the change, for example `Goal 01M5 now follows
goal 01M4`.

A saved plan is kept when its goal is reordered. Approval is refused with
`VALUE_CHANGED` while the goal waits for its new predecessor; the question stays
open so it can be answered when that predecessor is Done or Dropped. Clearing
the order also restores a missing approval question for a saved plan.

dvb refuses an unknown goal (`NOT_FOUND`), another goal that does not exist or is
in another project (`INVALID_VALUE`), a goal that is Done or Dropped
(`ALREADY_TERMINAL`), a goal whose plan is approved or that has started work
(`GOAL_STARTED`), and a goal pointed at itself or an order that would loop
(`GOAL_ORDER_CYCLE`).

```sh
dvb goal after 01M5 01M3
dvb goal after 01M5 none
```

### Answering a question

`dvb answer <question-id> <n>` chooses option `<n>`, as `dvb status` and
`dvb wait` number them; `dvb answer <question-id> --reply <text>` answers in
words. It prints `Answered: <subject>`. With `--json` it prints:

```json
{"question": "01M6…", "subject": "Approve the plan for …?",
 "answer": {"choice": 1},
 "option": {"id": 1, "text": "Approve", "effect": "The tasks start"},
 "goal": "01M5…", "task": null, "open": false}
```

`answer` is `{"choice": <n>}` or `{"reply": "<text>"}`; `option` is the chosen
option, null for a reply. `goal` and `task` are what the question belongs to,
each null when it has none (a question about a setting has neither). `open` is
true when the question stays open after the answer: choosing to wait on the
default branch's failing checks keeps that question open until they pass.

### A plan on hold

Answering a plan's question with "Not yet" (any option that does not approve
the plan) parks the plan: the question closes, nothing runs and dvb does not ask
again. Status shows the goal as "Plan on hold" with a `Needs you:` line naming
`dvb resume <goal-id>` (in `--json`, a `needs_you_items` entry of kind `plan`),
though it leaves the "need you" count, and `dvb wait` on the goal ends with
code 6. `dvb resume <goal-id>` asks the same plan question again, for the same
plan, with no new planning, under a new question id: it prints
`Needs you: <subject> (<question-id>)`, and with `--json`
`{"goal": {...}, "question": {...}}`, the goal as `dvb goal drop --json` prints
it and the question as in `dvb status --json`'s `questions`. `dvb wait <goal-id>
--until plan` also prints the new question. Answering the old question is
refused with `ALREADY_TERMINAL`, its detail naming the new one: `this question
is already answered; the plan is asked again in question <question-id>`. A goal
with no plan on hold is refused with `PLAN_NOT_PARKED`. To get a different plan,
answer the question in words: a written reply has the orchestrator plan again.

### Waiting on a goal

`dvb wait <goal-id> [--until plan|done] [--timeout <minutes>] [--json]` blocks
until the goal reaches a point a script or agent acts on, and says which with its
exit code, so nothing has to poll `dvb status`. Only the named goal's tasks and
questions, and its project's pause, end the wait; a failed or stopped task or a
question of another goal does not. A state that already holds when the command
starts returns at once; `--timeout 0` checks once and exits 4 when nothing
holds. When the engine restarts during the wait, it reconnects and carries on.
Reviews do not stop the wait: they run without you, and you read them after
with `dvb task verdict` and `dvb task diff` ([Reading a review](#reading-a-review)).

| Code | Meaning and output |
|---|---|
| 0 | `--until done` (the default): the goal is Done; one `Merged PR #<n> as <commit> (task <id>)` line per merged pull request. `--until plan`: its plan question is open, printed as below, or its plan is already approved (the goal's status line). |
| 1 | dvb could not read the engine: a refusal or an unreadable reply. |
| 2 | A usage error only: a missing or unknown argument, as for every command. Nothing was checked. |
| 3 | A task of this goal failed, with or without a question: `Task <id> failed: <reason>` and `Retry it: dvb retry <id>`. |
| 4 | `--timeout` elapsed; `--timeout 0` checks once. |
| 5 | The goal does not exist (`dvb: refused: NOT_FOUND (no goal <id>)` on stderr) or was dropped (its status line). |
| 6 | Nothing goes on until you act. A task of this goal is stopped: `Task <id> stopped: <reason>` and `Resume it: dvb resume <id>`; or its plan is on hold ([A plan on hold](#a-plan-on-hold)): the goal's status line and `Ask it again: dvb resume <goal-id>`; or its project is paused: `Project <name> is paused: nothing new starts until dvb resume <name>`. |
| 7 | A question of this goal waits for you: `Question <id>: <subject>` and its numbered options, so `dvb answer <id> <n>` can follow. |

The checks go in this order: a failed task (3), then a question (7), then a
stopped task, a plan on hold or a paused project (6), so a question about a
stopped task is printed as the question.

```sh
dvb wait 01M5 --until plan && dvb answer <question-id> <n>
dvb wait 01M5 --timeout 120
```

With `--json`, a wait that ends on the goal prints one object instead of the
lines, with the same exit code; a timeout or an error prints nothing on stdout.
`outcome` says what ended it, `goal` and `state` are the goal's id and state,
and the rest depends on `outcome`. `state` is the goal's state as in
`dvb status --json`, except that a goal whose tasks all ended, some stopped and
none failed, is `stopped` rather than `failed`.

| `outcome` | Code | Also |
|---|---|---|
| `done` | 0 | `merged`: `[{"task", "pr", "commit"}]`, one per merged pull request |
| `plan_question` | 0 | `question`: as in `dvb status --json`'s `questions` |
| `plan_approved` | 0 | nothing |
| `question` | 7 | `question` |
| `failed` | 3 | `task`: `{"id", "title", "reason", "command": "dvb retry <id>"}` |
| `dropped` | 5 | nothing |
| `stopped` | 6 | `task`: `{"id", "title", "reason", "command": "dvb resume <id>"}` (`reason` null when nothing says why) |
| `plan_parked` | 6 | `command`: `dvb resume <goal-id>` |
| `project_paused` | 6 | `command`: `dvb resume <project-name>` |

```json
{"outcome": "stopped", "goal": "01M5…", "state": "stopped",
 "task": {"id": "01M7…", "title": "Add the parser",
          "reason": "not before Monday", "command": "dvb resume 01M7…"}}
```

## Following work

### Status

| Command | Output |
|---|---|
| `dvb status` | Overall running counts and questions with their options. |
| `dvb status <project>` | Unfinished goals and tasks, including failures, plus counts and questions. |
| `dvb status <project> --all` | All goals and tasks in the same order. |
| `dvb status --bar` | Counts as JSON, with `warnings` when the install has any. |
| `dvb status [<project>] [--all] --json` | The same content as the text, as one JSON object. |

Project status hides Done and Dropped goals by default, with a count and `--all`
hint. Goal titles use the first text line, limited to 160 characters with `…`
for longer text, followed by the state.
The "need you" count is the app's Needs you: open questions plus failed or stopped
tasks of a current goal, or of the project's last goal when it ended failed, until
they are retried or the goal is dropped or followed by a later goal. Each counted
item gets one `Needs you:` line: a question with its options, or a task with its
title, `Stopped` or `Failed`, and the command that continues it (`dvb resume
<task>` or `dvb retry <task>`). A stopped task's open question that recommends
leaving it stopped counts with the task; project status shows its options under
the task's line. A goal whose plan is on hold also gets a line,
`Needs you: <goal> (<goal-id>) - Plan on hold: dvb resume <goal-id>`, though it
is not counted, and so does a paused project:
`Needs you: Project <name> (<project-id>) - Paused: dvb resume <name>`, also
not counted. A paused project's status says so before its goals too:
`Project <name> is paused: nothing new starts until dvb resume <name>`. A task
waiting for an automatic retry is `Retrying`, with why: after a run's end, for
example `Work ended: crashed (exit 4: …); attempt 2 starts automatically`.
After the counts, `dvb status` prints a `Warning:` line for each fix the engine's
machine needs, such as run units whose version stamp is missing or differs from
the engine's ("the run units are older than this engine: re-run install.sh").
While the engine mirrors goals and tasks to GitHub it prints
`GitHub mirror pass running since <time>, on project <name>`, naming the
project whose job runs now.

`dvb status --json` prints the same content as one object:

```json
{"running": 1, "needs_you": 2,
 "warnings": [{"code": "cache_isolation_missing",
               "text": "the run units are older than this engine: re-run install.sh"}],
 "only": {"models": ["claude-opus-5-5"], "ends_at": 1792000000000},
 "mirror_pass": {"started": 1791990000000, "project": "01M4…", "project_name": "example"},
 "questions": [{"id": "01M6…", "project": "01M4…", "goal": "01M5…", "task": null,
                "subject": "Approve the plan for …?", "recommended": 1,
                "options": [{"id": 1, "text": "Approve", "effect": "The tasks start"},
                            {"id": 2, "text": "Not yet", "effect": "Nothing runs"}]}],
 "needs_you_items": [
   {"kind": "task", "id": "01M7…", "project": "01M4…", "project_name": "example",
    "title": "Add the parser", "state": "failed", "reason": "the build failed",
    "question": null, "command": "dvb retry 01M7…"},
   {"kind": "plan", "id": "01M3…", "project": "01M4…", "project_name": "example",
    "title": "Add a search box", "command": "dvb resume 01M3…"},
   {"kind": "project", "id": "01M4…", "project": "01M4…", "project_name": "example",
    "title": "example", "command": "dvb resume example"},
   {"kind": "question", "id": "01M6…", "project": "01M4…", "project_name": "example",
    "title": "Approve the plan for …?", "command": "dvb answer 01M6… <n>"}]}
```

`only` is the timed model switch, null when none is active; `mirror_pass` is
the GitHub mirror pass running now, null between passes; `warnings` is empty
when the install needs nothing. `questions` lists every open question (of the
project, with one), including one counted with its stopped task, whose id is
that task item's `question`. `needs_you_items` has one entry per `Needs you:`
line, in the text's order; `command` continues it, with `<n>` an option's `id`
(or use `--reply <text>`). Each names its project by id (`project`) and name
(`project_name`). A task item's `state` is `failed` or `stopped`, and `reason`
the failure or why it stopped, the same as the task's `reason` in `tasks`, else
null. A `plan` item is a goal whose plan is on hold: its `id` is the goal's and
its `title` the goal's first line; it is not in the `needs_you` count. A
`project` item is a paused project (of the project shown, with one): its `id`
and `project` are the project's id, its `title` its name; it is not in the
`needs_you` count either.

With a project, the object also has `project` (`{"id", "name", "paused"}`,
`paused` true while the project is paused), the goals and tasks the text lists,
and `finished_goals_hidden`, the Done and Dropped goals left out without `--all`:

```json
{"project": {"id": "01M4…", "name": "example", "paused": false},
 "goals": [{"id": "01M5…", "state": "in_progress", "text": "Add a parser",
            "plan_parked": false, "waits_for": null}],
 "finished_goals_hidden": 3,
 "tasks": [{"id": "01M7…", "goal": "01M5…", "state": "in_review",
            "title": "Add the parser", "role": "01M2…", "role_name": "backend",
            "pr": 42, "reason": null, "waiting_for": []}]}
```

A goal's `text` is its whole text, as `dvb goal drop --json` prints it. A
task's `role` is the role's id and `role_name` its slug, as `dvb role list`
prints them. A task's `state` is
the engine's name for it: `planned`, `preparing`, `running`, `collecting`,
`publishing`, `in_review`, `awaiting_checks`, `merging`, `retry_pending`,
`stopped`, `unknown`, `done`, `cancelled` or `failed`. `pr` is null until it
has a pull request; `reason` is the failure, why it stopped, what it waits for
or why it retries, else null;
`waiting_for` lists the unmerged tasks a planned task waits on. There is no
`dvb task list`: read a project's tasks and their states here.

### Watching events

`dvb watch` prints events as they happen, one line each: the event id, its
kind and what happened. `--after <id>` starts after that event, `--once` prints
what has happened so far and exits, and `--project`, `--role`, `--task` and
`--kind` (repeatable) filter. When the engine restarts, it reconnects. A line
that names a role gives its id followed by its slug, for example
`01M3… (backend) started 01M7…`.

`dvb watch --json` prints one JSON object per line: the event as the engine
serves it, with `time` (RFC 3339, UTC) and `text`, the line's wording when the
kind has one, naming a role the same way. `payload` holds the kind's fields,
with the bare role id, as in [contracts/events.md](../contracts/events.md):

```json
{"id": 1204, "at": 1792800000000, "time": "2026-10-24T00:00:00.000Z",
 "actor": "owner", "project": "01M4…", "kind": "goal.stated",
 "payload": {"goal": "01M5…", "text": "Add a parser", "from_issue": null},
 "evidence": null, "text": "Goal: Add a parser"}
```

To follow without missing or repeating an event, keep the last printed `id` and
pass it as `--after <id>` the next time.

### Reading a review

`dvb task diff <task>` prints the task's published change against its base as
a unified diff; `dvb task verdict <task>` prints the latest review of that
change: the head, the count of open findings and the summary, then each
finding with its state, severity, place and detail. Both are refused with
`HEAD_MISMATCH` before the task has published a change, and the verdict is
`NOT_FOUND` until its current head is reviewed.

`dvb task diff <task> --json` prints
`{"task": "01M7…", "base": "<commit>", "head": "<commit>", "diff": "diff --git …"}`.
`dvb task verdict <task> --json` prints the review as the engine records it,
with `role_name`, the reviewer role's slug beside its id in `role`:

```json
{"verdict": "01M8…", "task": "01M7…", "head": "<commit>", "run": "01M9…",
 "role": "01M3…", "role_name": "code-reviewer",
 "model": "claude-opus-5-5", "vendor": "claude",
 "open_findings": 1, "summary": "Needs work",
 "findings": [{"severity": "high", "file": "src/parse.rs", "line": 12,
               "title": "Empty input panics", "detail": "…", "open": true}]}
```

`severity` is `high`, `medium` or `low`; `line` is null for a finding about a
whole file.

### Retrying and sending a task back

`dvb retry <task> [<text>]` retries a failed task, with the text as its
builder's retry context (at most 16 KiB). It also sends a task that has a PR
back to its builder with the text: a task stopped by you or by dvb (for example
after its review rounds are used up, or held on an unmet completion condition),
or one in review. It gets one builder round on the PR's head and branch, even
past the review-round cap, then review and CI as usual; a review in progress is
stopped first, so the old head is never merged as it is. The task's reviewers
judge the new head against every text it was sent back with. A task stopped on
its builder's blocked, out-of-role or other partial result resumes its builder
with the text.

```sh
dvb retry 01M597ZQ "Remove the PDF; generate it in the test instead."
```

It prints `Task <id>: <state>`. With `--json` it prints `{"task": {...}}`, the
task as `dvb status <project> --json` lists it in `tasks`, after the retry:

```json
{"task": {"id": "01M7…", "goal": "01M5…", "state": "planned",
          "title": "Add the parser", "role": "01M2…", "role_name": "backend",
          "pr": null, "reason": null, "waiting_for": []}}
```

A task stopped with kept, unpublished work and no PR (for example held on an
unmet completion condition before its first publication) gets one builder
round on that work with the text, then the completion check, publishing,
review and CI as usual. Without text, or with no kept work, a stopped task with
no PR is resumed with `dvb resume`; a done or cancelled task is refused. The
orchestrator can do the same after your yes to a question naming the task.

When a builder reports an unmet completion condition and you answer "Merge
anyway", the waiver stays with the task: a later round of it that reports the
same condition goes on to review and CI without asking again, and its builders
and reviewers are told the condition is waived. A different condition asks again.

## Team and models

### Hires

| Command | Action |
|---|---|
| `dvb role list <project>` | List roles and their ids. |
| `dvb hire <role-id> <model> [--effort <effort>] [--routine]` | Replace the role's active hires of the same kind (default, or routine with `--routine`); print the new id and retired ids/models/efforts. The effort defaults to `medium`. |
| `dvb hires <project> [--all]` | List active hires in role order; `--all` includes `[retired]` records. |
| `dvb history hire <hire-id>` | Read the hire's versions, including retired records. |
| `dvb restore hire <hire-id> <event> [--current <latest-event>]` | Restore a version; restoring an active hire replaces the role's other active hires of its kind. |

Hire rows show role name, role id, hire id, model and effort. When a role has
more than one active hire, `[default]` marks its default hire: new builder tasks
use the first active hire in the selected pool; orchestrators and reviewers use
the first active hire. A routine hire is never the default while the role has
another active hire.

`[routine]` marks a role's routine hire (`dvb hire ... --routine`), for example
`dvb hire backend gpt-6.1-sol --routine`. The orchestrator marks routine tasks:
follow-up fixes, tests, docs, chores and small mechanical changes. A routine
task's builder rounds run on the routine hire, and so does every fix round on
an open PR (CI fix, review fix, retry with text), whatever the task. After two
send-backs (review findings or failed CI) of PRs built on the routine hire, the
task's next rounds run on the default hire. A role with no routine hire runs
everything on its default hire. Task views carry `routine` and `round_model`,
the model the latest builder round ran on.

### Timed model selection

| Command | Action |
|---|---|
| `dvb only <model> [<model>…] --hours <n>` | Restrict new runs across the install for 1–168 whole hours. |
| `dvb only` | Show the ordered models and local end time. |
| `dvb only --clear` | End the restriction immediately. |

Use exact ids from the installed and selected model pools. A refusal lists valid
ids. Model order matters; setting another restriction replaces the current one.

Hires whose models are in the list use their own model and effort. Other roles,
including the orchestrator and reviewers, use the first listed model at their
own effort or its nearest supported effort.
Selection happens at launch. Queued runs follow the current restriction, while
running assignments stay fixed. Hires and settings are not edited.

A limited vendor waits; usage fallback stays within the listed models. Task and
feed evidence show hired and actual models, including same-vendor switched or
fallback reviews. The restriction survives restart. Expiry returns new runs to
their hires, including after downtime. Global and project status show an active
restriction and its local end time.

```sh
dvb only claude-opus-5-5 --hours 24
dvb only
dvb only --clear
```

## Install-wide

### Run capacity

`dvb settings` prints the settings as JSON (see [Settings](#settings)).
`dvb settings set max_runs 6` sets the install-wide number of runs that may go at
once, including builders, reviewers and orchestrators.
The default is 3; use a whole number of at least 1.
The app's Settings → Preferences has the same Maximum runs control.

Lowering the limit leaves running work alone and holds new starts until capacity
is available. Raising it wakes waiting work automatically. Each run may use up to
8 GB of memory; memory, disk and load holds also apply. Values survive restart
and use settings history.

Each run has a fixed time limit from its launch: 4 hours for builders and clerks,
90 minutes for reviewers, 45 minutes for orchestrators. A run that reaches it is
stopped, and its task is retried or fails. A reviewer still running a minute after
its verdict and its last output is ended as finished.

### Settings

`dvb settings` and `dvb settings get` print the same JSON: every setting's
current value, with the defaults filled in. `dvb settings set <key> <JSON>` sets
one, for example `dvb settings set vendor_fallback false` or
`dvb settings set question_format '"swot"'`; the value is JSON, so a string
needs its quotes.

`dvb settings publication` prints the identity dvb's published commits name as
author and committer, for example `dvb publishes commits as dvb <dvb@dvb.invalid>
(the default)`. `dvb settings publication --name <name> --email <email>` changes
it (either one alone keeps the other); the next commit dvb publishes uses it,
with no restart.

dvb keeps the owner's identity out of what it publishes: the name and email
can't contain the owner's name or email as recorded at GitHub setup. Neither may
be empty, contain a line break, `<` or `>`, or match a secret pattern of the
[publication scan](#publication-scan). A refusal names the field and the rule that matched, for
example `The commit author name can't contain your name (…) unless you publish
as yourself.`; the command adds the flags that change it (`--as-owner` only
for that owner rule).

`dvb settings publication --as-owner` (with `--name` and `--email` as needed)
publishes as the owner: the name and email may then be the owner's, and the
command adds `, as you: they count on that GitHub account`. Only that rule is
lifted; the other rules still apply, and the scan of published files is
unchanged. `--neutral` turns it off again (the default) and goes back to dvb's
default name and email unless `--name` or `--email` is given.

| Key | Values | Default |
|---|---|---|
| `max_runs` | whole number, at least 1 | `3` |
| `runs_memory_gb` | whole GiB, at least 2: the memory limit of all runs together (`dvb.slice`), changed live | the slice's limit (half RAM at install) |
| `response_style` | `"Simple"`, `"Clear"`, `"Technical"` or `"Expert"` | `"Clear"` |
| `question_format` | `"simple"`, `"quick_compare"` or `"swot"` | `"quick_compare"` |
| `model_mode` | `"Custom"` or `"Auto"` | `"Custom"` |
| `model_pool` | non-empty list of installed model ids (`dvb settings` lists them in `model_pool` until you set it) | every installed model |
| `vendor_fallback` | `true` or `false`: move a limited vendor's runs to the orchestrator's vendor | `true` |
| `notify_faults` | `true` or `false` | `true` |
| `notify_completed` | `true` or `false` | `false` |
| `theme` | theme name: lowercase letters, digits and `-` | `"auto"` |
| `font` | font name | `"Auto"` |
| `text_size` | `"S"`, `"M"`, `"L"` or `"XL"` | `"L"` |
| `motion` | `"Follow system"`, `"On"` or `"Off"` | `"Follow system"` |
| `task_panels` | `{"review": <bool>, "checks": <bool>}` | `{"review": true, "checks": true}` |
| `task_files_right` | `true` or `false` | `false` |
| `autonomous_projects` | list of project names, for example `'["example"]'` (see below) | `[]` |

In a project named in `autonomous_projects`, dvb answers two kinds of question
itself with the question's recommended option, recorded as answered by dvb: a
goal's plan question whose recommended option approves the plan (the plan's
tasks then start), and a what-next proposal whose recommended option has an
effect, when it names its issues (`ask`'s `issues`, or the issues the proposed
goal carries). Every other question goes to the owner as before: a recommended
"Not yet", merge anyway, failure, stop and retry questions, and proposals naming
no issue. A name that is not a project is refused with `INVALID_VALUE`.

### Publication scan

Before dvb pushes a task's change, it scans the changed files, the commit message,
the PR text and the other text it sends to GitHub for secret patterns and for the
owner's identity. A hit pushes nothing, and the feed shows `Publish blocked: <rule> in
<file>`. A hit in the builder's own change gets one fix round, told the file, line
and rule; the task fails if its next head still hits, and any other hit fails it
at once.

A file that must keep a match, such as a test fixture, can be allowed with
`.dvb/scan-ignore.toml` in the repository:

```toml
[[ignore]]
file = "fixtures/example.txt"
rule = "github-token"
```

Each entry allows only that exact file and rule pair; it does not apply to PR text
or file paths, and allowing one rule never hides another rule that matches. dvb
reads the file only from the commit the task started from, so it must be on the
default branch first: a run cannot allow its own new fixture. A malformed file
refuses the scan.

## Other commands

`dvb <command> --help` gives each one's arguments and flags.

| Command | Action |
|---|---|
| `dvb stop <task> [--reason <text>] [--json]` | Stop a task, in any state; its branch, workspace and results are kept. The reason is recorded and shown as the task's `reason`; with a reason the orchestrator is not woken to ask why, without one it is woken and may ask you. |
| `dvb pause <project> [--json]` | No new runs or steps start until the project is resumed; what is running carries on. `dvb wait` on its goals ends with code 6. |
| `dvb resume <project\|task\|goal> [--json]` | Unpause a project, resume a stopped task from where it stopped, or ask a goal's plan on hold again under a new question id ([A plan on hold](#a-plan-on-hold)). An id that is none of these is refused: `NOT_FOUND (no task, goal or project <id>)`. |
| `dvb redirect <goal> <text>` | Change course: the goal's unstarted tasks are dropped and the orchestrator plans the new text, then asks for your yes. |
| `dvb brief <project> [<text>\|--clear]` | Show, set or remove the project's brief: kept only in dvb, it goes into the orchestrator's and every builder's prompt and is never sent to GitHub. |
| `dvb usage [<project>]` | Tokens, estimated cost and each vendor's usage window. |
| `dvb report bug\|feature --title <title> <text>` | Prepare a bug report or feature request: prints the issue and the GitHub link that opens it. Nothing is filed. |
| `dvb keybind [--keys <keys>] [--file <file>]` | Add one Hyprland keybinding that opens the app (default `SUPER + ALT + D` in `~/.config/hypr/bindings.lua`). |
| `dvb login claude\|codex` | Log a vendor's CLI in as the worker user: prints the link to open and, for Claude, takes the code the page shows. |
| `dvb update [--check \| --to <version>]` | Install the newest release; `--check` only says whether one exists, `--to` installs that version (going back needs the same database schema). |
| `dvb role add <project> --name <name> [...]` | Add a role: its job, lane (what it decides), lead, who it reports to, who reviews it, review instructions, group and order. |
| `dvb role edit <project> <role> [...]` | Change a role: only the fields you give change; an empty `--reports-to`, `--reviewed-by` or `--group` clears it. |
| `dvb chart <project> report\|group\|reorder\|swap ...` | Move roles on the chart: who reports to whom, groups, order, or swap a role with its lead. |
| `dvb retire <hire-id>` | Retire a hire, keeping its record. |
| `dvb project repo <project> <owner/name>` | Point a project at its repository's new name after a rename; goals, tasks, history and the GitHub mirror stay with it, and a name that is not the same repository is refused (`REPOSITORY_MISMATCH`). Waits at most about 10 seconds for the GitHub mirror job running now; after that it is refused with nothing changed (`MIRROR_PASS_RUNNING`), saying when the pass started: try again. |
| `dvb project remove <project>` | Remove a project from dvb: it leaves `project list`, status and the app and takes no goal; its work directories, mirror, toolchain cache and GitHub mirror receipts on this machine are freed, its history stays and nothing changes on GitHub. Refused while it has running or open tasks (`PROJECT_BUSY`), naming each and how to stop it. Stopped tasks' unpublished work on this computer is deleted. Waits at most about 10 seconds for the GitHub mirror job running now; after that it is refused with nothing removed (`MIRROR_PASS_RUNNING`), saying when the pass started: try again. If a toolchain cache is not freed, this folder was made by an older dvb and can be deleted as root: `sudo rm -rf <path>`. |
| `dvb project list [--json]` | List projects: name, id and repository, one per line; `--json` prints `{"projects": [...]}`, each as `project add --json` prints it. |

With `--json`, `stop`, `retry` and `resume <task>` print `{"task": {...}}` as
[`dvb retry`](#retrying-and-sending-a-task-back) does; `pause` and
`resume <project>` print `{"project": {...}}`, the project after the change as
`dvb project list --json` lists it (`paused` true or false); `resume <goal>`
prints `{"goal": {...}, "question": {...}}` ([A plan on hold](#a-plan-on-hold)).

## Scripting

`status`, `watch`, `wait`, `goal`, `goal drop`, `goal after`, `task diff`,
`task verdict`, `answer`, `project add` and `project list` take `--json`: the
output on stdout is JSON, one object (one per line for `watch`), with the
shapes above. Errors go to stderr as text and exit 1: a refusal, including a
name or id with nothing behind it, is `dvb: refused: <CODE> (<detail>)`, for
example `dvb: refused: NOT_FOUND (no project example)`; an engine dvb cannot
reach is `dvb: <what went wrong>`. Exit 2 is a usage error only: a missing or
unknown argument, printed with the usage. Ids are strings, and times in the
engine's objects are milliseconds since the Unix epoch. `dvb doctor --json` and
`dvb github connect --json` are described with their commands, and `dvb wait`
says what happened with its own exit codes.
