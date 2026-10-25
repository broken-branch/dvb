//! Result-bound completion holds. Worker trees remain available while stopped.
use super::{
    Next,
    effects::pr_body,
    runs::{builder_assignment, launch_run, preserved_workspace},
};
use crate::{
    AppState, Fail,
    proj::{Project, Projections, Task},
};
use dvb_core::event::{Answer, LapsesOn, QuestionFormat, QuestionKind, QuestionOption};
use dvb_core::forge::Repo;
use dvb_core::git::Git;
use dvb_core::run_verbs::{BuilderOutcome, PartialReason};
use dvb_core::{Actor, Command, EventId, EventKind, QuestionId, RunId};

pub(crate) fn result(proj: &Projections, t: &Task) -> Option<(RunId, EventId)> {
    let r = proj.runs.get(t.facts.builder.as_ref()?)?;
    Some((r.assignment.run.clone(), r.outcome_at.unwrap_or(r.started)))
}

fn question_id(run: &RunId, result: EventId) -> QuestionId {
    QuestionId::new(format!("completion-{run}-{result}"))
}

/// The current result's unmet completion condition is waived: the owner chose
/// "Merge anyway" for it, or for an earlier result of the task that reported
/// the same condition (the same detail, whitespace aside).
pub(crate) fn waived(proj: &Projections, t: &Task) -> bool {
    let BuilderOutcome::Partial {
        reason: PartialReason::CompletionUnmet,
        detail,
    } = &t.view.outcome
    else {
        return false;
    };
    result(proj, t)
        .and_then(|(run, at)| proj.questions.get(&question_id(&run, at)))
        .is_some_and(|q| q.view.answer == Some(Answer::Choice(1)))
        || waivers(proj, &t.view.id)
            .iter()
            .any(|waived| waived.split_whitespace().eq(detail.split_whitespace()))
}

/// The unmet completion conditions the owner waived for the task ("Merge
/// anyway"), in the order they were waived, each once. They stay with the
/// task for its later rounds and go into its reviewers' task text.
pub(crate) fn waivers(proj: &Projections, task: &dvb_core::TaskId) -> Vec<String> {
    let mut answered: Vec<_> = proj
        .questions
        .values()
        .filter_map(|q| match &q.kind {
            QuestionKind::Completion { task: t, run, .. }
                if t == task && q.view.answer == Some(Answer::Choice(1)) =>
            {
                let detail = proj.runs.get(run)?.outcome.reason()?;
                Some((q.answered_at, detail.to_owned()))
            }
            QuestionKind::Completion { .. }
            | QuestionKind::Ordinary
            | QuestionKind::OrchestratorFailed
            | QuestionKind::PlanningHold { .. } => None,
        })
        .collect();
    answered.sort();
    let mut details: Vec<String> = Vec::new();
    for (_, detail) in answered {
        if !details.contains(&detail) {
            details.push(detail);
        }
    }
    details
}

/// Called at both effect boundaries, including after restart/reconcile.
pub(super) async fn hold(e: &AppState, t: &Task, p: &Project) -> Result<bool, Fail> {
    if t.view.outcome == BuilderOutcome::Complete {
        return Ok(false);
    }
    // Replace an existing PR's closing language even when this result cannot
    // publish another round. Retrying the effect is safe after a lost reply.
    if let Some(pr) = t.view.pr {
        let (run, summary) = {
            let proj = e.proj();
            let r = t.facts.builder.as_ref().and_then(|id| proj.runs.get(id));
            (
                r.map(|r| r.assignment.run.clone())
                    .unwrap_or_else(|| RunId::new("")),
                r.and_then(|r| r.summary.clone()),
            )
        };
        let issues = super::effects::goal_issue_refs(&e.proj(), &t.view.id);
        let body = references_only(&pr_body(
            &t.view.id,
            t.facts.round,
            &run,
            summary.as_deref(),
            &issues,
        ));
        if let Some(hit) = e.publication().policy().scan("pr body", &body) {
            super::effects::blocked(
                e,
                &t.view.id,
                Some(&p.view.id),
                dvb_core::state::Effect::OpenPr,
                hit,
            )?;
            return Ok(true);
        }
        e.forge()
            .ok_or_else(|| Fail::internal("no forge"))?
            .edit_pr_body(&Repo(p.view.repo.clone()), pr, &body)
            .await
            .map_err(Fail::internal)?;
    }
    if waived(&e.proj(), t) {
        return Ok(false);
    }
    let reason = t
        .view
        .outcome
        .reason()
        .unwrap_or("Incomplete work")
        .to_owned();
    let question = completion_question(&e.proj(), t)?;
    e.decide(
        e.proj(),
        &t.view.id,
        Actor::Engine,
        crate::Decision {
            question,
            ..crate::Decision::new("completion_hold", Command::Stop(Some(reason)))
        },
        |_| Ok(()),
    )?;
    crate::wake::wake_if_missed(e, &p.view.id);
    Ok(true)
}

fn completion_question(proj: &Projections, t: &Task) -> Result<Option<EventKind>, Fail> {
    let reason = t.view.outcome.reason().unwrap_or_default();
    if matches!(
        t.view.outcome,
        BuilderOutcome::Partial {
            reason: PartialReason::CompletionUnmet,
            ..
        }
    ) {
        let (run, result) =
            result(proj, t).ok_or_else(|| Fail::internal("missing builder result"))?;
        let id = question_id(&run, result);
        if proj.questions.contains_key(&id) {
            Ok(None)
        } else {
            Ok(Some(EventKind::QuestionOpened {
                kind: Some(QuestionKind::Completion {
                    task: t.view.id.clone(),
                    run,
                    result,
                }),
                main_ci: None,
                task: Some(t.view.id.clone()),
                question: id,
                subject: dvb_core::wording::render(
                    "question.completion-subject",
                    &serde_json::json!({"title":t.view.title,"reason":reason}),
                ),
                format: QuestionFormat::QuickCompare,
                options: vec![
                    QuestionOption::owner(
                        1,
                        dvb_core::wording::text("question.completion-merge"),
                        dvb_core::wording::text("question.completion-merge-effect"),
                    ),
                    QuestionOption::owner(
                        2,
                        dvb_core::wording::text("question.completion-retry"),
                        dvb_core::wording::text("question.completion-retry-effect"),
                    ),
                    QuestionOption::owner(
                        3,
                        dvb_core::wording::text("question.completion-stop"),
                        dvb_core::wording::text("question.completion-stop-effect"),
                    ),
                ],
                recommended: 2,
                lapses_on: LapsesOn::GoalVersion(proj.goals[&t.view.goal].version),
            }))
        }
    } else {
        Ok(None)
    }
}

/// The durable answer is the intent. A crash before its effect is replayed
/// here; starting the successor atomically replaces the result identity.
pub(super) async fn stopped(e: &AppState, t: &Task, p: &Project) -> Result<Next, Fail> {
    // A concurrent owner stop can precede the boundary's stop/question batch.
    // Recover the same deterministic question without another state transition.
    e.append_checked(Some(&p.view.id), Actor::Engine, |proj, _| {
        let current = proj
            .tasks
            .get(&t.view.id)
            .ok_or_else(|| Fail::NotFound(format!("task {}", t.view.id)))?;
        Ok(completion_question(proj, current)?.into_iter().collect())
    })?;
    if super::control::live_run(e, t).is_some() {
        return Ok(Next::Idle);
    }
    let blocker = e
        .proj()
        .merged_blocker(t)
        .map(|b| (b.view.id.clone(), b.view.title.clone()));
    if let Some((id, title)) = blocker {
        return prerequisite_merged(e, t, p, &id, &title).await;
    }
    let answer = {
        let proj = e.proj();
        result(&proj, t)
            .and_then(|(run, at)| proj.questions.get(&question_id(&run, at)))
            .filter(|q| {
                q.answered_at
                    .is_some_and(|at| t.problem_at.is_none_or(|stop| at > stop))
            })
            .and_then(|q| q.view.answer.clone())
    };
    match answer {
        Some(Answer::Choice(1)) if waived(&e.proj(), t) => {
            e.decide(
                e.proj(),
                &t.view.id,
                Actor::Engine,
                crate::Decision::new("completion_override", Command::Resume(None)),
                |_| Ok(()),
            )?;
        }
        Some(Answer::Choice(2)) => {
            let reason = format!(
                "Retry the completion check: {}",
                t.view.outcome.reason().unwrap_or_default()
            );
            let a = builder_assignment(e, p, t, reason).await?;
            let from = preserved_workspace(e, &t.view.id, &a.base);
            e.decide(
                e.proj(),
                &t.view.id,
                Actor::Engine,
                crate::Decision {
                    pre: vec![EventKind::TaskRetryTextSet {
                        task: t.view.id.clone(),
                        text: Some(a.reason.clone()),
                    }],
                    ..crate::Decision::new("completion_recheck", Command::Resume(Some(a.clone())))
                },
                |_| Ok(()),
            )?;
            if e.proj().runs.contains_key(&a.run) {
                launch_run(e, &a, from);
            }
        }
        Some(Answer::Choice(_)) | Some(Answer::Reply(_)) | None => {}
    }
    Ok(Next::Idle)
}

/// The task a `blocked` result named has merged since: a new builder carries
/// on in the kept workspace and merges the default branch's commit fetched
/// now, published as a merge with it (the conflict round's pinned commit).
async fn prerequisite_merged(
    e: &AppState,
    t: &Task,
    p: &Project,
    blocker: &dvb_core::TaskId,
    title: &str,
) -> Result<Next, Fail> {
    let forge = e.forge().ok_or_else(|| Fail::internal("no forge"))?;
    let mirror = e.mirror(&p.view.name);
    let main = e
        .fetch(
            forge.as_ref(),
            &Repo(p.view.repo.clone()),
            &mirror,
            &p.view.default_branch,
        )
        .await
        .map_err(Fail::internal)?;
    Git::new(&mirror)
        .run(
            &[
                "update-ref",
                &crate::publish::merges_ref(t.view.id.as_str()),
                &main,
            ],
            &[],
        )
        .await
        .map_err(Fail::internal)?;
    let reason = format!(
        "Task {title} ({blocker}), which this task was blocked on, has merged into {}. Bring it into your kept work and finish the task.",
        p.view.default_branch
    );
    let a = builder_assignment(e, p, t, reason).await?;
    let from = preserved_workspace(e, &t.view.id, &a.base);
    e.decide(
        e.proj(),
        &t.view.id,
        Actor::Engine,
        crate::Decision {
            pre: vec![EventKind::TaskRetryTextSet {
                task: t.view.id.clone(),
                text: Some(a.reason.clone()),
            }],
            ..crate::Decision::new("prerequisite_merged", Command::Resume(Some(a.clone())))
        },
        |_| Ok(()),
    )?;
    if e.proj().runs.contains_key(&a.run) {
        launch_run(e, &a, from);
    }
    Ok(Next::Idle)
}

/// Disable every GitHub closing keyword, including cross-repository/URL
/// references and mixed casing. Used only for incomplete publication text.
pub(super) fn references_only(text: &str) -> String {
    let mut offset = 0;
    text.split_inclusive(|c: char| !c.is_ascii_alphabetic())
        .map(|part| {
            let start = offset;
            offset += part.len();
            let word = part.trim_end_matches(|c: char| !c.is_ascii_alphabetic());
            let closing = matches!(
                word.to_ascii_lowercase().as_str(),
                "close"
                    | "closes"
                    | "closed"
                    | "fix"
                    | "fixes"
                    | "fixed"
                    | "resolve"
                    | "resolves"
                    | "resolved"
            );
            let reference = text[start + word.len()..]
                .trim_start_matches(|c: char| {
                    c.is_whitespace() || matches!(c, ':' | '*' | '_' | '`')
                })
                .split_whitespace()
                .next()
                .unwrap_or_default();
            let number = reference
                .rsplit_once('#')
                .map(|(_, number)| number)
                .or_else(|| {
                    (reference.starts_with("https://") || reference.starts_with("http://"))
                        .then(|| reference.rsplit_once("/issues/").map(|(_, number)| number))
                        .flatten()
                });
            if closing
                && number.is_some_and(|n| n.as_bytes().first().is_some_and(u8::is_ascii_digit))
            {
                format!("Refs{}", &part[word.len()..])
            } else {
                part.to_owned()
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::references_only;

    #[test]
    fn incomplete_text_rewrites_closing_references_without_corrupting_prose_or_repository_urls() {
        let text = "Fixed the parser. Closes #323; FIXES org/repo#283; **Resolves** https://github.com/org/fixes/issues/324.";
        assert_eq!(
            references_only(text),
            "Fixed the parser. Refs #323; Refs org/repo#283; **Refs** https://github.com/org/fixes/issues/324."
        );
    }
}
