# Installation details

The [first-use guide](../README.md) covers requirements, current-release installation,
setup and a first goal. This page covers what the installer does, a separate engine
and desktop, installed files, costs and GitHub effects, security, complete removal,
forks, and how runs get toolchains.

`sudo sh install.sh --engine --cockpit` (or `pkexec sh "$PWD/install.sh" --engine
--cockpit`) is the only step on the machine. Run as root, it checks each missing
package with `pacman -Si` and installs it with `pacman -S --needed`. The tools the
install itself uses (Git, curl, tar, minisign, polkit, shadow, OpenSSH for
`--engine-host`) must all install, or it stops. Because worker units hide home
directories, it then installs these system packages for runs and the app, one at a
time; one that pacman lacks or cannot install is a warning, not a stop: the Claude
Code CLI (`claude-code`), the Codex CLI (`openai-codex`), Chromium, rustup and mise.
Where no repository has `claude-code`, the installer downloads Claude Code's native
Linux build at the pinned version from the npm registry, checks it against the
SHA-512 recorded in the installer and installs it as `/usr/local/bin/claude`;
nothing in it runs during the install. systemd is checked, not installed.
On Arch, installing rustup replaces conflicting system Rust packages automatically,
so runs can fetch a project's pinned toolchain even when system cargo was present.

Without pacman, the installer stops before changing anything if a tool it uses is
missing, and lists those tools. The CLIs, Chromium and toolchain managers are not
installed: it names those runs cannot find and carries on with the install.
Existing system Rust is kept and runs use it. Runs build with the toolchain a
project pins ([how runs get toolchains](#how-runs-get-toolchains)).

On Omarchy, the cockpit adds a dvb submenu to
`~/.config/omarchy/extensions/omarchy-menu.jsonc` at its first start, once: into a
missing menu or one holding only comments; otherwise it prints the block to add by
hand. Open **dvb → New goal** or **dvb → Answer a question** from the menu and press
the voxtype key to dictate into the text box.

## Separate engine and desktop

Download `install.sh` from the current GitHub release on each machine.
On the engine host, name the account that uses dvb as `--owner` (default: the
account running sudo):

```sh
sudo sh install.sh --engine --owner <user>
```

On the desktop, use an SSH host you can reach without an interactive password. The
SSH login on the engine host must be that `--owner` account: the engine's owner
socket accepts no other account.

```sh
sudo sh install.sh --cockpit --engine-host <user>@<engine-host>
dvb app
```

## Installed files

The installer creates service accounts `dvb` and `dvb-worker`, without logins.
`/var/lib/dvb` contains `state/` (database and backups), `mirrors/`, `work/` and
`logins/`. `/opt/dvb/versions/<version>/dvb` holds binaries; `current` selects the
active version, linked from `/usr/local/bin/dvb`. `role-engine` and `role-cockpit`
markers under `/opt/dvb` tell the update command which role is installed.
New `/var/lib/dvb` is a btrfs subvolume when its parent is btrfs and the tool is
available. This keeps data out of system snapshots. `dvb.slice` defaults to half
RAM for all runs together, with a minimum of 2 GiB. An existing slice is preserved.
Change its limit live with `dvb settings set runs_memory_gb <GiB>`. Each run keeps
its 8G cap.

Engine units in `/etc/systemd/system` are `dvb.service`, `dvb.socket`,
`dvb-run.socket` and `dvb-run-{claude,codex,fake}@.service`. The installer enables
the engine and sockets; run instances start on demand, including provider logins.
`/etc/polkit-1/rules.d/50-dvb.rules` permits the engine to manage run units and
call `set-property` on `dvb.slice`. `systemctl cat dvb-run-{claude,codex,fake}@.service`
shows the run templates with any local overrides that widen their access.
The owner socket `/run/dvb/engine.sock` accepts the `--owner` account (default:
the account running sudo). `/run/dvb/run.sock` is for workers.

The cockpit installs `/etc/systemd/user/dvb-desk.service`, its drop-in
`dvb-desk.service.d/10-owner.conf` and `/usr/share/applications/dvb.desktop`.
Optional forwarding adds `/etc/systemd/user/dvb-forward.service` and
`/etc/tmpfiles.d/dvb-cockpit.conf`, which creates `/run/dvb` owned by the desktop
account and its primary group. The installer enables these user services for every
session (`systemctl --global enable`), runs them only for the `--owner` account
(`ConditionUser=`), and starts them in that account's running session; otherwise
they start at its next login. The cockpit's control socket is
`$XDG_RUNTIME_DIR/dvb-desk.sock` (normally under `/run/user/<uid>`).

Updates verify signatures and checksums, back up state and move `current` to the
new version. Installing over an existing engine also backs up its database.
`state/publication.json` holds the identity dvb publishes commits as
([settings](commands.md#settings)).

## Costs and GitHub effects

Runs use the Claude Code and Codex accounts you log in and their subscription limits.
Provider subscriptions or usage can cost money; dvb's token cost display is an
estimate at API prices, not a bill. GitHub Actions can also consume your plan's
allowance. dvb creates a private GitHub App owned by your account, a repository
ruleset, a project board and goal milestones. It pushes task branches, opens pull requests and merges
reviewed work after the configured checks pass. It publishes commits as
`dvb <dvb@dvb.invalid>` unless you [choose another identity](commands.md#settings).

dvb merges pull requests reviewed only by its AI reviewers into the default
branch, with no human code review. The ruleset it adds to the default branch
(`dvb default branch`) requires pull requests (no approving review or resolved
review threads are needed), blocks force-push and deletion, requires the
project's chosen checks to pass on an up-to-date branch, and has no bypass actors. On an
existing repository it therefore blocks your own direct pushes to the default
branch too; change or delete the ruleset in the repository's settings on GitHub
if you need to. GitHub allows rulesets on a public repository on any plan, and on
a private repository only on a paid plan (Team or Pro).

## Security

Runs of all projects share the Claude and Codex login directories
(`/var/lib/dvb/logins/claude` and `/var/lib/dvb/logins/codex`, each run's
`~/.claude` and `~/.codex`), so a project you don't trust shouldn't share a dvb
install with projects you do. Report security problems as
[SECURITY.md](../SECURITY.md) describes.

## Remove dvb

These commands delete local work, logins, database and backups. Run the
`systemctl --user` commands as the desktop account. On a split install, remove the
cockpit on the desktop and the engine on its host. GitHub repositories, App,
rulesets, boards and milestones remain; remove those separately in GitHub if wanted.

If you used `dvb keybind`, delete its line from `~/.config/hypr/bindings.lua`.
On Omarchy, delete the `dvb` entries (`"dvb"` and `"dvb.…"`) the cockpit added to
`~/.config/omarchy/extensions/omarchy-menu.jsonc`.

Stop and disable the cockpit, then remove its installed files, its icons and the
copies of the desktop entry and icons it keeps in your data directory:

```sh
systemctl --user stop dvb-desk.service
sudo systemctl --global disable dvb-desk.service
sudo rm -rf /etc/systemd/user/dvb-desk.service /etc/systemd/user/dvb-desk.service.d \
  /usr/share/applications/dvb.desktop
systemctl --user daemon-reload
rm -f "${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/dvb-desk.sock"
data="${XDG_DATA_HOME:-$HOME/.local/share}"
for size in 16 32 48 64 128 256 512; do
  sudo rm -f "/usr/share/icons/hicolor/${size}x$size/apps/dvb.png"
  rm -f "$data/icons/hicolor/${size}x$size/apps/dvb.png"
done
sudo rm -f /usr/share/icons/hicolor/scalable/apps/dvb.svg
rm -f "$data/icons/hicolor/scalable/apps/dvb.svg" "$data/applications/dvb.desktop" \
  "${XDG_STATE_HOME:-$HOME/.local/state}/dvb/omarchy-menu-attempted"
```

If you installed SSH forwarding, also remove it:

```sh
systemctl --user stop dvb-forward.service
sudo systemctl --global disable dvb-forward.service
sudo rm -f /etc/systemd/user/dvb-forward.service /etc/tmpfiles.d/dvb-cockpit.conf
systemctl --user daemon-reload
```

On the engine host, stop activation and all run instances before deleting files:

```sh
sudo systemctl disable --now dvb.service dvb.socket dvb-run.socket
sudo systemctl stop 'dvb-run-claude@*.service' 'dvb-run-codex@*.service' 'dvb-run-fake@*.service'
sudo rm -f /etc/systemd/system/dvb.service /etc/systemd/system/dvb.socket \
  /etc/systemd/system/dvb-run.socket /etc/systemd/system/dvb-run-claude@.service \
  /etc/systemd/system/dvb-run-codex@.service /etc/systemd/system/dvb-run-fake@.service \
  /etc/polkit-1/rules.d/50-dvb.rules /etc/systemd/system/dvb.slice
sudo rm -rf /etc/systemd/system/dvb-run-.service.d /etc/systemd/system.control/dvb.slice.d
sudo systemctl daemon-reload
sudo rm -rf /var/lib/dvb
sudo userdel dvb
sudo userdel dvb-worker
sudo groupdel dvb
sudo groupdel dvb-worker
```

`userdel` may already remove the matching group; skip `groupdel` if it is gone.
Finally, on every installed host remove the binaries and runtime directory:

```sh
sudo rm -f /usr/local/bin/dvb /usr/local/bin/sccache
sudo rm -rf /opt/dvb /run/dvb
```

Packages the installer added with pacman stay installed; remove them with pacman if
you no longer want them. Where no package had Claude Code, the installer put it in
`/usr/local/bin/claude`; delete that file if you no longer want it.

## Forks

After replacing `broken-branch/dvb` throughout the source with a fork's repository name, its installer and `dvb update` fetch releases from that fork's own repository.
For a private fork's releases, give `install.sh` a GitHub token with
`--token-file <file>` (the token is the file's first line), or set
`DVB_GITHUB_TOKEN_FILE` or `DVB_GITHUB_TOKEN`; sudo drops the environment, so
prefer the option. `dvb update` reads `DVB_GITHUB_TOKEN` or `DVB_GITHUB_TOKEN_FILE`
from the engine's environment; the file must be readable by the engine's user.

## How runs get toolchains

Runs build with the toolchain a project pins. Unless the run units or the engine's
service already set `RUSTUP_HOME` (for example a drop-in with
`Environment=RUSTUP_HOME=/opt/rust/rustup`), which is kept, runs use a `RUSTUP_HOME`
beside `CARGO_HOME` in the project's cache under `/var/lib/dvb/mirrors/.cache/`, so
rustup fetches the `rust-toolchain.toml` toolchain on first use (stable when none is
pinned). A run of a Rust workspace installs it before its session, under a lock in
`RUSTUP_HOME`, so concurrent runs of a project do not install it at once. mise puts
`node`, `npm`, `npx` and `corepack` first on a run's `PATH` and fetches the `.nvmrc`
or `.node-version` Node into the same cache; a project that pins none uses the
system's Node. Each run makes the mise links afresh. The engine installer also
includes pinned, checksum-verified sccache for the per-project Cargo caches.

A run sees only its own project's cache: the run units' drop-in
`/etc/systemd/system/dvb-run-.service.d/10-common.conf` hides
`/var/lib/dvb/mirrors/.cache` and mounts back the one directory the engine links
from the run's directory. `dvb update` replaces only the binary, never unit files.
`install.sh` writes the units' version to
`/var/lib/dvb/state/units-version`; while it is missing or differs from the
engine's, `dvb status` and the engine's log at start warn "the run units are
older than this engine: re-run install.sh". Before each run the engine replaces a project's cache entry that is
a link or not a directory with a real directory, and it sets the mode of a
directory it makes without following a link. A run that waits more than 10
minutes for another run of its project to install the Rust toolchain gives up on
that step and sets `RUSTUP_AUTO_INSTALL=0` for its session, so its `cargo` and
`rustc` do not install the toolchain into the same `RUSTUP_HOME` at once; they
fail until the other run has installed it. rustup before 1.28.1 ignores that
variable and installs it anyway.

The same drop-in gives each run private IPC and protects kernel tunables, modules, logs and control groups.
