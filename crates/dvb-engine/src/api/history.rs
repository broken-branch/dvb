//! Version history for project checks, roles, hires and settings; restore for
//! roles, hires and settings. A history is
//! read from the events that set the record, newest first; a restore appends
//! the matching `*.changed` event carrying the old value and `restored_from`.

use crate::proj::Projections;
use crate::{AppState, Fail};
use dvb_core::api::Version;
use dvb_core::{Actor, Event, EventId, EventKind, ProjectId, Refusal, RefusalCode};

/// The events that set one record, oldest first, each with the value it set.
pub(super) fn versions_of<T>(
    e: &AppState,
    pick: impl Fn(&EventKind) -> Option<T>,
) -> Result<Vec<(Event, T)>, Fail> {
    Ok(e.store
        .select(EventId(0), dvb_store::Select::WithoutTelemetry)?
        .into_iter()
        .filter_map(|ev| pick(&ev.kind).map(|v| (ev, v)))
        .collect())
}

pub(super) fn newest_first<T, U>(
    found: Vec<(Event, T)>,
    value: impl Fn(T) -> U,
) -> Vec<Version<U>> {
    found
        .into_iter()
        .rev()
        .map(|(ev, v)| Version {
            event: ev.id,
            at: ev.at,
            actor: ev.actor,
            value: value(v),
        })
        .collect()
}

/// Picks the version to restore, checking that the owner was looking at the
/// record's latest event.
pub(super) fn pick<T: Clone>(
    found: &[(Event, T)],
    what: &str,
    r: (EventId, EventId),
) -> Result<T, Fail> {
    let (event, current) = r;
    let Some((latest, _)) = found.last() else {
        return Err(Fail::NotFound(what.to_owned()));
    };
    if latest.id != current {
        return Err(Refusal::new(
            RefusalCode::ValueChanged,
            format!("{what} changed since you looked at it; look again"),
        )
        .into());
    }
    found
        .iter()
        .find(|(ev, _)| ev.id == event)
        .map(|(_, v)| v.clone())
        .ok_or_else(|| Fail::NotFound(format!("event {event} of {what}")))
}

/// Appends the restoring event, checked and appended under one projection lock;
/// a refused restore is recorded like any other refused command.
pub(super) fn restore(
    e: &AppState,
    project: Option<&ProjectId>,
    command: &str,
    build: impl FnOnce(&Projections) -> Result<Vec<EventKind>, Fail>,
) -> Result<(), Fail> {
    e.append_checked(project, Actor::Owner, |proj, _now| build(proj))
        .map(|_| ())
        .map_err(|error| e.refuse(project, Actor::Owner, command, error))
}
