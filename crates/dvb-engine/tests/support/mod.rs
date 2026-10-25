// Shared helpers are compiled separately for each integration test binary.
#![expect(
    dead_code,
    reason = "Each integration test uses a subset of the shared fixtures"
)]
pub mod events;
pub mod git;
pub mod http;
pub mod wait;

mod forge;
pub type InterruptedForge = forge::InterruptedForge;

pub mod assignment;
pub mod engine;
pub mod launcher;
pub mod orchestrator;
