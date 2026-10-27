# Forge, owner CLI and desk

`dvb-forge` implements GitHub and Fake/local Git operations. `dvb-cli` translates
owner commands into engine requests. `dvb-desk` serves the embedded cockpit on
loopback and connects it to the engine and desktop. The `dvb` binary selects
these library entry points.

## Main modules

- `crates/dvb-forge/src/github.rs`: App configuration, scoped tokens, Git
  transport and REST forge operations.
- `crates/dvb-forge/src/gateway.rs`: shared rate-limit gate and diagnostics.
- `crates/dvb-forge/src/github_setup.rs`: account type, manifest, key files and
  installation, for an organization or a personal account.
- `crates/dvb-forge/src/github_board.rs`: board fields/cards and milestones.
- `crates/dvb-forge/src/github_issues.rs`: issue, comment and visibility reads.
- `crates/dvb-forge/src/scan.rs`, `prompts/scan-rules.toml`: publication scanning.
- `crates/dvb-forge/src/fake.rs`: recorded remote evidence, optional local Git and
  dev-mode issue files.
- `crates/dvb-cli/src/lib.rs`: command definitions, dispatch, and text and
  `--json` output.
- `crates/dvb-cli/src/client.rs`: Unix HTTP client, refusals and event streaming.
- `crates/dvb-cli/src/watch.rs`: `dvb watch` feed lines and JSON lines.
- `crates/dvb-cli/src/wait.rs`: [`dvb wait`](../commands.md#waiting-on-a-goal).
- `crates/dvb-cli/src/app.rs`: desk discovery, route validation and app launch.
- `crates/dvb-cli/src/update.rs`: owner update commands.
- `crates/dvb-cli/src/doctor.rs`: [`dvb doctor`](../commands.md#setup-check).
- `crates/dvb-cli/src/github.rs`: `dvb github connect` and the owner's git identity fallback.
- `crates/dvb-desk/src/lib.rs`: browser/control routes, sessions, assets and proxy.
- `crates/dvb-desk/src/github_redirect.rs`: authenticated GitHub setup bridge and
  prefill, and GitHub's callback, answered from the engine's reply with plain desk
  pages (no session needed): the installation redirect, Connected, or failure.
- `crates/dvb-desk/src/theme.rs`, `notifications.rs`: desktop palette and notices
  (questions; with `notify_faults`, failed tasks and stuck CI checks; with
  `notify_completed`, "Goal completed" and "PR #n merged").
- `crates/dvb-desk/src/desktop.rs`: the installed desk's `dvb.desktop` and app icon.
- `crates/dvb-desk/src/omarchy.rs`, `omarchy-menu.json`: the Omarchy menu entries.

## Start reading

For a remote operation, begin with `dvb_core::forge::Forge`, then the GitHub
implementation and its Fake counterpart. Engine `publish/` owns publication
policy and reconciliation, and the engine's `issues.rs` applies the issue trust
rule to the facts the forge reports.

For an owner command, follow the CLI's verb dispatch through `Client` to the
engine route. CLI wording delegates to `dvb_core::wording`. For browser access,
follow `Desk::router`, launch-token redemption and the authenticated proxy. The
browser receives HTTP and events through the desk; the desk connects to the
engine socket.

Forge tests use recorded HTTP responses and local Git. CLI and desk tests live
beside their modules; `crates/dvb/tests/` covers binary/socket integration
(`socket_json.rs`: the `--json` outputs). `skills/dvb/SKILL.md` is the agent
skill for driving the CLI.
`crates/dvb/tests/support/` shares fixtures and captures run prompts from the
socket event feed before completed run directories are removed.
Never expose credentials in diagnostics or publication. Keep authorization and
version checks on the corresponding engine/client boundaries.

See [engine APIs](engine-rest.md), [UI](ui.md), [installation](run.md),
[command reference](../commands.md) and [testing](../testing.md).
