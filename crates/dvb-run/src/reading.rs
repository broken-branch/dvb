//! A usage reading run (a usage check, `POST /usage/{vendor}/check`): one
//! reading of the vendor's usage window, as the worker inside the vendor's unit
//! where its login is mounted, and no session. It is the reading a run takes at
//! its start, so it spends nothing. Like a login run it needs no run-socket
//! registration: it leaves `reading.json` (`UsageReading`) in its run directory
//! for the engine.

use crate::exec::RunEnv;
use dvb_core::io::write_handoff;
use dvb_core::run_verbs::{RateLimitBody, UsageReading};
use dvb_core::{RunOutcome, Vendor};

/// The reading run. Exit codes: 0 read, 2 setup failed, 4 no reading.
pub fn run(env: &RunEnv) -> RunOutcome {
    let (reading, outcome) = match read(env) {
        Ok(window) => (UsageReading::Read(window), RunOutcome::Completed),
        Err(e) => {
            eprintln!("usage reading: {e}");
            (UsageReading::Failed(e), RunOutcome::SessionFailed)
        }
    };
    let written = serde_json::to_string(&reading)
        .map_err(|e| e.to_string())
        .and_then(|text| {
            write_handoff(&env.dir.join("reading.json"), &text).map_err(|e| e.to_string())
        });
    match written {
        Ok(()) => outcome,
        Err(e) => {
            eprintln!("reading.json: {e}");
            RunOutcome::SetupFailed
        }
    }
}

fn read(env: &RunEnv) -> Result<RateLimitBody, String> {
    std::fs::create_dir_all(env.home()).map_err(|e| format!("home: {e}"))?;
    let window = match env.vendor {
        Vendor::Codex => {
            let bin = crate::preflight::find_cli(env).ok_or("codex is not on PATH")?;
            crate::codex::read_window(&bin, &env.home(), &env.dir).map_err(|e| e.to_string())?
        }
        Vendor::Fake => crate::fake::read_window(
            env.fake_script
                .as_ref()
                .ok_or("DVB_FAKE_SCRIPT is not set")?,
        )?,
        // Its window arrives only with a model reply, which spends usage.
        Vendor::Claude => return Err("claude has no usage reading that spends nothing".into()),
    };
    window.ok_or_else(|| format!("{} gave no usage window", env.vendor.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use dvb_core::Timestamp;

    fn reading(dir: &std::path::Path) -> UsageReading {
        serde_json::from_str(&std::fs::read_to_string(dir.join("reading.json")).unwrap()).unwrap()
    }

    #[test]
    fn the_fake_vendors_reading_is_its_scripts_window() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("fake-script.json");
        std::fs::write(
            &script,
            r#"[{"rate_limit": {"used_pct": 100, "window": "5h", "resets_at": 1800000000000}}]"#,
        )
        .unwrap();
        let mut env = crate::testutil::run_env(dir.path(), Vendor::Fake, "usage");
        env.fake_script = Some(script.clone());
        assert_eq!(run(&env), RunOutcome::Completed);
        assert_eq!(
            reading(dir.path()),
            UsageReading::Read(RateLimitBody {
                used_pct: 100.0,
                window: "5h".into(),
                resets_at: Some(Timestamp(1_800_000_000_000)),
            })
        );

        std::fs::write(&script, r#"[{"exit": 0}]"#).unwrap();
        assert_eq!(run(&env), RunOutcome::SessionFailed);
        assert_eq!(
            reading(dir.path()),
            UsageReading::Failed("fake gave no usage window".into())
        );
    }

    #[test]
    fn claude_has_no_reading() {
        let dir = tempfile::tempdir().unwrap();
        let env = crate::testutil::run_env(dir.path(), Vendor::Claude, "usage");
        assert_eq!(run(&env), RunOutcome::SessionFailed);
        assert_eq!(
            reading(dir.path()),
            UsageReading::Failed("claude has no usage reading that spends nothing".into())
        );
    }
}
