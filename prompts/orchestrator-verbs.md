## Orchestrator verbs

| Verb | Body |
|---|---|
| `plan` | `{"goal"?, "issues"?: [<issue number>], "tasks": [{"title", "role", "summary", "review": "code" or "design", "routine"?: true, "after"?: [<1-based task position or existing task id>]}]}` |
| `ask` | `{"subject", "options": [{"text", "effect", "approves_plan", "no_effect"?, "retries_task"?}], "recommended": <1-based>, "format"?: "simple" or "quick_compare" or "swot", "issues"?: [<issue number>]}` |
| `task retry` | `{"task", "text"?}`: a failed task, or one stopped or in review with a PR, after the owner's yes |
| `hire` | `{"role", "model", "effort"?}` |
| `retire` | `{"hire"}` |

For `plan`, propose a plan with `dvb run plan`, then put it to the owner with
`dvb run ask` (one option with `"approves_plan": true`). Start no task before the
owner's yes. Omit `goal` when planning a stated goal.

Start tasks with `dvb run task start`, one role per task. The model guidance
above says when to name `model` and `effort`. Normally omit `model` and
`effort` to use the role's active hire and its effort. After `NO_ACTIVE_HIRE`,
hire a model from the selected pool for that role with `dvb run hire` yourself
(no owner step), then start the task again in the same run.

Mark each routine task yourself with `"routine": true` in `plan` (or in
`task start`): follow-up fixes, tests, docs, chores and small mechanical
changes. Leave `routine` out for new features, new modules, UI work and
anything that needs design choices. The approval question shows which tasks
are routine. A routine task's builder rounds run on the role's routine hire
(the project summary shows `Active routine hire`), and every fix round on an
open PR does too, whatever the task. After a routine round's PR is sent back
twice (review findings or failed CI), the next rounds go to the role's default
hire. A role with no routine hire runs everything on its default hire.

For `task retry`, the owner's yes is the answer to your `ask` choosing an option
whose `retries_task` names the task, given since it last failed or stopped and
since its last retry. One yes allows one retry. Without it the verb is refused
with `GOAL_NOT_APPROVED`. The retry is a new run in the task's kept workspace,
or a builder round on its PR's head with the retry text (one more round, even
past the review-round cap), then review and CI; the task's goal carries on. A
stopped task with kept, unpublished work and no PR, retried with text, gets one
builder round on that work with the text, then its completion check,
publishing, review and CI. Without text, or with no kept work, a stopped task
with no PR is resumed, not retried.

The retried builder receives no new instruction except optional retry `text`.
Pass any owner-approved retry instruction in that text (at most 16 KiB of UTF-8).
It supplies missing information and does not replace the task. Later retries
replace it for the builder; omitting text clears it. Blank text is absent.
Reviewers judge the head against every retry text the task was sent back with.
