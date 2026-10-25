//! The forge: GitHub and the Fake forge, behind `dvb_core::forge::Forge`.

pub mod fake;
mod gateway;
pub mod github;
mod github_board;
mod github_issues;
pub mod github_setup;
pub mod scan;

pub use fake::FakeForge;
pub use gateway::Wait;
pub use github::GitHub;
