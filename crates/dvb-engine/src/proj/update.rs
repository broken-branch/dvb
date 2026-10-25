use super::Projections;
use dvb_core::api::UpdateCompletion;
use dvb_core::{Actor, Event, EventKind};

impl Projections {
    pub fn update_completion(&self) -> Option<UpdateCompletion> {
        let mut result = self.update_completed.clone()?;
        result.pending = u32::try_from(
            self.update_interrupted
                .iter()
                .filter(|task| {
                    self.tasks
                        .get(*task)
                        .is_some_and(|t| t.update_stop.is_some())
                })
                .count(),
        )
        .unwrap_or(u32::MAX);
        Some(result)
    }

    pub(super) fn fold_update(&mut self, event: &Event) {
        if let EventKind::TaskStopped {
            task, update: true, ..
        } = &event.kind
            && event.actor == Actor::Engine
        {
            self.update_batch.insert(task.clone());
        }
        if let EventKind::EngineUpdated { to, .. } | EventKind::EngineUpdateFailed { to, .. } =
            &event.kind
        {
            self.update_interrupted = std::mem::take(&mut self.update_batch);
            self.update_completed = Some(UpdateCompletion {
                to: to.clone(),
                stopped: u32::try_from(self.update_interrupted.len()).unwrap_or(u32::MAX),
                resumed: 0,
                pending: 0,
            });
        }
        if let EventKind::TaskResumed { task, .. } = &event.kind
            && self.update_interrupted.remove(task)
            && let Some(result) = &mut self.update_completed
        {
            result.resumed = result.resumed.saturating_add(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dvb_core::{EventId, ProjectId, TaskId, TaskState, Timestamp};

    fn ev(id: u64, kind: EventKind) -> Event {
        Event {
            id: EventId(id),
            at: Timestamp(0),
            actor: Actor::Engine,
            project: Some(ProjectId::new("p")),
            kind,
            evidence: None,
        }
    }

    #[test]
    fn update_receipt_folds_incrementally_and_replays_without_the_store() {
        let mut proj = Projections::default();
        let mut events = Vec::new();
        let mut apply = |proj: &mut Projections, mut event: Event| {
            event.id = EventId(u64::try_from(events.len()).unwrap() + 1);
            proj.apply(&event);
            events.push(event);
            assert_eq!(
                proj.update_completion(),
                Projections::rebuild(&events).update_completion()
            );
        };
        for kind in crate::test_support::events::approved_task_events() {
            apply(&mut proj, ev(0, kind));
        }
        let task = TaskId::new("t");
        let stop = EventKind::TaskStopped {
            task: task.clone(),
            update: true,
            reason: None,
        };
        let updated = EventKind::EngineUpdated {
            from: "1.0.0".into(),
            to: "2.0.0".into(),
            snapshot: "backup".into(),
        };
        // Only engine update stops belong in a receipt.
        let mut owner_stop = ev(0, stop.clone());
        owner_stop.actor = Actor::Owner;
        apply(&mut proj, owner_stop);
        apply(&mut proj, ev(0, updated.clone()));
        assert_eq!(proj.update_completion().unwrap().stopped, 0);
        apply(&mut proj, ev(0, stop.clone()));
        apply(&mut proj, ev(0, updated.clone()));
        let receipt = proj.update_completion().unwrap();
        assert_eq!(
            (receipt.stopped, receipt.resumed, receipt.pending),
            (1, 0, 1)
        );
        apply(
            &mut proj,
            ev(
                0,
                EventKind::TaskStateChanged {
                    task: task.clone(),
                    from: TaskState::Stopped {
                        was: Box::new(TaskState::Running),
                    },
                    to: TaskState::Preparing,
                },
            ),
        );
        assert_eq!(proj.update_completion().unwrap().pending, 0);
        apply(
            &mut proj,
            ev(
                0,
                EventKind::TaskResumed {
                    task: task.clone(),
                    reason: None,
                },
            ),
        );
        assert_eq!(proj.update_completion().unwrap().resumed, 1);
        // Rollback receipts use the same fold, and a subsequent update resets the counts.
        apply(&mut proj, ev(0, stop));
        apply(
            &mut proj,
            ev(
                0,
                EventKind::EngineUpdateFailed {
                    from: "1.0.0".into(),
                    to: "2.0.0".into(),
                    reason: "rollback".into(),
                },
            ),
        );
        assert_eq!(proj.update_completion().unwrap().pending, 1);
        apply(
            &mut proj,
            ev(
                0,
                EventKind::TaskStopped {
                    task,
                    update: false,
                    reason: None,
                },
            ),
        );
        assert_eq!(proj.update_completion().unwrap().pending, 0);
        apply(&mut proj, ev(0, updated));
        let receipt = proj.update_completion().unwrap();
        assert_eq!(
            (receipt.stopped, receipt.resumed, receipt.pending),
            (0, 0, 0)
        );
    }
}
