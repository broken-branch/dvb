# Preflight checks and their fixes

Every run checks these before it spends anything. A failed check ends the
run with `run.ended {preflight: <code>}` and shows this hint.

| Code | Fails when | Fix |
|---|---|---|
| `PF_UID` | The run is not running as `dvb-worker` | Reinstall the unit files: `install/install.sh --engine` |
| `PF_HOME` | HOME is not the run's own home (`work/<run>/home`) | Reinstall the unit files |
| `PF_CRED_VISIBLE` | A GitHub token or SSH agent is visible | Remove it from the worker's reach; see the install notes |
| `PF_STORE_VISIBLE` | `/var/lib/dvb/state` is readable from the run | Fix the directory's owner and mode: `install/install.sh --engine` |
| `PF_WS_BASE` | The workspace is not at the task's base head | Stop and resume the task |
| `PF_CLI_MISSING` | The vendor's CLI is not installed for the worker | Install the CLI |
| `PF_LOGIN_MISSING` | The vendor's login is missing or expired | `dvb login <vendor>` |
| `PF_MODEL_UNKNOWN` | The CLI does not know the pinned model | Pick a model from the pool in Settings |
| `PF_MEMORY_CAP` | The run is not in a dvb run unit | Reinstall the unit files |
| `PF_MEMORY_FEATURE` | memories or fast_mode is on in the worker's Codex config | Set `memories = false` and `fast_mode = false` under `[features]` in `/var/lib/dvb/logins/codex/config.toml` |
