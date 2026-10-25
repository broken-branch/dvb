# dvb

dvb turns approved development goals into plans, code, reviews and merged pull
requests. Agents work in isolated runs while you approve plans, answer questions
and follow progress from a command line or desktop cockpit. It works with private
and public repositories in your GitHub organization or personal account.

**Status:** pre-1.0: commands, settings and the cockpit may change between
releases.

**dvb merges its own work.** It merges pull requests reviewed only by its AI
reviewers into the default branch, with no human code review. The ruleset it adds
to each project's default branch requires pull requests, blocks force-push and
deletion and has no bypass actors, so on an existing repository it blocks your own
direct pushes too ([costs and GitHub effects](docs/install.md#costs-and-github-effects)).

## How it works

The **engine** is a system service that keeps dvb's state and talks to GitHub.
The **cockpit** is a local web app served by the desktop service, opened in
Chromium and connected to the engine. A **role** is a job on a project's team
(orchestrator, builders such as `backend`, reviewers), and a **hire** is the model
and effort it runs on. Each piece of work is
a **run**: one Claude Code or Codex session in its own isolated systemd unit and
checkout. The orchestrator plans a goal into tasks, builders do them in runs, AI
reviewers check each pull request, and dvb merges it once the checks pass.

![The dvb cockpit on a small demo project](docs/cockpit.png)

## Requirements

- Arch Linux or Omarchy on x86_64, with systemd 255 or newer and user namespaces
  enabled, and sudo (or pkexec).
- Memory: each run is capped at 8 GiB and starts only while 10 GiB is available;
  all runs together get half the RAM (at least 2 GiB, setting `runs_memory_gb`),
  and 3 runs go at once by default (`max_runs`).
- Disk: a run starts only while the filesystem holding `/var/lib/dvb/work` has
  25 GiB free. Project mirrors and their caches, with the Rust and Node toolchains
  runs fetch, live in `/var/lib/dvb/mirrors`.
- A GitHub organization you can administer, or your personal GitHub account.
- A public or private GitHub repository. A public repository works on any GitHub
  plan. dvb protects each project's default branch with a ruleset, and GitHub
  Free allows rulesets only on public repositories: on a private repository, a
  paid plan (Team for an organization, Pro for a personal account) adds that branch
  protection ([details](docs/commands.md#github-connection-and-project-addition)).
- A Claude Code or Codex subscription: runs use it
  ([costs and GitHub effects](docs/install.md#costs-and-github-effects)).
  The Claude login is required by default, because new projects' default hires use
  the first selected model, a Claude model unless `model_pool` lists only Codex
  models; the Codex login is optional.
  `dvb doctor` counts a missing login as unmet only when the default hires use its
  vendor. One vendor alone runs every role; with both you can hire roles on either
  vendor, and a limited vendor's runs can move to the orchestrator's vendor
  (`vendor_fallback`) instead of waiting for its limit to reset.

The installer installs everything else that is missing with pacman
([what it installs](docs/install.md)). There is no hosted service: the engine,
cockpit and the GitHub App's key stay on your machines, and you need no account
with the dvb authors.

## Install

Download the current GitHub release's installer and install the engine and cockpit
in one step:

```sh
curl -fsSL https://github.com/broken-branch/dvb/releases/latest/download/install.sh -o install.sh
sudo sh install.sh --engine --cockpit
```

Without sudo, use `pkexec sh "$PWD/install.sh" --engine --cockpit`. The installer
verifies the release signature and checksums before writing dvb's files, and starts
the cockpit in your session.

## Connect

1. Set a git name and email (`git config --global user.name` and `user.email`):
   connect records them with the App.
2. Run `dvb github connect --owner <your-org-or-user>`. It creates a private GitHub
   App owned by your organization or personal account. Open the two links it prints
   in a browser on this machine: on GitHub, click Create, then choose the
   repositories the App may access and click Install. It ends with
   `Connected to <owner>; repositories: …`.
3. Run `dvb login claude` (and `dvb login codex` if you use Codex). They log the
   providers into dvb's isolated environment, separate from your usual home-folder
   CLI logins.

`dvb doctor` shows what setup still needs. The app's setup wizard (`dvb app`) does
the same steps.

## A first goal

This works on a personal account and on GitHub Free. On GitHub, make a public
repository named `example` with a README, and give the dvb App access to it (the
App's installation settings on GitHub). Then:

```sh
dvb project add example --repo <you>/example --no-ci
dvb goal example "Add a search box to the catalogue"
dvb wait <goal-id> --until plan
dvb answer <question-id> <n>
dvb app
```

`--no-ci` says the repository has no CI: a new repository has no checks for dvb to
find, and dvb asks for a choice rather than assume none. `dvb goal` prints the goal's
id; planning takes minutes, and `dvb wait` returns when the plan question is open
and prints it with its numbered options. Read the plan, then answer with the number
of the option that approves it (or the recommended one): option numbers vary per
question.

In an organization on a paid plan, dvb can create the private repository itself,
also with no CI yet:

```sh
dvb project add example --repo your-org/example --create
```

For an existing repository with CI, see [everyday use](docs/commands.md#everyday-use).

## More

- [Installation details](docs/install.md): separate engine and desktop, installed
  files, costs and GitHub effects, security, removal, forks and how runs get
  toolchains.
- [Command reference](docs/commands.md): everyday use, goals from issues, checks,
  model selection, hires and settings. `dvb --help` lists all commands.
- [Agent skill](skills/dvb/SKILL.md): drive dvb from a coding agent with a shell
  using JSON output and exit codes. Install it by copying `skills/dvb` to
  `~/.claude/skills/dvb` for Claude Code, or to `~/.codex/skills/dvb` for Codex.
- [Omarchy bar widget](https://github.com/broken-branch/omarchy-dvb#readme): a bar icon that shows when dvb
  needs you.
- [Contributing](CONTRIBUTING.md) and [security](SECURITY.md). Licence:
  [MIT](LICENSE).
