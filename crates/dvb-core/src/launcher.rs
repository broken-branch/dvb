//! Starting and stopping run units: `Systemd` over D-Bus in an install,
//! `Process` (child processes of the engine) in dev mode and tests.

use crate::ids::RunId;
use crate::model::Vendor;

/// A run's unit: `dvb-run-<vendor>@<run>.service`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct UnitId {
    pub vendor: Vendor,
    pub run: RunId,
}

impl UnitId {
    pub fn unit_name(&self) -> String {
        format!("dvb-run-{}@{}.service", self.vendor.as_str(), self.run)
    }
}

/// A unit's state as the launcher sees it (systemd's activating, deactivating and
/// reloading count as `Active`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnitState {
    Active,
    Inactive,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchError(pub String);

impl std::fmt::Display for LaunchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for LaunchError {}

#[async_trait::async_trait]
pub trait Launcher: Send + Sync {
    /// Resolve the authenticated peer through this launcher's process boundary.
    fn run_of_pid(&self, _pid: i32) -> Option<RunId> {
        None
    }
    async fn start(&self, unit: &UnitId) -> Result<(), LaunchError>;
    async fn stop(&self, unit: &UnitId) -> Result<(), LaunchError>;
    async fn state(&self, unit: &UnitId) -> Result<UnitState, LaunchError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_name_follows_the_template() {
        let u = UnitId {
            vendor: Vendor::Fake,
            run: RunId::new("01J"),
        };
        assert_eq!(u.unit_name(), "dvb-run-fake@01J.service");
    }
}
