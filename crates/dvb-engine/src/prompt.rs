//! What goes into a run's prompt beyond the role's template:
//! the owner's response style, the verb help, the project summary, the
//! events since the orchestrator last ran, the owner's pending reply, and for a
//! reviewer or a builder the task summary, head, base and findings. Values are
//! plain text; the template (`prompts/*.md`) says where each goes.

use crate::engine::{AppState, Fail};
use crate::proj::{Project, Projections, Run};
use dvb_core::event::Answer;
use dvb_core::roles::ReviewKind;
use dvb_core::{Event, EventId, EventKind};

const RUN_VERBS: &str = include_str!("../../../prompts/run-verbs.md");
const ORCHESTRATOR_VERBS: &str = include_str!("../../../prompts/orchestrator-verbs.md");
/// The orchestrator's `{{model_guidance}}` for each `model_mode`.
pub const MODEL_MODE_CUSTOM: &str = include_str!("../../../prompts/model-mode-custom.md");
pub const MODEL_MODE_AUTO: &str = include_str!("../../../prompts/model-mode-auto.md");

#[expect(
    clippy::expect_used,
    reason = "Malformed bundled prompt templates are a build defect"
)]
static FRAGMENTS: std::sync::LazyLock<std::collections::BTreeMap<String, String>> =
    std::sync::LazyLock::new(|| {
        toml::from_str(include_str!("../../../prompts/fragments.toml"))
            .expect("invalid bundled fragments.toml")
    });

pub(crate) fn fragment_text(name: &str) -> &'static str {
    &FRAGMENTS[name]
}

pub(crate) fn fragment(name: &str, vars: &[(&str, String)]) -> String {
    render(fragment_text(name), vars, false)
}

/// The variables `context` can fill; every template placeholder is one of these,
/// or one the launcher fills itself (a test checks).
pub const FILLED: [&str; 24] = [
    "autonomy",
    "response_style",
    "question_format",
    "run_verbs",
    "orchestrator_verbs",
    "browser",
    "model_pool",
    "model_guidance",
    "project_summary",
    "notes",
    "events_since_last_wake",
    "pending_reply",
    "head",
    "base",
    "review_instructions",
    "task_summary",
    "review_findings",
    "merge_main",
    "project_brief",
    "failures",
    "retry_text",
    "owner_goal",
    "check_results",
    "pr_description",
];

/// The release prompt template for a role.
pub(crate) fn template(slug: &str) -> &'static str {
    match slug {
        dvb_core::roles::ORCHESTRATOR => include_str!("../../../prompts/orchestrator.md"),
        dvb_core::roles::CODE_REVIEWER => include_str!("../../../prompts/code-reviewer.md"),
        dvb_core::roles::DESIGN_REVIEWER => include_str!("../../../prompts/design-reviewer.md"),
        dvb_core::roles::CLERK => include_str!("../../../prompts/clerk.md"),
        _ => include_str!("../../../prompts/builder.md"),
    }
}

/// The default response style, also verified by the settings tests.
#[cfg(test)]
pub(crate) const DEFAULT_STYLE: &str = "Clear";

#[expect(
    clippy::expect_used,
    reason = "Malformed bundled instructions are a build defect and must fail loudly"
)]
static RESPONSE_STYLES: std::sync::LazyLock<std::collections::BTreeMap<String, String>> =
    std::sync::LazyLock::new(|| {
        toml::from_str(include_str!("../../../prompts/response-styles.toml"))
            .expect("invalid bundled response-styles.toml")
    });

/// The instruction for a style name (any case); Clear for an unknown one.
pub fn style_instruction(name: &str) -> String {
    RESPONSE_STYLES
        .get(&name.to_lowercase())
        .unwrap_or(&RESPONSE_STYLES["clear"])
        .clone()
}

fn clip(s: &str, max: usize) -> String {
    let kept = dvb_core::text::clip(s, max);
    if kept.len() < s.len() {
        format!("{kept}…")
    } else {
        s.to_owned()
    }
}

/// An event as one line for the orchestrator, or `None` for run chatter.
fn event_line(ev: &Event) -> Option<String> {
    let kind = ev.kind.kind();
    if ev.kind.is_prompt_chatter() {
        return None;
    }
    let payload = serde_json::to_value(&ev.kind)
        .ok()
        .and_then(|v| v.get("payload").map(ToString::to_string))
        .unwrap_or_default();
    Some(format!("- {kind}: {}", clip(&payload, 240)))
}

/// The fixed window from the preceding orchestrator start to this one's start.
/// Later events belong to the next run, even if they arrive before rendering.
pub(crate) fn orchestrator_window(events: &[Event], current: &Run) -> (EventId, EventId) {
    let mut previous = EventId(0);
    let failed = crate::wake::failed_runs(events);
    for ev in events {
        if ev.project.as_ref() == Some(&current.project)
            && let EventKind::RunStarted(a) = &ev.kind
            && a.is_orchestrator()
        {
            if a.run == current.assignment.run {
                return (previous, ev.id);
            }
            // A run that failed was retried: its window carries over.
            if !failed.contains(&a.run) {
                previous = ev.id;
            }
        }
    }
    (EventId(0), EventId(0))
}

/// Events of this project between the previous and current starts, newest last.
pub fn events_since(events: &[Event], project: &Project, current: &Run) -> String {
    let mine = |ev: &Event| ev.project.as_ref() == Some(&project.view.id);
    let (cutoff, end) = orchestrator_window(events, current);
    let lines: Vec<String> = events
        .iter()
        .filter(|ev| ev.id > cutoff && ev.id < end && mine(ev))
        .filter_map(event_line)
        .collect();
    let skip = lines.len().saturating_sub(40);
    lines[skip..].join("\n")
}

/// The owner's answers in this run's fixed window, as "question -> answer".
pub fn pending_replies(events: &[Event], project: &Project, current: &Run) -> String {
    let mine = |ev: &Event| ev.project.as_ref() == Some(&project.view.id);
    let (cutoff, end) = orchestrator_window(events, current);
    let mut out = Vec::new();
    for ev in events
        .iter()
        .filter(|ev| ev.id > cutoff && ev.id < end && mine(ev))
    {
        let EventKind::QuestionAnswered { question, answer } = &ev.kind else {
            continue;
        };
        let asked = events.iter().find_map(|o| {
            if let EventKind::QuestionOpened {
                question: q,
                subject,
                options,
                lapses_on,
                ..
            } = &o.kind
                && q == question
            {
                // The goal the question is about, so replies about different
                // goals are not taken for one.
                let goal = match lapses_on {
                    dvb_core::event::LapsesOn::GoalVersion(v) => {
                        events.iter().find(|g| g.id == *v).and_then(|g| {
                            if let EventKind::GoalStated { text, .. } = &g.kind {
                                Some(text.clone())
                            } else {
                                None
                            }
                        })
                    }
                    dvb_core::event::LapsesOn::ValueVersion(_) => None,
                };
                let subject = match goal {
                    Some(g) => format!("{subject} [goal: {g}]"),
                    None => subject.clone(),
                };
                Some((subject, options.clone()))
            } else {
                None
            }
        });
        let (subject, options) = asked.unwrap_or_default();
        let said = match answer {
            Answer::Choice(n) => options.iter().find(|o| o.id == *n).map_or_else(
                || format!("option {n}"),
                |o| match &o.retries_task {
                    // The owner's yes to retrying it: `dvb run task retry`.
                    Some(t) => format!("{} ({}) [retries_task: {t}]", o.text, o.effect),
                    None => format!("{} ({})", o.text, o.effect),
                },
            ),
            Answer::Reply(text) => text.clone(),
        };
        out.push(format!("- {subject} -> {said}"));
    }
    out.join("\n")
}

/// Every goal not done, in the order stated (else the newest).
fn summary_goals<'a>(proj: &'a Projections, p: &Project, run: &Run) -> Vec<&'a crate::proj::Goal> {
    let mut goals = proj.goals_in_order(&p.view.id);
    goals.retain(|g| {
        g.state != dvb_core::api::GoalState::Done || Some(g.version) == run.goal_version
    });
    if goals.is_empty() {
        goals.extend(proj.latest_goal(&p.view.id));
    }
    goals
}

/// The fenced issue content a goal carries, after its text.
fn goal_issues(issues: &[dvb_core::event::GoalIssue]) -> String {
    if issues.is_empty() {
        return String::new();
    }
    let texts: Vec<&str> = issues.iter().map(|i| i.text.as_str()).collect();
    fragment("goal-issues", &[("issues", texts.join("\n\n"))])
}

/// Issues the listed goals mention by `#n`, read now for the orchestrator.
async fn mentioned_issues(e: &AppState, p: &Project, run: &Run) -> String {
    let (text, carried) = {
        let proj = e.proj();
        let goals = summary_goals(&proj, p, run);
        let text = goals
            .iter()
            .map(|g| g.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let carried: Vec<u64> = goals
            .iter()
            .flat_map(|g| g.attached.iter().map(|i| i.number))
            .collect();
        drop(proj);
        (text, carried)
    };
    crate::issues::mentioned(
        e,
        &dvb_core::forge::Repo(p.view.repo.clone()),
        &text,
        &carried,
    )
    .await
}

/// For a run bound to a finished goal or a proposal request, which proposes
/// the next goal: the open trusted issues, read from the forge for every
/// prompt so issues opened or labelled since are included.
async fn proposal_issues(e: &AppState, p: &Project, run: &Run) -> String {
    let proposing = {
        let proj = e.proj();
        proj.goals.values().any(|g| {
            g.project == p.view.id
                && Some(g.version) == run.goal_version
                && g.state == dvb_core::api::GoalState::Done
        })
    };
    if !proposing {
        return String::new();
    }
    crate::issues::open_trusted(e, &dvb_core::forge::Repo(p.view.repo.clone())).await
}

fn project_summary(e: &AppState, p: &Project, run: &Run) -> String {
    let proj = e.proj();
    let mut s = fragment(
        "project-summary",
        &[
            ("name", p.view.name.to_string()),
            ("repo", p.view.repo.to_string()),
            ("branch", p.view.default_branch.to_string()),
        ],
    );
    s.push_str("\nConfigured roles and active hires:\n");
    for role in &p.roles {
        s.push_str(&format!(
            "- {}: {} Lane: {}\n",
            role.slug, role.job, role.lane
        ));
        let hires: Vec<_> = p
            .hires
            .iter()
            .filter(|h| h.role == role.id && h.status == dvb_core::roles::HireStatus::Active)
            .collect();
        if hires.is_empty() {
            s.push_str("  No active hire.\n");
        }
        for hire in hires {
            s.push_str(&format!(
                "  Active {}hire {}: {}, effort {}\n",
                if hire.routine { "routine " } else { "" },
                hire.id,
                hire.model,
                hire.effort.as_str()
            ));
        }
    }
    let bound = run.goal_version;
    for g in summary_goals(&proj, p, run) {
        let mark = if Some(g.version) == bound {
            fragment_text("bound-goal")
        } else {
            ""
        };
        s.push_str(&fragment(
            "goal-summary",
            &[
                ("state", format!("{:?}", g.state)),
                ("mark", mark.to_string()),
                ("goal", g.text.to_string()),
            ],
        ));
        s.push_str(&goal_issues(&g.attached));
        for t in g.tasks.iter().filter_map(|t| proj.tasks.get(t)) {
            let role = p
                .role(&t.view.role)
                .map_or(t.view.role.as_str(), |r| r.slug.as_str());
            let hire = crate::dispatch::round_hire(p, t, None).map_or_else(
                || fragment_text("no-active-hire").to_owned(),
                |h| {
                    let effort = h.effort.as_str();
                    let routine = if t.view.routine { ", routine task" } else { "" };
                    format!("{}, {effort}{routine}", h.model)
                },
            );
            s.push_str(&fragment(
                "task-summary",
                &[
                    ("task", t.view.id.to_string()),
                    ("title", t.view.title.replace(['\r', '\n'], " ")),
                    ("role", role.to_string()),
                    ("hire", hire.to_string()),
                    ("state", format!("{:?}", t.view.state)),
                ],
            ));
            if !t.view.after.is_empty() {
                s.push_str(&format!(
                    "\n  Task {} starts after PRs for {} merge. {}",
                    t.view.id,
                    t.view
                        .after
                        .iter()
                        .map(dvb_core::TaskId::as_str)
                        .collect::<Vec<_>>()
                        .join(", "),
                    t.view.reason.as_deref().unwrap_or_default()
                ));
            }
            if let Some(reason) = t.view.outcome.reason() {
                s.push_str(&format!(
                    "\n  Task {} builder outcome: {}. Reason: {reason}",
                    t.view.id,
                    serde_json::json!(t.view.outcome)
                ));
            }
            if t.stopped_blocked()
                && let Some(b) = proj.blocking_task(t).filter(|b| b.merged.is_none())
            {
                s.push_str(&format!(
                    "\n  Task {} resumes by itself when task {} merges.",
                    t.view.id, b.view.id
                ));
            }
            // Planner recovery can precede individual task reconciliation.
            let mut state = &t.view.state;
            while let dvb_core::TaskState::Unknown { last } = state {
                state = last;
            }
            if let dvb_core::TaskState::Stopped { was } = state {
                s.push_str(&fragment(
                    "stopped-task",
                    &[
                        ("task", t.view.id.to_string()),
                        ("stopped", dvb_core::wording::stopped_word(was).to_owned()),
                        (
                            "reason",
                            t.view
                                .reason
                                .clone()
                                .unwrap_or_else(|| fragment_text("failure-no-reason").to_owned()),
                        ),
                    ],
                ));
            }
            if Some(g.version) == bound
                && g.version > g.stated
                && Projections::needs_plan(g)
                && matches!(state, dvb_core::TaskState::Stopped { was } if **was == dvb_core::TaskState::InReview)
                && t.facts.round >= dvb_core::caps::REVIEW_ROUNDS_PER_PR
                && t.facts
                    .verdict
                    .as_ref()
                    .is_some_and(|v| t.view.head.as_ref() == Some(&v.head) && v.open > 0)
            {
                s.push_str(fragment_text("exhausted-task"));
                if let Some(branch) = &t.branch {
                    s.push_str(&fragment("old-branch", &[("branch", branch.clone())]));
                }
            }
            if t.view.state == dvb_core::TaskState::Planned
                && t.approved
                && g.state == dvb_core::api::GoalState::Approved
                && t.facts.run.is_none()
                && t.waiting
                    .as_ref()
                    .is_none_or(|w| w.approved_version.is_none_or(|v| v == g.version))
                && proj.needs_orchestrator(t, &e.pool)
            {
                s.push_str(fragment_text("returned-task"));
                if let Some(w) = &t.waiting {
                    s.push_str(&fragment("saved-start", &[("choice", serde_json::json!({"model": w.model, "effort": w.effort, "reason": w.reason}).to_string())]));
                }
            }
        }
    }
    s
}

/// The open findings of the last verdict on a task, for its next builder run.
pub fn findings_for(events: &[Event], task: &dvb_core::TaskId) -> String {
    let open = open_findings_of(events, task, None);
    if open.is_empty() {
        String::new()
    } else {
        fragment("findings", &[("findings", open.join("\n"))])
    }
}

/// The open findings of the last verdict for this review stage: a fix round
/// may have changed nothing, and the other stage's verdict cannot close them.
pub fn findings_to_recheck(
    events: &[Event],
    task: &dvb_core::TaskId,
    stage: Option<ReviewKind>,
) -> String {
    let open = open_findings_of(events, task, stage);
    if open.is_empty() {
        String::new()
    } else {
        fragment("recheck-findings", &[("findings", open.join("\n"))])
    }
}

fn open_findings_of(
    events: &[Event],
    task: &dvb_core::TaskId,
    stage: Option<ReviewKind>,
) -> Vec<String> {
    let last = events.iter().rev().find_map(|ev| {
        if let EventKind::VerdictRecorded {
            task: t,
            findings,
            review_stage,
            ..
        } = &ev.kind
            && t == task
            // Builders and historical reviewer runs use the latest task
            // verdict. Historical unstaged verdicts remain recheckable.
            && (stage.is_none() || review_stage.is_none() || *review_stage == stage)
        {
            Some(findings.clone())
        } else {
            None
        }
    });
    last.into_iter()
        .flatten()
        .filter(|f| f.open)
        .map(|f| {
            let at = f.line.map_or(f.file.clone(), |l| format!("{}:{l}", f.file));
            format!("- [{:?}] {at}: {}. {}", f.severity, f.title, f.detail)
        })
        .collect()
}

/// Why the task failed since its head was last published (such as the CI
/// failure that started this fix round), oldest first; empty when it did not.
pub fn failures_for(events: &[Event], task: &dvb_core::TaskId) -> String {
    let mut reasons: Vec<String> = events
        .iter()
        .rev()
        .take_while(|ev| {
            !matches!(&ev.kind, EventKind::BranchPublished { task: t, .. }
                | EventKind::BranchUpdated { task: t, .. } if t == task)
        })
        .filter_map(|ev| {
            if let EventKind::TaskFailed { task: t, reason } = &ev.kind
                && t == task
            {
                Some(format!(
                    "- {}",
                    reason
                        .as_deref()
                        .unwrap_or(fragment_text("failure-no-reason"))
                ))
            } else {
                None
            }
        })
        .collect();
    if reasons.is_empty() {
        return String::new();
    }
    reasons.reverse();
    fragment("failures", &[("reasons", reasons.join("\n"))])
}

/// A conflict fix round's instruction: merge the default branch's commit
/// and resolve the conflict. Empty for any other builder run.
pub fn merge_main(branch: &str, commit: Option<String>) -> String {
    commit.map_or_else(String::new, |c| {
        fragment(
            "merge-main",
            &[("branch", branch.to_owned()), ("commit", c)],
        )
    })
}

/// The task's summary from the plan that proposed it.
pub fn summary_of(events: &[Event], task: &dvb_core::TaskId) -> String {
    events
        .iter()
        .find_map(|ev| {
            if let EventKind::PlanProposed { tasks, .. } = &ev.kind {
                tasks
                    .iter()
                    .find(|t| &t.task == task)
                    .map(|t| t.summary.clone())
            } else {
                None
            }
        })
        .unwrap_or_default()
}

/// The completion conditions the owner waived for the task, which are no
/// longer required of it; empty when there are none.
fn waivers_for(proj: &Projections, task: &dvb_core::TaskId) -> String {
    let waivers = crate::dispatch::completion_waivers(proj, task);
    if waivers.is_empty() {
        return String::new();
    }
    let list: String = waivers.iter().map(|w| format!("- {w}\n")).collect();
    fragment("owner-waivers", &[("waivers", list)])
}

/// The texts the owner (or the orchestrator, with the owner's yes) sent the
/// task back with, oldest first: they are part of the task its reviewers
/// judge. The engine's own retry texts are not.
fn owner_retry_texts(events: &[Event], task: &dvb_core::TaskId) -> String {
    let texts: Vec<&str> = events
        .iter()
        .filter_map(|ev| {
            if let EventKind::TaskRetryTextSet {
                task: t,
                text: Some(text),
            } = &ev.kind
                && t == task
                && ev.actor != dvb_core::Actor::Engine
            {
                Some(text.as_str())
            } else {
                None
            }
        })
        .collect();
    if texts.is_empty() {
        return String::new();
    }
    let list: String = texts
        .iter()
        .enumerate()
        .map(|(i, text)| format!("{}. {}\n\n", i + 1, text.trim()))
        .collect();
    fragment("owner-retry-texts", &[("texts", list)])
}

/// Private, unbounded goal context from the version preceding the task's
/// originating plan. Never use the current goal projection: redirects replace
/// its text, while older tasks and their retries/reviewers keep their own version.
/// Existing events supply the binding, including on replay of earlier releases.
/// Worded plan-approval replies for that version precede the plan and follow
/// the unchanged goal text, oldest first, as the owner's overriding context.
pub fn owner_goal_for(events: &[Event], task: &dvb_core::TaskId) -> String {
    let Some(plan) = events.iter().position(|ev| {
        matches!(&ev.kind, EventKind::PlanProposed { tasks, .. }
            if tasks.iter().any(|t| &t.task == task))
    }) else {
        return String::new();
    };
    let EventKind::PlanProposed { goal, .. } = &events[plan].kind else {
        unreachable!("the originating event is a plan");
    };
    let stated = events[..plan].iter().rev().find_map(|ev| {
        if ev.project == events[plan].project
            && let EventKind::GoalStated { goal: g, text, .. } = &ev.kind
            && g == goal
        {
            Some((ev.id, text.as_str()))
        } else {
            None
        }
    });
    let Some((version, text)) = stated.filter(|(_, text)| !text.is_empty()) else {
        return String::new();
    };
    let mut issues: Vec<dvb_core::event::GoalIssue> = Vec::new();
    for ev in &events[..plan] {
        if ev.project == events[plan].project
            && let EventKind::GoalStated {
                goal: g,
                issues: carried,
                ..
            } = &ev.kind
            && g == goal
        {
            for issue in carried {
                if !issues.iter().any(|i| i.number == issue.number) {
                    issues.push(issue.clone());
                }
            }
        }
    }
    let text = format!("{text}{}", goal_issues(&issues));
    let mut context = fragment("owner-goal", &[("text", text)]);
    let mut replies = Vec::new();
    for (i, ev) in events[..plan].iter().enumerate() {
        if ev.project != events[plan].project {
            continue;
        }
        let EventKind::QuestionAnswered {
            question,
            answer: Answer::Reply(reply),
        } = &ev.kind
        else {
            continue;
        };
        if events[..i].iter().any(|asked| {
            asked.project == ev.project
                && matches!(&asked.kind, EventKind::QuestionOpened {
                    question: q, options, lapses_on, ..
                } if q == question
                    && *lapses_on == dvb_core::event::LapsesOn::GoalVersion(version)
                    && options.iter().any(|o| o.approves_plan))
        }) {
            replies.push(reply.as_str());
        }
    }
    if !replies.is_empty() {
        context.push_str(fragment_text("owner-replies"));
        context.push_str(&replies.join("\n\n"));
        context.push_str("\n\n");
    }
    context
}

/// The `{{notes}}` block stops growing past this many bytes.
const NOTES_CAP: usize = 16 * 1024;
/// How many of the newest notes tied to no goal the orchestrator sees.
const UNTIED_NOTES: usize = 5;

/// The project's notes with the run that wrote each, oldest first, one bullet
/// each; later lines of a multi-line note are indented under its bullet.
fn note_bullets<'a>(
    events: &'a [Event],
    project: &'a dvb_core::ProjectId,
) -> impl Iterator<Item = (&'a dvb_core::RunId, String)> {
    events
        .iter()
        .filter(move |ev| {
            ev.project.as_ref() == Some(project) && matches!(ev.actor, dvb_core::Actor::Run(_))
        })
        .filter_map(|ev| {
            if let EventKind::RunOutput {
                resource: None,
                run,
                item: dvb_core::event::OutputItem::Note(text),
            } = &ev.kind
            {
                Some((run, format!("- {}\n", text.trim().replace('\n', "\n  "))))
            } else {
                None
            }
        })
}

/// Every note of the project, the content of NOTES.md.
pub fn notes_text(events: &[Event], project: &dvb_core::ProjectId) -> String {
    note_bullets(events, project).map(|(_, b)| b).collect()
}

/// The goal of the run that wrote a note: its task's goal, or the goal
/// version an orchestrator wake is bound to.
fn note_goal<'a>(proj: &'a Projections, run: &dvb_core::RunId) -> Option<&'a dvb_core::GoalId> {
    let r = proj.runs.get(run)?;
    match &r.assignment.task {
        Some(task) => proj.tasks.get(task).map(|t| &t.view.goal),
        None => proj.goal_versions.get(&r.goal_version?),
    }
}

/// The orchestrator's `{{notes}}`: the notes of the goals the summary lists,
/// except Done or Dropped goals not bound to this run, and the newest few
/// notes tied to no goal. Past the cap the oldest are left out.
fn prompt_notes(proj: &Projections, events: &[Event], p: &Project, run: &Run) -> String {
    use dvb_core::api::GoalState;
    let listed: Vec<&dvb_core::GoalId> = summary_goals(proj, p, run)
        .into_iter()
        .filter(|g| {
            !matches!(g.state, GoalState::Done | GoalState::Dropped)
                || Some(g.version) == run.goal_version
        })
        .map(|g| &g.id)
        .collect();
    let all: Vec<_> = note_bullets(events, &p.view.id)
        .map(|(r, b)| (note_goal(proj, r), b))
        .collect();
    let untied = all.iter().filter(|(g, _)| g.is_none()).count();
    let mut seen_untied = 0;
    let kept: Vec<&str> = all
        .iter()
        .filter(|(g, _)| match g {
            Some(g) => listed.contains(g),
            None => {
                seen_untied += 1;
                seen_untied + UNTIED_NOTES > untied
            }
        })
        .map(|(_, b)| b.as_str())
        .collect();
    if kept.iter().map(|b| b.len()).sum::<usize>() <= NOTES_CAP {
        return kept.concat();
    }
    let line = |n: usize| fragment("notes-left-out", &[("count", n.to_string())]);
    let mut room = NOTES_CAP - line(kept.len()).len();
    let newest = kept
        .iter()
        .rev()
        .take_while(|b| {
            let fits = b.len() <= room;
            room = room.saturating_sub(b.len());
            fits
        })
        .count();
    let left_out = kept.len() - newest;
    line(left_out) + &kept[left_out..].concat()
}

pub(crate) fn write_notes(
    state: &std::path::Path,
    project: &dvb_core::ProjectId,
    text: &str,
) -> std::io::Result<()> {
    let dir = state.join("projects").join(project.to_string());
    std::fs::create_dir_all(&dir)?;
    dvb_core::io::write_atomic(&dir.join("NOTES.md"), text.as_bytes(), 0o666, false)
}

/// Retry context is fixed at this builder's run start, and survives later runs.
pub fn retry_text_for(events: &[Event], r: &Run) -> String {
    let Some(task) = r.builds() else {
        return String::new();
    };
    let before = events.iter().take_while(|ev| {
        !matches!(
            &ev.kind, EventKind::RunStarted(a) if a.run == r.assignment.run
        )
    });
    let text = before
        .filter_map(|ev| {
            if let EventKind::TaskRetryTextSet { task: t, text } = &ev.kind
                && t == task
            {
                Some(text.as_deref())
            } else {
                None
            }
        })
        .last()
        .flatten();
    text.map(|text| fragment("retry-context", &[("text", text.to_string())]))
        .unwrap_or_default()
}

// Review startup binds the publication, including same-head fix rounds and
// engine branch updates. Later builders must not replace its description.
fn pr_description_for(events: &[Event], r: &Run, task: &dvb_core::TaskId) -> String {
    let before = &events[..events.partition_point(|ev| ev.id < r.started)];
    let summary = (|| {
        let publication = before.iter().rposition(
            |ev| matches!(&ev.kind, EventKind::BranchPublished { task: t, .. } if t == task),
        )?;
        let builder = before[..publication].iter().rev().find_map(|ev| {
            if let EventKind::RunStarted(a) = &ev.kind
                && a.task.as_ref() == Some(task)
                && a.reviews.is_none()
            {
                Some(&a.run)
            } else {
                None
            }
        })?;
        before.iter().rev().find_map(|ev| {
            if let EventKind::RunCollected { run, summary, .. } = &ev.kind
                && run == builder
            {
                summary.as_deref()
            } else {
                None
            }
        })
    })();
    summary
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| fragment("pr-description", &[("summary", s.to_owned())]))
        .unwrap_or_default()
}

/// The variables for this run's prompt.
pub async fn context(
    e: &AppState,
    r: &Run,
    p: &Project,
) -> Result<Vec<(&'static str, String)>, Fail> {
    let events = e
        .store
        .select(EventId(0), dvb_store::Select::WithoutTelemetry)?;
    let settings = e.proj().settings.clone();
    let mut vars = vec![
        (
            "response_style",
            style_instruction(settings.response_style()),
        ),
        ("run_verbs", RUN_VERBS.to_owned()),
        (
            "browser",
            include_str!("../../../prompts/browser.md")
                .trim()
                .to_owned(),
        ),
        (
            "model_pool",
            e.selected_models(&e.proj())
                .iter()
                .map(|m| m.id.as_str())
                .collect::<Vec<_>>()
                .join(", "),
        ),
    ];
    let brief = p.brief.trim();
    if !brief.is_empty()
        && matches!(
            (&r.assignment.task, &r.assignment.reviews),
            (None, _) | (Some(_), None)
        )
    {
        vars.push((
            "project_brief",
            fragment("project-brief", &[("brief", brief.to_string())]),
        ));
    }
    if let Some(task) = &r.assignment.task {
        vars.push(("owner_goal", owner_goal_for(&events, task)));
    }
    match (&r.assignment.task, &r.assignment.reviews) {
        (None, _) => {
            vars.push((
                "question_format",
                serde_json::to_value(settings.question_format())
                    .map_err(Fail::internal)?
                    .as_str()
                    .unwrap_or("quick_compare")
                    .to_owned(),
            ));
            let guidance = if settings.model_mode() == dvb_core::api::ModelMode::Auto {
                MODEL_MODE_AUTO
            } else {
                MODEL_MODE_CUSTOM
            };
            vars.push(("model_guidance", guidance.to_owned()));
            vars.push(("orchestrator_verbs", ORCHESTRATOR_VERBS.to_owned()));
            if e.proj().autonomous(&p.view.id) {
                vars.push(("autonomy", fragment_text("autonomy").to_owned()));
            }
            let mut summary = project_summary(e, p, r);
            summary.push_str(&mentioned_issues(e, p, r).await);
            summary.push_str(&proposal_issues(e, p, r).await);
            vars.push(("project_summary", summary));
            vars.push(("events_since_last_wake", events_since(&events, p, r)));
            vars.push(("pending_reply", pending_replies(&events, p, r)));
            e.sync_notes(&p.view.id)?;
            let notes = prompt_notes(&e.proj(), &events, p, r);
            vars.push(("notes", notes));
        }
        (Some(task), Some(head)) => {
            let mut results = String::from(fragment_text("required-checks"));
            match &p.view.ci {
                dvb_core::event::CiMode::NoCi => results.push_str(fragment_text("no-ci")),
                dvb_core::event::CiMode::Expected(names) => {
                    let forge = e.forge().ok_or_else(|| Fail::internal("no forge"))?;
                    let seen = forge
                        .checks(&dvb_core::forge::Repo(p.view.repo.clone()), head)
                        .await;
                    for name in names {
                        match &seen {
                            Ok(checks) => {
                                let matching: Vec<_> =
                                    checks.iter().filter(|c| &c.name == name).collect();
                                if matching.is_empty() {
                                    results.push_str(&fragment(
                                        "check-missing",
                                        &[("name", name.to_string())],
                                    ));
                                }
                                for check in matching {
                                    let conclusion =
                                        check.conclusion.clone().unwrap_or_else(|| {
                                            format!("{:?}", check.status).to_lowercase()
                                        });
                                    let url = check
                                        .url
                                        .as_deref()
                                        .unwrap_or(fragment_text("check-link-unavailable"));
                                    results.push_str(&fragment(
                                        "check-result",
                                        &[
                                            ("name", name.to_string()),
                                            ("conclusion", conclusion.to_string()),
                                            ("url", url.to_string()),
                                        ],
                                    ));
                                }
                            }
                            Err(error) => results.push_str(&fragment(
                                "check-unreadable",
                                &[("name", name.to_string()), ("error", error.to_string())],
                            )),
                        }
                    }
                }
            }
            vars.push(("check_results", results));
            let summary = summary_of(&events, task)
                + &waivers_for(&e.proj(), task)
                + &owner_retry_texts(&events, task);
            vars.push(("task_summary", summary));
            vars.push(("pr_description", pr_description_for(&events, r, task)));
            vars.push(("head", head.clone()));
            let stage = r.assignment.review_stage.unwrap_or(ReviewKind::Code);
            let base = crate::publish::review_base(e, task, head, stage)
                .await
                .unwrap_or_default();
            vars.push(("base", base));
            vars.push((
                "review_instructions",
                p.role(&r.assignment.role)
                    .map(|role| role.review_instructions.clone())
                    .unwrap_or_default(),
            ));
            vars.push((
                "review_findings",
                findings_to_recheck(&events, task, r.assignment.review_stage),
            ));
        }
        (Some(task), None) => {
            let summary = summary_of(&events, task) + &waivers_for(&e.proj(), task);
            vars.push(("task_summary", summary));
            vars.push(("review_findings", findings_for(&events, task)));
            vars.push(("failures", failures_for(&events, task)));
            vars.push(("retry_text", retry_text_for(&events, r)));
            let merge = crate::publish::merges(&e.mirror(&p.view.name), task.as_str()).await;
            vars.push(("merge_main", merge_main(&p.view.default_branch, merge)));
        }
    }
    Ok(vars)
}

/// Fills `{{name}}` from `vars`; names without a value become empty. In a JSON
/// template the values are escaped as JSON string contents.
pub fn render(template: &str, vars: &[(&str, String)], json: bool) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let Some(len) = rest[start..].find("}}") else {
            rest = &rest[start..];
            break;
        };
        let key = &rest[start + 2..start + len];
        let value = vars
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v.as_str())
            .unwrap_or_default();
        if json {
            let quoted = serde_json::to_string(value).unwrap_or_default();
            out.push_str(&quoted[1..quoted.len() - 1]);
        } else {
            out.push_str(value);
        }
        rest = &rest[start + len + 2..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn render_fills_and_escapes() {
        let vars = [("goal_text", "Say \"hi\"".to_owned())];
        assert_eq!(
            render(r#"{"t": "{{goal_text}}", "u": "{{nope}}"}"#, &vars, true),
            r#"{"t": "Say \"hi\"", "u": ""}"#
        );
        assert_eq!(
            render("Goal: {{goal_text}}", &vars, false),
            "Goal: Say \"hi\""
        );
    }

    #[test]
    fn the_bundled_styles_are_the_four_with_instructions() {
        assert_eq!(
            super::RESPONSE_STYLES.keys().collect::<Vec<_>>(),
            ["clear", "expert", "simple", "technical"]
        );
        assert!(
            super::RESPONSE_STYLES
                .values()
                .all(|text| !text.trim().is_empty())
        );
    }

    use super::*;

    #[test]
    fn the_four_styles_differ_and_an_unknown_one_is_clear() {
        let all: Vec<String> = ["Simple", "Clear", "Technical", "Expert"]
            .iter()
            .map(|s| style_instruction(s))
            .collect();
        for (i, a) in all.iter().enumerate() {
            assert!(!a.is_empty());
            for b in &all[i + 1..] {
                assert_ne!(a, b);
            }
        }
        assert_eq!(style_instruction("loud"), style_instruction("clear"));
        assert_eq!(style_instruction("EXPERT"), style_instruction("expert"));
    }
}
