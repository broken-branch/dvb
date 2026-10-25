//! The installed slice is the only authority for the aggregate run memory limit.

use crate::{Config, Fail};
use dvb_core::api::{Settings, SettingsKey};
use serde_json::{Value, json};
use std::process::Output;

const GIB: u64 = 1024 * 1024 * 1024;

pub(crate) async fn read(cfg: &Config, settings: &mut Settings) -> Result<(), Fail> {
    let meminfo = std::fs::read_to_string(&cfg.meminfo)
        .map_err(|error| Fail::io("cannot read PC RAM", error))?;
    let ram_kb = meminfo
        .lines()
        .find_map(|line| line.strip_prefix("MemTotal:"))
        .and_then(|line| line.split_whitespace().next())
        .and_then(|n| n.parse::<u64>().ok())
        .ok_or_else(|| Fail::internal("cannot read MemTotal from PC RAM information"))?;
    settings.ram_gb = Some(ram_kb / (GIB / 1024));
    settings.half_ram_gb = Some(ram_kb / (GIB / 1024) / 2);
    settings.values.remove(&SettingsKey::RunsMemoryGb);
    if cfg.systemctl.is_none() {
        return Ok(());
    }
    let output = systemctl(
        cfg,
        &[
            "show",
            "dvb.slice",
            "-p",
            "FragmentPath",
            "-p",
            "DropInPaths",
            "-p",
            "MemoryMax",
        ],
    )
    .await?;
    success(&output)?;
    let text = String::from_utf8_lossy(&output.stdout);
    // systemd synthesizes loaded slices even without a unit file or drop-in.
    if !text
        .lines()
        .filter_map(|line| {
            line.strip_prefix("FragmentPath=")
                .or_else(|| line.strip_prefix("DropInPaths="))
        })
        .any(|paths| !paths.is_empty())
    {
        return Ok(());
    }
    let memory = text
        .lines()
        .find_map(|line| line.strip_prefix("MemoryMax="))
        .ok_or_else(|| Fail::internal("systemctl did not report dvb.slice MemoryMax"))?;
    let value = if memory == "infinity" {
        Value::Null
    } else {
        json!(
            memory
                .parse::<u64>()
                .map_err(|_| Fail::internal("systemctl reported invalid dvb.slice MemoryMax"))?
                / GIB
        )
    };
    settings.values.insert(SettingsKey::RunsMemoryGb, value);
    Ok(())
}

pub(crate) async fn save(cfg: &Config, gb: u64) -> Result<(), Fail> {
    let output = systemctl(
        cfg,
        &[
            "set-property",
            "dvb.slice",
            &format!("MemoryMax={gb}G"),
            &format!("MemoryHigh={}M", gb * 896),
        ],
    )
    .await?;
    success(&output)
}

async fn systemctl(cfg: &Config, args: &[&str]) -> Result<Output, Fail> {
    let program = cfg.systemctl.as_ref().ok_or_else(|| {
        Fail::internal("this engine has no dvb.slice; install the engine units first")
    })?;
    tokio::process::Command::new(program)
        .arg("--no-ask-password")
        .args(args)
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|error| Fail::io("cannot run systemctl for dvb.slice", error))
}

fn success(output: &Output) -> Result<(), Fail> {
    if output.status.success() {
        Ok(())
    } else {
        Err(Fail::internal(format!(
            "cannot read or save dvb.slice: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}
