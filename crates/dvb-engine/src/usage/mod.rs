//! Usage: tokens and estimated cost per project, role and model, from the
//! event log (`run.started`, `run.rerouted` and `run.usage`); the vendor windows.

pub mod prices;

use dvb_core::api::{UsageQuery, UsageRow};
use dvb_core::{Event, EventKind, ProjectId, RoleId, RunId, Vendor};
use std::collections::BTreeMap;

type Key = (ProjectId, RoleId, String, String);

/// Sums `run.usage` by project, role and model, one event at a time. Events
/// before `since` and runs of other projects are left out.
pub struct Sums<'a> {
    q: &'a UsageQuery,
    runs: BTreeMap<RunId, (ProjectId, RoleId, String, Vendor)>,
    sums: BTreeMap<Key, UsageRow>,
}

impl<'a> Sums<'a> {
    pub fn new(q: &'a UsageQuery) -> Self {
        Self {
            q,
            runs: BTreeMap::new(),
            sums: BTreeMap::new(),
        }
    }

    pub fn add(&mut self, ev: &Event) {
        if let EventKind::RunStarted(a) = &ev.kind {
            if let Some(project) = &ev.project {
                self.runs.insert(
                    a.run.clone(),
                    (project.clone(), a.role.clone(), a.model.clone(), a.vendor),
                );
            }
        } else if let EventKind::RunRerouted {
            run,
            to_model,
            vendor,
            ..
        } = &ev.kind
        {
            if let Some((_, _, model, run_vendor)) = self.runs.get_mut(run) {
                model.clone_from(to_model);
                *run_vendor = *vendor;
            }
        } else if let EventKind::RunUsage {
            run,
            uncached_in,
            cached_in,
            out,
            est_cost,
        } = &ev.kind
        {
            let Some((project, role, model, vendor)) = self.runs.get(run) else {
                return;
            };
            if self.q.project.as_ref().is_some_and(|p| p != project)
                || self.q.since.is_some_and(|since| ev.at < since)
            {
                return;
            }
            let key = (
                project.clone(),
                role.clone(),
                model.clone(),
                vendor.as_str().to_owned(),
            );
            let row = self.sums.entry(key).or_insert_with(|| UsageRow {
                project: project.clone(),
                role: role.clone(),
                model: model.clone(),
                vendor: *vendor,
                uncached_in: 0,
                cached_in: 0,
                out: 0,
                est_cost: 0.0,
            });
            row.uncached_in += uncached_in;
            row.cached_in += cached_in;
            row.out += out;
            row.est_cost += est_cost;
        }
    }

    pub fn rows(self) -> Vec<UsageRow> {
        self.sums.into_values().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dvb_core::{Actor, Effort, EventId, Timestamp};

    fn rows(events: &[Event], q: &UsageQuery) -> Vec<UsageRow> {
        let mut sums = Sums::new(q);
        for ev in events {
            sums.add(ev);
        }
        sums.rows()
    }

    fn ev(id: u64, at: i64, project: &str, kind: EventKind) -> Event {
        Event {
            id: EventId(id),
            at: Timestamp(at),
            actor: Actor::Engine,
            project: Some(ProjectId::new(project)),
            kind,
            evidence: None,
        }
    }

    fn started(id: u64, project: &str, run: &str, role: &str, model: &str) -> Event {
        ev(
            id,
            0,
            project,
            EventKind::RunStarted(
                crate::test_support::assignment::Assignment::new(RunId::new(run))
                    .role(RoleId::new(role))
                    .model(model.into())
                    .vendor(Vendor::Claude)
                    .reason(String::new())
                    .build(),
            ),
        )
    }

    fn used(id: u64, at: i64, project: &str, run: &str, n: u64) -> Event {
        ev(
            id,
            at,
            project,
            EventKind::RunUsage {
                run: RunId::new(run),
                uncached_in: n,
                cached_in: n * 2,
                out: n * 3,
                est_cost: 0.5,
            },
        )
    }

    #[test]
    fn rerouted_usage_uses_the_actual_model_and_vendor() {
        let events = [
            started(1, "p", "r", "backend", "claude-opus-5-5"),
            ev(
                2,
                0,
                "p",
                EventKind::RunRerouted {
                    run: RunId::new("r"),
                    task: None,
                    role: RoleId::new("backend"),
                    from_model: "claude-opus-5-5".into(),
                    to_model: "gpt-6.1-sol".into(),
                    vendor: Vendor::Codex,
                    effort: Effort::High,
                    limited: None,
                    resets_at: None,
                },
            ),
            used(3, 100, "p", "r", 10),
        ];
        let rows = rows(&events, &UsageQuery::default());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].model, "gpt-6.1-sol");
        assert_eq!(rows[0].vendor, Vendor::Codex);
        assert_eq!(
            (rows[0].uncached_in, rows[0].cached_in, rows[0].out),
            (10, 20, 30)
        );
        assert_eq!(rows[0].est_cost, 0.5);
    }

    #[test]
    fn usage_sums_by_project_role_and_model() {
        let events = [
            started(1, "p1", "r1", "backend", "claude-sonnet-5-5"),
            started(2, "p1", "r2", "backend", "claude-sonnet-5-5"),
            started(3, "p2", "r3", "backend", "claude-sonnet-5-5"),
            used(4, 100, "p1", "r1", 1),
            used(5, 200, "p1", "r2", 10),
            used(6, 300, "p2", "r3", 100),
        ];
        let all = rows(&events, &UsageQuery::default());
        assert_eq!(all.len(), 2);
        let p1 = all
            .iter()
            .find(|r| r.project == ProjectId::new("p1"))
            .unwrap();
        assert_eq!((p1.uncached_in, p1.cached_in, p1.out), (11, 22, 33));
        assert!((p1.est_cost - 1.0).abs() < 1e-9);

        let one = rows(
            &events,
            &UsageQuery {
                project: Some(ProjectId::new("p2")),
                since: None,
            },
        );
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].uncached_in, 100);

        let late = rows(
            &events,
            &UsageQuery {
                project: None,
                since: Some(Timestamp(250)),
            },
        );
        assert_eq!(late.len(), 1);
        assert_eq!(late[0].project, ProjectId::new("p2"));
    }
}
