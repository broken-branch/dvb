# Testing and releases

Run commands from the repository root. [CI](../.github/workflows/ci.yml),
[scripts/check.sh](../scripts/check.sh) and the [release workflow](../.github/workflows/release.yml)
define the complete gates; [code maps](map/README.md) locate implementation and fixtures.
`scripts/check.sh` requires `CARGO_TARGET_DIR` to be set.

## Prerequisites

Use [rust-toolchain.toml](../rust-toolchain.toml) (Rust, rustfmt, Clippy), Node 26,
npm, Python 3, Bash, Git, jq and curl. Rust fixtures also use `sh`, `setsid`, OpenSSL
and `systemd-tmpfiles`. Downloads require network access or populated caches.
The cockpit embeds `ui/dist`; build UI assets before compiling a changed cockpit.

`scripts/check.sh` refuses to run without these:

- Node 26.
- The stable Rust toolchain through rustup (`rustup show active-toolchain` names
  `stable-…`).
- cargo-nextest 0.9.148 and cargo-deny 0.20.2, for its `rust` part:

```sh
cargo install --locked cargo-nextest --version 0.9.148
cargo install --locked cargo-deny --version 0.20.2
```

## Before a PR

Run formatting, clippy and tests for each package you changed. For UI changes,
run lint and the UI tests covering the change. CI runs the complete suite and
is the merge gate; do not repeat the workspace, full UI, e2e or upgrade suites
locally unless the task explicitly requires them.

```sh
cargo fmt --check
cargo clippy --all-targets -p <changed-package>
cargo test -p <changed-package>
(cd ui && npm run lint)
(cd ui && npx vitest run tests/<covering-test>.test.tsx)
```

After changing `dvb-core`, also run `cargo check --workspace --all-targets`.
For changes to contracts, prompts, installation, units or CI, run
`scripts/gen-contracts.sh` and include any generated changes. Never hand-edit
`contracts/openapi.json` or `ui/src/api/schema.ts`. Format UI edits with
`(cd ui && npm run format)`. Use focused shell/Python/unit checks for script or
installer changes, including `scripts/test-install.py` for installer behavior.
`scripts/test-leak-check.py` covers `scripts/leak-check.sh` and `scripts/public-copy.sh`;
the `contracts` job runs it on pull requests, then leak-checks the public copy of the
tree with the `LEAK_DENYLIST` secret as the denylist (skipped with a notice when empty).

Contract generation also publishes `prompts/verdict.schema.json`,
`contracts/wording.json` and `ui/src/api/wording.json`. The contracts gate requires
all generated artifacts to reproduce exactly and be included in the collected head.

## CI and acceptance

Obtain code review and pass every PR check, including any applicable release dry run.
The `rust`, `ui`, `contracts`, `identity`, `e2e` and `upgrade` jobs cover Rust
checks/tests, UI build/format/lint/tests, schema regeneration, commit identity and
secrets, installed Fake acceptance, and upgrade preservation/rollback.
`scripts/check.sh [ui|rust|contracts]` reproduces the corresponding complete gate.
CI runs on pull requests and on manual `workflow_dispatch` (use it to run the
complete suite on main). It does not run on merges: the main ruleset requires
up-to-date branches and dvb squash-merges, so main gets the tree that just passed
in its PR. A push to main runs only [cache.yml](../.github/workflows/cache.yml),
which saves the Rust and npm caches PR runs restore and builds only when a cache
key is missing.
CI uses pinned nextest and cargo-deny versions from the workflow; nextest retries
are disabled. Tests use Fake providers/forges without model calls or network.

`scripts/run-e2e.sh --dev --no-browser` uses a temporary engine and local Git;
installed acceptance requires a disposable systemd/polkit install with sudo and
`DVB_FAKE=1`. Browser checks require Chrome and the UI-pinned playwright-core.
Run browsers only when the assigned task requires them. Real-provider checks
are opt-in via `--provider claude|codex` with `DVB_E2E_PROJECT`; they need a
configured, dedicated idle project and consume model usage.
`(cd ui && DVB_DEMO_DIR=<dir> npm run test:browser)` also writes `<dir>/cockpit.png` and, through the Omarchy `/theme.css` path, `<dir>/themes/tokyo-night.png`, `catppuccin.png` and `gruvbox.png`; `magick <dir>/cockpit.png -strip +dither -colors 256 PNG8:docs/cockpit.png` makes the README screenshot from it, and the same command makes each `docs/themes/<theme>.png` from `<dir>/themes/<theme>.png`.

`scripts/run-upgrade.sh` has `seed`, `bootstrapped <version>` and
`cases <broken> <other-key> <good>` stages. Follow CI's candidate preparation on
a disposable privileged runner; it changes versions, migrations and signing
keys. Never prepare upgrade candidates in an implementation tree or live install.

## Debugging

Read the first failed command and reproduce its package, test target or UI file.
Preserve failure output; do not hide failures with sleeps, ignores or retries.
For a known unrelated flaky job, rerun it once. Report environment-blocked checks.
For contract failures, inspect generated files and the before/after checkout diff.
Dev acceptance prints its temporary directory containing engine/desk/browser logs.
Installed failures need `systemctl status` and
`sudo journalctl -u dvb.service -u 'dvb-run-*' --no-pager -o short-precise`.
Inspect events, backups, permissions and unit drop-ins when diagnosing upgrades.
Optional mutation testing: give `cargo mutants --test-tool nextest --in-diff <file>`
a diff limited to changed Rust code; inspect survivors for missing assertions.

## Releases

Releases are for maintainers only.

1. Submit a `[contract]` version-only PR changing the workspace version,
   workspace package entries in `Cargo.lock` (`cargo update -w --offline`) and
   generated OpenAPI `info.version`. Inspect regeneration for only those changes.
2. Run focused local checks, obtain code review and every green PR check, then
   squash merge. Tag the merged main commit with annotated `vX.Y.Z` and push it.
3. The tag workflow builds the UI and release binary; `scripts/release.sh` packages
   assets and requires minisign signing/verification. Confirm build, publish and
   release assets. Hyphenated tags are prereleases; other tags are stable releases.
4. Installed machines use `dvb update [--to X.Y.Z]`. Verified updates drain work,
   back up data and roll back on failure. Bootstrap older installs with the
   candidate release's installer; see [requirements](install.md#requirements-in-detail).

A maintainer runs the dispatch-only [stranger check](../.github/workflows/stranger.yml)
by hand to check a published release (`gh workflow run stranger.yml`): on GitHub-hosted
Ubuntu it installs the latest published release as README.md says, with the default
token and the Fake provider and forge, and runs `scripts/run-e2e.sh` on it.
