# Workers, installation and CI

`dvb-run` prepares a workspace, checks its environment, drives a provider CLI,
reports output and collects the finished tree. Runs execute in isolated systemd
units on an install or child processes in dev mode. The `dvb` crate supplies the
single executable; installation and scripts package and exercise it.

## Main modules

- `crates/dvb/src/main.rs`: executable dispatch.
- `crates/dvb-run/src/lib.rs`, `exec.rs`: worker entry, preparation, build cache,
  toolchain install and collection.
- `crates/dvb-run/src/preflight.rs`: environment checks and fix hints.
- `crates/dvb-run/src/claude.rs`, `codex.rs`, `fake.rs`: provider implementations.
- `crates/dvb-run/src/stream.rs`, `usage.rs`, `child.rs`: output, tokens and children.
- `crates/dvb-run/src/client.rs`, `verb.rs`, `login.rs`: run API and vendor login.
- `install/install.sh`: prerequisites, users, paths and units; see
  [installation details](../install.md).
- `install/dvb*.service`, `dvb*.socket`, `dvb-run-.service.d/10-common.conf`,
  `50-dvb.rules`: engine, worker and desk services, socket access and polkit.
- `scripts/run-e2e.sh`, `run-upgrade.sh`: acceptance and upgrade entry points.
- `scripts/e2e/lib.sh`: sourced transport, polling, diagnostics and origin seeding.
- `scripts/provider-e2e.sh`, `scripts/e2e/browser.mjs`: opt-in provider and browser checks.
- `crates/dvb/tests/crash.rs`: crash recovery with persistent Fake forge and FIFO gates.
- `scripts/test-install.py`: temporary-root installer regression cases.
- `scripts/check.sh`, `gen-contracts.sh`, `release.sh`: CI gates, schemas and packaging.
- `scripts/public-copy.sh`, `leak-check.sh`: the tree the public copy sees (rewrites, and
  files with its private line left out) and its leak gate; `scripts/test-leak-check.py`
  has their cases.
- `.github/workflows/ci.yml`, `release.yml`, `cache.yml`, `stranger.yml`: pull request
  checks, signed releases, the cache refresh on push to main, and the dispatch-only
  install of the latest published release.

## Start reading

Follow `exec.rs` from `RunEnv` through preparation, preflight, provider execution
and Git collection. Follow `verb.rs` for model-side commands. Engine `launch.rs`
prepares the run directory, environment and prompt, starts and watches the unit;
`prompt.rs` selects templates, `units.rs` prepares login mounts and chooses
installed or dev launchers. `dispatch/runs.rs` cleans build output and removes
run directories nothing can resume from (`sweep_run_dirs` at start),
`publish::fetch_collected` takes the collected bundle into the mirror and
`reconcile/collection.rs` recovers durable collection reports.

For installation, read the installer with the engine sockets and worker units.
Provider fixtures are under `crates/dvb-run/tests/fixtures/`; binary and
installed-boundary assertions are under `crates/dvb/tests/`; the root
`tests/support/` shares the Unix-socket server and executable stubs.

Use the preflight skill for check changes. See [engine](engine-rest.md),
[core](core.md) and [testing](../testing.md) for neighboring flows and check scope.
