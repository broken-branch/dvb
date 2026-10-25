use super::Projections;
use dvb_core::api::QuestionView;
use dvb_core::event::{LapsesOn, QuestionKind};
use dvb_core::{Actor, Event, EventId, EventKind, ProjectId, RunId, TaskId};

#[derive(Debug, Clone)]
pub struct Question {
    pub kind: QuestionKind,
    pub failure_task: Option<TaskId>,
    pub main_ci: Option<Vec<dvb_core::forge::CheckObservation>>,
    pub view: QuestionView,
    /// Distinguishes approval questions for successive plans of one version.
    pub opened_at: EventId,
    /// The id of the event that answered it.
    pub answered_at: Option<EventId>,
    pub lapses_on: LapsesOn,
    /// Engine planning question, tied to one empty attempt as well as its version.
    pub planning_run: Option<RunId>,
    /// Derived from the opening event actor and its durable run binding.
    pub orchestrator: bool,
    /// The run that opened it, when a run did.
    pub asked_by: Option<RunId>,
    /// Asks for a yes to the saved, unapproved plan of a stated goal.
    pub plan_question: bool,
}

impl Question {
    /// Answered with an option marked `no_effect`: there is nothing to act on.
    pub fn no_effect(&self) -> bool {
        matches!(self.view.answer, Some(dvb_core::event::Answer::Choice(n))
            if self.view.options.iter().any(|o| o.id == n && o.no_effect))
    }

    /// A plan question answered with an option that does not approve the plan
    /// ("Not yet"): the plan is parked, and nothing wakes for the answer. Only
    /// a written reply asks the orchestrator for a different plan.
    pub fn parks_plan(&self) -> bool {
        matches!(self.view.answer, Some(dvb_core::event::Answer::Choice(n))
        if self.plan_question
            && self.view.options.iter().any(|o| {
                o.id == n && !o.approves_plan && o.retries_task.is_none()
            }))
    }
}

pub(crate) const PLAN_AGAIN: u32 = 1;
pub(crate) const DROP_GOAL: u32 = 2;
impl Projections {
    /// a question lapses when its goal is stated again (a new version).
    pub(super) fn close_lapsed_questions(&mut self, project: &ProjectId) {
        let versions: Vec<EventId> = self
            .goals
            .values()
            .filter(|g| &g.project == project)
            .map(|g| g.version)
            .collect();
        for q in self
            .questions
            .values_mut()
            .filter(|q| &q.view.project == project)
        {
            match q.lapses_on {
                LapsesOn::GoalVersion(v) if !versions.contains(&v) => q.view.open = false,
                LapsesOn::GoalVersion(_) | LapsesOn::ValueVersion(_) => {}
            }
        }
    }
    #[expect(
        clippy::wildcard_enum_match_arm,
        reason = "only the kinds this fold reads have an arm"
    )]
    pub(super) fn fold_questions(
        &mut self,
        e: &Event,
        previous_problem: Option<&(EventId, ProjectId, TaskId)>,
    ) {
        let project = e.project.clone();
        match &e.kind {
            EventKind::QuestionOpened {
                kind,
                task,
                question,
                subject,
                format,
                options,
                recommended,
                lapses_on,
                main_ci,
            } => {
                if let Some(project) = project {
                    let historical = kind.is_none();
                    let kind = kind.clone().unwrap_or_else(|| {
                        if question.as_str().starts_with("planning-") {
                            let attempt = match lapses_on {
                                LapsesOn::GoalVersion(version) => question
                                    .as_str()
                                    .strip_prefix(&format!("planning-{version}-"))
                                    .map(RunId::new),
                                LapsesOn::ValueVersion(_) => None,
                            };
                            let attempt = attempt
                                .or_else(|| {
                                    self.goals
                                        .values()
                                        .find(|g| {
                                            Self::planning_question(g).as_ref() == Some(question)
                                        })
                                        .and_then(|g| g.planning_run.clone())
                                })
                                .unwrap_or_else(|| RunId::new(question.as_str()));
                            let fetch = attempt.as_str().starts_with("fetch-");
                            QuestionKind::PlanningHold { attempt, fetch }
                        } else if subject.starts_with("The orchestrator stopped") {
                            QuestionKind::OrchestratorFailed
                        } else {
                            QuestionKind::Ordinary
                        }
                    });
                    if e.actor == Actor::Engine
                        && let QuestionKind::PlanningHold {
                            attempt,
                            fetch: true,
                        } = &kind
                        && let LapsesOn::GoalVersion(version) = lapses_on
                        && let Some(g) = self
                            .goals
                            .values_mut()
                            .find(|g| g.project == project && g.version == *version)
                    {
                        g.planning_run = Some(attempt.clone());
                        g.planning_held = true;
                    }
                    let failure_task = task
                        .clone()
                        .or_else(|| options.iter().find_map(|o| o.retries_task.clone()))
                        .or_else(|| {
                            if e.actor != Actor::Engine
                                || options.iter().any(|o| o.approves_plan)
                                || if historical {
                                    self.goals.values().any(|g| {
                                        Self::planning_question(g).as_ref() == Some(question)
                                    })
                                } else {
                                    matches!(kind, QuestionKind::PlanningHold { .. })
                                }
                            {
                                return None;
                            }
                            previous_problem.as_ref().and_then(|(_, _, task)| {
                                let task = self.tasks.get(task)?;
                                let goal = self.goals.get(&task.view.goal)?;
                                (*lapses_on == LapsesOn::GoalVersion(goal.version))
                                    .then(|| task.view.id.clone())
                            })
                        });
                    let plan_question = options.iter().any(|o| o.approves_plan)
                        && self.goals.values().any(|g| {
                            *lapses_on == LapsesOn::GoalVersion(g.version)
                                && self.plan_unapproved(g)
                        });
                    let lapses_with = self
                        .goals
                        .values()
                        .find(|g| *lapses_on == LapsesOn::GoalVersion(g.version));
                    let plan = if options.iter().any(|o| o.approves_plan) {
                        lapses_with.map(|g| self.plan_tasks(g)).unwrap_or_default()
                    } else {
                        vec![]
                    };
                    let goal = failure_task
                        .as_ref()
                        .and_then(|t| self.tasks.get(t))
                        .map(|t| t.view.goal.clone())
                        .or_else(|| lapses_with.map(|g| g.id.clone()));
                    self.questions.insert(
                        question.clone(),
                        Question {
                            main_ci: main_ci.clone(),
                            failure_task: failure_task.clone(),
                            opened_at: e.id,
                            planning_run: if historical {
                                self.goals.values()
                                    .find(|g| Self::planning_question(g).as_ref() == Some(question))
                                    .and_then(|g| g.planning_run.clone())
                            } else { match &kind {
                                QuestionKind::PlanningHold { attempt, .. } => Some(attempt.clone()),
                                QuestionKind::Ordinary | QuestionKind::OrchestratorFailed | QuestionKind::Completion { .. } => None,
                            } },
                            kind: kind.clone(),
                            orchestrator: matches!(&e.actor, Actor::Run(run)
                                if self.runs.get(run).is_some_and(|r| {
                                    r.assignment.is_orchestrator()
                                        && r.goal_version.is_some_and(|v| *lapses_on == LapsesOn::GoalVersion(v))
                                })),
                            asked_by: match &e.actor {
                                Actor::Run(run) => Some(run.clone()),
                                Actor::Owner | Actor::Engine => None,
                            },
                            answered_at: None,
                            plan_question,
                            view: QuestionView {
                                kind: Some(kind),
                                plan,
                                task: failure_task,
                                goal,
                                opened_at: Some(e.at),
                                id: question.clone(),
                                project: project.clone(),
                                subject: subject.clone(),
                                format: *format,
                                options: options.clone(),
                                recommended: *recommended,
                                open: true,
                                answer: None,
                            },
                            lapses_on: lapses_on.clone(),
                        },
                    );
                    self.close_lapsed_questions(&project);
                }
            }
            EventKind::QuestionClosed { question, .. } => {
                if let Some(q) = self.questions.get_mut(question) {
                    q.view.open = false;
                }
            }
            EventKind::QuestionAnswered { question, answer } => {
                if let Some(p) = project.as_ref().and_then(|id| self.projects.get_mut(id)) {
                    p.answers.insert(question.clone(), e.id);
                    p.orchestrator_failures = (0, None);
                }
                if let Some(q) = self.questions.get(question)
                    && let Some(g) = self.goals.values_mut().find(|g| {
                        q.lapses_on == LapsesOn::GoalVersion(g.version)
                            && (q.orchestrator
                                || q.plan_question
                                || (*answer == dvb_core::event::Answer::Choice(PLAN_AGAIN)
                                    && q.planning_run.is_some()
                                    && g.planning_run == q.planning_run))
                    })
                {
                    g.planning_held = false;
                }
                if let Some(q) = self.questions.get_mut(question) {
                    q.view.open =
                        q.main_ci.is_some() && *answer == dvb_core::event::Answer::Choice(1);
                    q.view.answer = if q.view.open {
                        None
                    } else {
                        Some(answer.clone())
                    };
                    q.answered_at = Some(e.id);
                }
            }
            _ => {}
        }
        // Closing is a projection of the durable release, like version lapse
        // and drop. No synthetic owner answer or new public event is required.
        for q in self
            .questions
            .values_mut()
            .filter(|q| q.planning_run.is_some())
        {
            if !self.goals.values().any(|g| {
                g.planning_held
                    && q.lapses_on == LapsesOn::GoalVersion(g.version)
                    && q.planning_run == g.planning_run
            }) {
                q.view.open = false;
            }
        }
    }
}
