use super::{orchestrator, planning::run_goal};
use crate::AppState;
use crate::engine::Fail;
use crate::serve::Caller;
use axum::Json;
use axum::extract::State;
use dvb_core::QuestionId;
use dvb_core::event::{Answer, LapsesOn, QuestionOption};
use dvb_core::run_verbs::{AskBody, AskReply};
use dvb_core::{Actor, EventKind};

pub async fn ask(
    State(e): State<AppState>,
    c: Caller,
    Json(body): Json<AskBody>,
) -> Result<Json<AskReply>, Fail> {
    orchestrator(&e, &c, false)?;
    let project = c.run.project.clone();
    let issues = body.issues;
    let format = body.format.unwrap_or(e.proj().settings.question_format());
    let question = QuestionId::new(e.new_id());
    let options = body
        .options
        .into_iter()
        .zip(1u32..)
        .map(|(o, id)| QuestionOption {
            id,
            text: o.text,
            effect: o.effect,
            approves_plan: o.approves_plan,
            no_effect: o.no_effect,
            retries_task: o.retries_task,
        })
        .collect::<Vec<_>>();
    // Checked and appended in one transaction (a redirect cannot come between).
    e.append_checked(
        Some(&project),
        Actor::Run(c.run.assignment.run.clone()),
        |proj, _now| {
            // A no-effect answer wakes no one to approve or retry anything.
            if options
                .iter()
                .any(|o| o.no_effect && (o.approves_plan || o.retries_task.is_some()))
            {
                return Err(dvb_core::Refusal::new(
                    dvb_core::RefusalCode::InvalidValue,
                    "a no_effect option cannot approve the plan or retry a task",
                )
                .into());
            }
            let goal = run_goal(proj, &c)?;
            if goal.state == dvb_core::api::GoalState::Done
                && let Some(current) = proj.goal_under_way(&project)
            {
                return Err(dvb_core::Refusal::new(
                    dvb_core::RefusalCode::ValueChanged,
                    format!(
                        "goal {} is stated or under way; leave it for its own run and ask nothing",
                        current.id
                    ),
                )
                .into());
            }
            let version = goal.version;
            let mut subject = body.subject;
            if options.iter().any(|o| o.approves_plan) {
                subject.push_str(&proj.plan_wording(goal));
            }
            for task in options.iter().filter_map(|o| o.retries_task.as_ref()) {
                if !proj
                    .tasks
                    .get(task)
                    .is_some_and(|t| t.view.project == project)
                {
                    return Err(Fail::NotFound(format!("task {task}")));
                }
            }
            Ok(vec![EventKind::QuestionOpened {
                kind: Some(dvb_core::event::QuestionKind::Ordinary),
                main_ci: None,
                task: options
                    .iter()
                    .find_map(|option| option.retries_task.clone()),
                question: question.clone(),
                subject,
                // An explicit run format overrides the owner's default for this question.
                format,
                options,
                recommended: body.recommended,
                lapses_on: LapsesOn::GoalVersion(version),
            }])
        },
    )?;
    if takes_recommended(&e, &c, &question, issues).await {
        let recommended = body.recommended;
        // Refused (the owner answered first, or the goal waits for another):
        // the refusal is recorded and the question stays the owner's.
        if let Err(error) = crate::api::questions::answer_as(
            &e,
            &question,
            Answer::Choice(recommended),
            Actor::Engine,
        ) {
            eprintln!("dvb engine: taking the recommended option of {question}: {error}");
        }
    }
    Ok(Json(AskReply { question }))
}

/// In an autonomous project dvb takes the recommended option itself of a
/// plan question that it approves, and of a what-next proposal (asked by a
/// run bound to a Done goal) built only from trusted issues: those `ask`
/// names, and those the proposed goal carries. A proposal naming no issue,
/// and every other question, is the owner's.
async fn takes_recommended(
    e: &AppState,
    c: &Caller,
    question: &QuestionId,
    mut issues: Vec<u64>,
) -> bool {
    {
        let proj = e.proj();
        let Some(q) = proj.questions.get(question) else {
            return false;
        };
        let Some(option) = q.view.options.iter().find(|o| o.id == q.view.recommended) else {
            return false;
        };
        if !proj.autonomous(&c.run.project)
            || option.no_effect
            || option.retries_task.is_some()
            || option.approves_plan != q.plan_question
        {
            return false;
        }
        let proposal = proj
            .runs
            .get(&c.run.assignment.run)
            .and_then(|r| r.delivered_goal_version)
            .and_then(|v| proj.goal_versions.get(&v))
            .and_then(|id| proj.goals.get(id))
            .is_some_and(|g| g.state == dvb_core::api::GoalState::Done);
        if !proposal {
            return q.plan_question;
        }
        if q.plan_question
            && let LapsesOn::GoalVersion(v) = q.lapses_on
            && let Some(g) = proj.goal_versions.get(&v).and_then(|id| proj.goals.get(id))
        {
            issues.extend(g.issues());
        }
    }
    issues.sort_unstable();
    issues.dedup();
    if issues.is_empty() {
        return false;
    }
    let repo = match crate::issues::repo_of(e, &c.run.project) {
        Ok(repo) => repo,
        Err(error) => {
            eprintln!("dvb engine: reading the proposal's issues: {error}");
            return false;
        }
    };
    for number in issues {
        match crate::issues::read_trusted(e, &repo, number, crate::issues::Reader::Run).await {
            Ok(_) => {}
            // Untrusted, or no such issue: the owner decides.
            Err(Fail::Refused(_) | Fail::NotFound(_)) => return false,
            Err(error @ Fail::Internal(_)) => {
                eprintln!("dvb engine: reading issue #{number} of the proposal: {error}");
                return false;
            }
        }
    }
    true
}
