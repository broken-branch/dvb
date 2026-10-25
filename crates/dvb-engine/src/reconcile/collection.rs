//! Recover a builder's durable collected snapshot after a lost report.
use crate::launch::driver_reason;
use crate::{AppState, Fail};
use dvb_core::event::EndReason;
use dvb_core::{Command, EventKind, RunId};
/// A finished builder can lose its collected report after writing the snapshot.
/// Recover only a successful exit, never a failed model session. Publication
/// performs its scan and review.
pub(crate) async fn recover_collection(
    e: &AppState,
    run: &RunId,
    reason: &EndReason,
) -> Result<(), Fail> {
    let dir = e.cfg.paths.work.join(run.as_str());
    if *reason != EndReason::Completed {
        return Ok(());
    }
    let Some(r) = e.proj().runs.get(run).cloned() else {
        return Ok(());
    };
    if r.builds().is_none()
        || r.ended.is_some()
        || r.preflight_failed.is_some()
        || driver_reason(&dir).is_some()
    {
        return Ok(());
    }
    let head = r.collected.clone().or_else(|| {
        crate::launch::read_run_file(&dir.join("head"))
            .ok()
            .map(|s| s.trim().to_owned())
    });
    let Some(head) =
        head.filter(|h| matches!(h.len(), 40 | 64) && h.bytes().all(|b| b.is_ascii_hexdigit()))
    else {
        return Ok(());
    };
    let Some(project) = e
        .proj()
        .projects
        .get(&r.project)
        .map(|p| p.view.name.clone())
    else {
        return Ok(());
    };
    // Checked in the engine's mirror, from the run's bundle: no git runs in the
    // run's own repository.
    let mirror = e.mirror(&project);
    if let Err(err) = crate::publish::fetch_collected(&mirror, &dir, run.as_str(), &head).await {
        eprintln!("dvb engine: collect run {run}: {err}");
        return Ok(());
    }
    let pre = if r.collected.is_none() {
        vec![EventKind::RunCollected {
            outcome: None,
            run: run.clone(),
            head: Some(head),
            summary: None,
        }]
    } else {
        Vec::new()
    };
    e.run_command(
        run,
        crate::Decision {
            pre,
            ..crate::Decision::new("collected", Command::HeadWritten)
        },
    )?;
    Ok(())
}
