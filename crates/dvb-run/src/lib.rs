//! The run side: prepare, preflight, exec, collect; the providers; and the
//! `dvb run <verb>` client the model's CLI calls.

mod child;
mod claude;
mod client;
mod codex;
mod exec;
pub mod fake;
mod login;
mod preflight;
mod stream;
mod usage;
mod verb;

pub use exec::{prepare_build_cache, share_run};
pub use stream::StderrTail;

/// `dvb run exec <run>` or `dvb run <verb> [<json>]`; returns the exit code.
pub async fn main(args: &[String]) -> std::process::ExitCode {
    use std::process::Termination;
    match args {
        [cmd, run] if cmd == "exec" => exec::exec(run).await.report(),
        _ => std::process::ExitCode::from(u8::try_from(verb::verb(args)).unwrap_or(1)),
    }
}

#[cfg(test)]
mod testutil {
    use crate::exec::RunEnv;
    use dvb_core::{Effort, Vendor};
    use std::path::{Path, PathBuf};

    /// A dev-mode run in `dir`; tests override what they exercise.
    pub fn run_env(dir: &Path, vendor: Vendor, model: &str) -> RunEnv {
        RunEnv {
            run: "r1".into(),
            vendor,
            model: model.into(),
            effort: Effort::Medium,
            base: String::new(),
            mirror: PathBuf::new(),
            run_sock: dir.join("sock"),
            prompt: None,
            fake_script: None,
            dev: true,
            login: false,
            cli_bin: None,
            dir: dir.to_owned(),
        }
    }

    pub use crate::executable::write_executable;
}

#[cfg(test)]
#[path = "../../../tests/support/executable.rs"]
mod executable;

#[cfg(test)]
#[path = "../../../tests/support/http.rs"]
pub mod test_http;
