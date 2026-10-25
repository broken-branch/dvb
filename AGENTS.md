# dvb — code instructions

dvb is a development system. Its engine takes approved goals through planning,
building, review and merge; the maintainer uses a command line and a desktop
cockpit.

## Find the code

Start with [docs/map/README.md](docs/map/README.md), then read the map for the
area you are changing. The Cargo workspace is under `crates/`; the embedded
React/TypeScript cockpit is under `ui/`. `contracts/`, `prompts/` and `install/`
hold wire contracts, runtime instructions and installation files.

Read `.agents/skills/dvb-conventions/SKILL.md` and any skill relevant to the
change. Keep the owning code map current when its modules or entry points
change. UI conventions are in `ui/DESIGN.md`.

## Run, build and check

Before a PR, run formatting, clippy and tests for changed packages, plus targeted UI checks for UI changes; CI runs the complete suite and gates merge.
See [docs/testing.md](docs/testing.md) for commands, prerequisites and releases.
Use a private, writable build directory and set `CARGO_TARGET_DIR` to it.
In a managed run, keep the existing value.

## Deliver the change

In a dvb-managed run, follow the assigned role prompt and `dvb run` verbs;
the engine collects and publishes the worker's tree. A change made outside dvb
uses a branch and pull request.

Commit changes made outside dvb with your GitHub noreply address as author and
committer (`login@users.noreply.github.com` or `id+login@users.noreply.github.com`);
the CI identity check refuses any other address.
Use `[contract]` in PR titles to mark changes to `crates/dvb-core`, contracts, prompts, installation, units or CI.

No secrets, tokens or credentials in files or commits. Build the simplest thing
that works: remove unused code and do not build for hypothetical problems.
A change made outside dvb is a pull request only; dvb merges its own reviewed
work, and an outside pull request is read and, if wanted, re-made by the
maintainers rather than merged as it is ([CONTRIBUTING.md](CONTRIBUTING.md)).
Migrations are add-only: preserve deployed migration bytes and durable event
payloads.
Coordinate callers and regenerate contracts after changing `dvb-core`.
