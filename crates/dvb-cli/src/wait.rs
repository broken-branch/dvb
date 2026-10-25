//! `dvb wait`: block on one goal until its plan is asked, it is Done, one of
//! its tasks fails or stops, or its plan is put on hold. The project's
//! `goal.stated` and `question.opened` events tie questions to the goal's
//! versions; `pr.merged` events give the merge commits.
use crate::client::CliError;
use crate::client::WatchEvent;
use crate::{Client, goal_line, goal_title, option_line, question_json, write_json, write_stdout};
use dvb_core::api::{EventPageQuery, GoalState, GoalView, QuestionView, TaskView};
use dvb_core::event::LapsesOn;
use dvb_core::{EventId, EventKind, GoalId, ProjectId, QuestionId, TaskId, TaskState};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::ops::ControlFlow;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Until {
    /// The goal's plan question is open.
    Plan,
    /// The goal is Done.
    Done,
}

pub const ERROR: i32 = 1;
pub const QUESTION: i32 = 2;
pub const FAILED: i32 = 3;
pub const TIMED_OUT: i32 = 4;
pub const NO_GOAL: i32 = 5;
pub const STOPPED: i32 = 6;

pub const EXIT_CODES: &str = "Exit codes:
  0  --until done: the goal is Done (prints its merged pull requests and merge commits);
     --until plan: its plan question is open (prints the question), or its plan is already approved
  1  dvb could not read the engine (a refusal or an unreadable reply)
  2  a question of this goal waits for you (prints its id, subject and options)
  3  a task of this goal failed (prints the task id, reason and dvb retry <task>)
  4  the timeout elapsed
  5  the goal does not exist or was dropped
  6  a task of this goal is stopped (prints the task id, reason and dvb resume <task>),
     or its plan is on hold (prints dvb resume <goal>)";

const KINDS: &str = "goal.stated,question.opened,pr.merged";

/// What the project's events say about the goal, read once and then followed.
#[derive(Default)]
struct Seen {
    cursor: u64,
    /// The goal's project, learned on the first check.
    project: Option<ProjectId>,
    /// The goal's `goal.stated` events: a question lapsing on one is the goal's.
    versions: BTreeSet<EventId>,
    questions: BTreeMap<QuestionId, EventId>,
    merges: Vec<(TaskId, u64, String)>,
}

impl Seen {
    async fn read(
        &mut self,
        client: &Client,
        goal: &GoalId,
        filter: &EventPageQuery,
    ) -> Result<(), CliError> {
        let (tail, events) = client.filtered_since(self.cursor, filter).await?;
        for event in events {
            self.fold(goal, event);
        }
        self.cursor = tail;
        Ok(())
    }

    fn fold(&mut self, goal: &GoalId, event: WatchEvent) {
        let WatchEvent::Known(event) = event else {
            return;
        };
        if let EventKind::GoalStated { goal: stated, .. } = &event.kind
            && stated == goal
        {
            self.versions.insert(event.id);
        } else if let EventKind::QuestionOpened {
            question,
            lapses_on: LapsesOn::GoalVersion(version),
            ..
        } = event.kind
        {
            self.questions.insert(question, version);
        } else if let EventKind::PrMerged { task, pr, sha } = event.kind {
            self.merges.push((task, pr, sha));
        }
    }

    fn goal_question(&self, q: &QuestionView, tasks: &BTreeSet<&TaskId>) -> bool {
        self.questions
            .get(&q.id)
            .is_some_and(|version| self.versions.contains(version))
            || q.task.as_ref().is_some_and(|task| tasks.contains(task))
    }

    /// The wait's end, once the state holds.
    fn check(
        &self,
        g: &GoalView,
        until: Until,
        tasks: &[TaskView],
        questions: &[QuestionView],
    ) -> Option<End> {
        let end = |code, outcome, text: String, more: serde_json::Value| {
            let mut json = json!({"outcome": outcome, "goal": g.id, "state": g.state});
            if let (Some(json), serde_json::Value::Object(more)) = (json.as_object_mut(), more) {
                json.extend(more);
            }
            End { code, text, json }
        };
        if g.state == GoalState::Dropped {
            return Some(end(
                NO_GOAL,
                "dropped",
                format!("{}\n", goal_line(g)),
                json!({}),
            ));
        }
        let tasks: Vec<_> = tasks.iter().filter(|t| t.goal == g.id).collect();
        let ids: BTreeSet<_> = tasks.iter().map(|t| &t.id).collect();
        let mut questions = questions
            .iter()
            .filter(|q| q.open && q.project == g.project && self.goal_question(q, &ids));
        match until {
            Until::Done if g.state == GoalState::Done => {
                let mut text = format!("Goal {}: Done\n", g.id);
                let mut merged = Vec::new();
                for (task, pr, sha) in self.merges.iter().filter(|(t, ..)| ids.contains(t)) {
                    text.push_str(&format!("Merged PR #{pr} as {sha} (task {task})\n"));
                    merged.push(json!({"task": task, "pr": pr, "commit": sha}));
                }
                return Some(end(0, "done", text, json!({"merged": merged})));
            }
            Until::Plan => {
                if let Some(q) = questions
                    .clone()
                    .find(|q| q.options.iter().any(|o| o.approves_plan))
                {
                    let more = json!({"question": question_json(q)});
                    return Some(end(0, "plan_question", question_text(q), more));
                }
                if g.state != GoalState::Stated {
                    let text = format!("{}\n", goal_line(g));
                    return Some(end(0, "plan_approved", text, json!({})));
                }
            }
            Until::Done => {}
        }
        let task = |t: &TaskView, reason: Option<&str>, verb: &str| {
            json!({"task": {
                "id": t.id,
                "title": t.title,
                "reason": reason,
                "command": format!("dvb {verb} {}", t.id),
            }})
        };
        #[expect(
            clippy::wildcard_enum_match_arm,
            reason = "only a failure ends the wait here; every other state, present or future, waits on"
        )]
        let failed = tasks.iter().find_map(|t| match &t.state {
            TaskState::Failed { reason } if !t.problem_superseded => Some((t, reason)),
            _ => None,
        });
        if let Some((t, reason)) = failed {
            let text = format!(
                "Task {} failed: {reason}\nRetry it: dvb retry {}\n",
                t.id, t.id
            );
            return Some(end(FAILED, "failed", text, task(t, Some(reason), "retry")));
        }
        if let Some(q) = questions.next() {
            let more = json!({"question": question_json(q)});
            return Some(end(QUESTION, "question", question_text(q), more));
        }
        if let Some(t) = tasks
            .iter()
            .find(|t| matches!(t.state, TaskState::Stopped { .. }) && !t.problem_superseded)
        {
            let reason = t.reason.as_deref();
            let text = format!(
                "Task {} stopped{}\nResume it: dvb resume {}\n",
                t.id,
                reason.map(|r| format!(": {r}")).unwrap_or_default(),
                t.id
            );
            return Some(end(STOPPED, "stopped", text, task(t, reason, "resume")));
        }
        g.plan_parked.then(|| {
            let text = format!(
                "Goal {}: {} - {}\nAsk it again: dvb resume {}\n",
                g.id,
                goal_title(g),
                dvb_core::wording::text("goal.plan_parked"),
                g.id
            );
            let more = json!({"command": format!("dvb resume {}", g.id)});
            end(STOPPED, "plan_parked", text, more)
        })
    }
}

/// Where the wait ended: its exit code, its lines, and its `--json` object;
/// with no object (null), the goal was not found and the line is an error.
#[derive(Debug)]
struct End {
    code: i32,
    text: String,
    json: serde_json::Value,
}

fn question_text(q: &QuestionView) -> String {
    let mut out = format!("Question {}: {}\n", q.id, q.subject);
    for o in &q.options {
        out.push_str(&option_line(q, o));
        out.push('\n');
    }
    out
}

/// Exit code for `dvb wait`; `timeout` in minutes, 0 checking only once.
pub async fn wait(
    client: &Client,
    goal: &GoalId,
    until: Until,
    timeout: Option<u64>,
    json: bool,
) -> Result<i32, CliError> {
    let mut seen = Seen::default();
    let end = match timeout {
        Some(0) => step(client, goal, until, &mut seen).await?,
        Some(minutes) => tokio::time::timeout(
            std::time::Duration::from_secs(minutes.saturating_mul(60)),
            follow(client, goal, until, &mut seen),
        )
        .await
        .ok()
        .transpose()?,
        None => Some(follow(client, goal, until, &mut seen).await?),
    };
    let Some(end) = end else {
        eprintln!(
            "dvb wait: timed out after {} minutes",
            timeout.unwrap_or_default()
        );
        return Ok(TIMED_OUT);
    };
    if end.json.is_null() {
        eprint!("{}", end.text);
    } else if json {
        write_json(&end.json)?;
    } else {
        write_stdout(format_args!("{}", end.text))?;
    }
    Ok(end.code)
}

/// Like `dvb watch`, reconnects when the engine restarts; any event wakes a new check.
async fn follow(
    client: &Client,
    goal: &GoalId,
    until: Until,
    seen: &mut Seen,
) -> Result<End, CliError> {
    loop {
        let pass = step(client, goal, until, seen).await.map(|end| match end {
            Some(end) => ControlFlow::Break(end),
            None => ControlFlow::Continue(seen.cursor),
        });
        if let Some(end) = client.reconnecting(false, "dvb wait", pass).await? {
            return Ok(end);
        }
    }
}

/// Reads the events since the last check, then decides from the current views.
/// The goal, tasks and questions are read after the tail is fixed, so a change
/// they miss has an event after the tail that wakes the next check.
async fn step(
    client: &Client,
    goal: &GoalId,
    until: Until,
    seen: &mut Seen,
) -> Result<Option<End>, CliError> {
    let project = match &seen.project {
        Some(project) => project.clone(),
        None => match goal_view(client, goal).await? {
            Some(g) => seen.project.insert(g.project).clone(),
            None => return Ok(Some(not_found(goal))),
        },
    };
    let filter = EventPageQuery {
        project: Some(project.clone()),
        kinds: Some(KINDS.into()),
        ..Default::default()
    };
    seen.read(client, goal, &filter).await?;
    let Some(g) = goal_view(client, goal).await? else {
        return Ok(Some(not_found(goal)));
    };
    if g.state == GoalState::Done {
        // Its merges may have come after the tail.
        seen.read(client, goal, &filter).await?;
    }
    let tasks: Vec<TaskView> = client.get(&format!("/tasks?project={project}")).await?;
    let questions: Vec<QuestionView> = client.get("/questions?open=true").await?;
    Ok(seen.check(&g, until, &tasks, &questions))
}

async fn goal_view(client: &Client, goal: &GoalId) -> Result<Option<GoalView>, CliError> {
    let goals: Vec<GoalView> = client.get("/goals").await?;
    Ok(goals.into_iter().find(|g| &g.id == goal))
}

fn not_found(goal: &GoalId) -> End {
    End {
        code: NO_GOAL,
        text: format!("dvb: refused: NOT_FOUND (no goal {goal})\n"),
        json: serde_json::Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_http;
    use serde_json::json;

    #[tokio::test(start_paused = true)]
    async fn a_goal_done_just_before_the_tail_is_read_ends_the_wait() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("engine.sock");
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        let mut requests = test_http::requests(listener);
        tokio::spawn(async move {
            let (mut goals, mut pages) = (0, 0);
            let mut idle = Vec::new();
            while let Some((request, response)) = requests.recv().await {
                let body = match request.uri().path() {
                    // The goal finishes after the first read: its events lie
                    // before the tail, so none comes after it to wake a check.
                    "/goals" => {
                        goals += 1;
                        let state = if goals == 1 { "in_progress" } else { "done" };
                        json!([{"id": "g", "project": "p", "text": "Work", "state": state}])
                    }
                    "/events/recent" => {
                        pages += 1;
                        let events = if pages == 1 {
                            json!([{
                                "id": 3, "at": 0, "actor": "engine", "project": "p",
                                "kind": "pr.merged", "payload": {"task": "t", "pr": 7, "sha": "abc"}
                            }])
                        } else {
                            json!([])
                        };
                        json!({"cursor": 3, "events": events, "before": null, "activity": []})
                    }
                    "/tasks" => json!([{
                        "id": "t", "project": "p", "goal": "g", "title": "Build",
                        "role": "backend", "review": "code",
                        "state": serde_json::to_value(TaskState::Done).unwrap(),
                        "pr": 7, "head": "abc"
                    }]),
                    "/questions" => json!([]),
                    "/events" => {
                        idle.push(response);
                        continue;
                    }
                    path => panic!("unexpected path: {path}"),
                };
                response.send(test_http::json(200, body)).unwrap();
            }
        });
        let client = Client::new(sock);
        let mut seen = Seen::default();
        let end = step(&client, &GoalId::new("g"), Until::Done, &mut seen)
            .await
            .unwrap();
        let end = end.unwrap();
        assert_eq!(
            (end.code, end.text.as_str()),
            (0, "Goal g: Done\nMerged PR #7 as abc (task t)\n")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn minutes_without_a_change_end_in_the_timeout_code() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("engine.sock");
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        let mut requests = test_http::requests(listener);
        tokio::spawn(async move {
            let mut idle = Vec::new();
            while let Some((request, response)) = requests.recv().await {
                let body = match request.uri().path() {
                    "/goals" => json!([{
                        "id": "g", "project": "p", "text": "Work", "state": "in_progress"
                    }]),
                    "/events/recent" => {
                        json!({"cursor": 3, "events": [], "before": null, "activity": []})
                    }
                    "/tasks" | "/questions" => json!([]),
                    // The event stream never delivers anything.
                    "/events" => {
                        idle.push(response);
                        continue;
                    }
                    path => panic!("unexpected path: {path}"),
                };
                response.send(test_http::json(200, body)).unwrap();
            }
        });
        let code = wait(
            &Client::new(sock),
            &GoalId::new("g"),
            Until::Done,
            Some(2),
            false,
        )
        .await
        .unwrap();
        assert_eq!(code, TIMED_OUT);
    }
}
