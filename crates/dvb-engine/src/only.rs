//! Install-wide model selection folded from durable events.

use crate::{AppState, Engine, Fail};
use dvb_core::api::SetOnly;
use dvb_core::model::OnlyModels;
use dvb_core::{Actor, EventId, EventKind, Refusal, RefusalCode};
use std::sync::Arc;
use std::time::Duration;

impl Engine {
    /// Reads the effective switch, ending an overdue record exactly once.
    pub fn only_models(&self) -> Result<Option<OnlyModels>, Fail> {
        self.expire_only(None)?;
        Ok(self.proj().only.as_ref().map(|(_, only)| only.clone()))
    }

    pub fn set_only(self: &Arc<Self>, body: SetOnly) -> Result<OnlyModels, Fail> {
        let events = self.append_checked(None, Actor::Owner, |proj, now| {
            let only = OnlyModels {
                models: body.models,
                ends_at: now.plus_millis(i64::from(body.hours) * 3_600_000),
            };
            if !(1..=168).contains(&body.hours) {
                return Err(Refusal::new(
                    RefusalCode::InvalidValue,
                    "hours must be a whole number from 1 to 168",
                )
                .into());
            }
            let valid: Vec<_> = self
                .pool
                .models
                .iter()
                .filter(|m| proj.model_selected(&m.id))
                .map(|m| m.id.as_str())
                .collect();
            if only.models.is_empty() || only.models.iter().any(|id| !valid.contains(&id.as_str()))
            {
                return Err(Refusal::new(
                    RefusalCode::ModelNotInPool,
                    format!("valid model ids: {}", valid.join(", ")),
                )
                .into());
            }
            let mut kinds = Vec::new();
            if let Some((started, _)) = &proj.only {
                kinds.push(EventKind::OnlyEnded { started: *started });
            }
            kinds.push(EventKind::OnlyStarted(only));
            Ok(kinds)
        })?;
        let event = events
            .last()
            .ok_or_else(|| Fail::internal("switch start missing"))?;
        let EventKind::OnlyStarted(only) = &event.kind else {
            return Err(Fail::internal("switch start missing"));
        };
        schedule(self, event.id);
        Ok(only.clone())
    }

    pub fn clear_only(&self) -> Result<(), Fail> {
        self.append_checked(None, Actor::Owner, |proj, _now| {
            Ok(proj
                .only
                .as_ref()
                .map(|(started, _)| EventKind::OnlyEnded { started: *started })
                .into_iter()
                .collect())
        })?;
        Ok(())
    }

    /// A timer may end only its own generation. Reads and launch decisions use
    /// the same deadline check, including before sockets are served at startup.
    pub(crate) fn expire_only(&self, expected: Option<EventId>) -> Result<(), Fail> {
        self.append_checked(None, Actor::Engine, |proj, now| {
            Ok(proj
                .only
                .as_ref()
                .filter(|(id, only)| expected.is_none_or(|want| want == *id) && only.ends_at <= now)
                .map(|(started, _)| EventKind::OnlyEnded { started: *started })
                .into_iter()
                .collect())
        })?;
        Ok(())
    }
}

pub(crate) fn restore(e: &AppState) -> Result<(), Fail> {
    e.expire_only(None)?;
    let started = e.proj().only.as_ref().map(|(id, _)| *id);
    if let Some(started) = started {
        schedule(e, started);
    }
    Ok(())
}

fn schedule(e: &AppState, started: EventId) {
    // A sleeping deadline must not keep an old engine/store alive after restart.
    let weak = Arc::downgrade(e);
    if tokio::runtime::Handle::try_current().is_ok() {
        crate::spawn("end model switch", async move {
            loop {
                let Some(e) = weak.upgrade() else {
                    return Ok(());
                };
                e.expire_only(Some(started))?;
                let deadline = e
                    .proj()
                    .only
                    .as_ref()
                    .filter(|(id, _)| *id == started)
                    .map(|(_, only)| only.ends_at);
                let Some(deadline) = deadline else {
                    return Ok(());
                };
                let wait = Duration::from_millis(
                    u64::try_from(deadline.0.saturating_sub(e.clock.now().0)).unwrap_or(0),
                );
                drop(e);
                tokio::time::sleep(wait).await;
            }
        });
    }
}
