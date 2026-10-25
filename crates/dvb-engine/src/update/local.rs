//! The desk's own update on a cockpit-only box: no engine here, only
//! `/opt/dvb`, which the install made the owner's. The release is fetched and
//! verified exactly as the engine does it; then `current` moves. The caller
//! restarts the desk.

use super::fetch::fetch;
use super::handover::{Layout, prune_versions, set_current};
use super::protocol::Source;
use crate::InternalError;

/// The version `current` points at, from its link (`versions/<v>`).
fn installed(l: &Layout) -> Option<String> {
    let link = std::fs::read_link(l.current()).ok()?;
    Some(link.file_name()?.to_string_lossy().into_owned())
}

/// Installs `to` (or the newest release) beside the running version and moves
/// `current` to it. Returns the version, or `None` when already the newest.
pub async fn install(
    l: &Layout,
    src: &Source,
    to: Option<String>,
) -> Result<Option<String>, InternalError> {
    let current = dvb_core::VERSION;
    let Some(target) = src.target(current, to).await? else {
        return Ok(None);
    };
    let to = target.version;
    let downgrade = target.downgrade;
    let schema = dvb_store::schema_version();
    fetch(l, &src.origin, &to, &src.key, schema, downgrade).await?;
    let from = installed(l).unwrap_or_else(|| current.to_owned());
    set_current(l, &to).map_err(InternalError::from)?;
    prune_versions(l, &[&from, &to]);
    Ok(Some(to))
}
