#!/bin/sh
# Installs dvb on this box, once, as root. The one step on a new machine:
#
#   sudo install/install.sh --engine [--cockpit] [--binary <path>] [--owner <user>]
#   sudo install/install.sh --cockpit --engine-host <ssh-host> [--owner <user>]
#
# From a release, the install.sh asset of that release downloads its own version:
#   curl -fsSL https://github.com/broken-branch/dvb/releases/latest/download/install.sh -o install.sh
#   sudo sh install.sh --engine --cockpit      (or: pkexec sh "$PWD/install.sh" --engine --cockpit)
#
# Missing packages are installed with pacman (Arch, Omarchy): the tools the install
# uses, which it stops without, and for runs, which see no home folder, the Claude
# Code and Codex CLIs, Chromium, rustup and mise, which it warns about if they cannot
# be installed. Without pacman, missing tools stop it and the rest are listed.
#
# --engine   the users dvb and dvb-worker, /var/lib/dvb, the engine's service and
#            sockets, the run unit templates and the polkit rule; starts the engine
# --cockpit  the desk's user unit, enabled for the owner's sessions and started in
#            the running one, and the desktop entry with its icon
# --engine-host  forward a remote engine's socket to this cockpit over SSH
# --binary   install this dvb binary (the files beside this script) instead of
#            downloading the release this script belongs to
# --token-file  a file whose first line is a GitHub token for installing from a private
#            fork's releases (or set DVB_GITHUB_TOKEN_FILE / DVB_GITHUB_TOKEN;
#            sudo drops the environment, so prefer the option)
# --owner    the account that uses dvb (default: the one that ran sudo or pkexec);
#            the engine accepts the owner API from it and from itself only
#
# Nothing here runs again after install: updates replace /opt/dvb/current.
set -eu

die() { echo "install.sh: $*" >&2; exit 1; }

engine=0 cockpit=0 engine_host="" binary="" token_file="" owner="${SUDO_USER:-}"
if [ -z "$owner" ] && [ -n "${PKEXEC_UID:-}" ]; then
  owner=$(id -nu "$PKEXEC_UID" 2>/dev/null) || owner=""
fi
while [ $# -gt 0 ]; do
  case "$1" in
    --engine) engine=1 ;;
    --cockpit) cockpit=1 ;;
    --engine-host)
      engine_host="${2:-}"
      case "$engine_host" in
        ''|-*|*[!A-Za-z0-9_.@:-]*) die "--engine-host <ssh-host>: use a host or user@host without spaces or a leading -" ;;
      esac
      shift ;;
    --binary) binary="${2:-}"; shift ;;
    --owner) owner="${2:-}"; shift ;;
    --token-file) token_file="${2:-}"; shift ;;
    *) die "unknown option: $1" ;;
  esac
  shift
done

if [ -n "$engine_host" ]; then
  [ "$engine" = 0 ] || die "--engine-host cannot be used with --engine"
  [ "$cockpit" = 1 ] || die "--engine-host requires --cockpit"
fi
[ "$(id -u)" = 0 ] || die "run as root (sudo)"
[ "$engine" = 1 ] || [ "$cockpit" = 1 ] || die "choose --engine, --cockpit or both"
if [ -z "$owner" ] || ! id "$owner" >/dev/null 2>&1; then
  die "--owner <user>: the account that uses dvb"
fi
here=$(cd "$(dirname "$0")" && pwd)

# release.sh stamps these two into the install.sh it ships (scripts/release.sh).
release_tag="@RELEASE_TAG@"
release_key="@RELEASE_PUBKEY@"
if [ -z "$binary" ]; then
  case "$release_tag" in
    @*) die "--binary <path to dvb>, or use the install.sh of a release" ;;
  esac
  case "$release_key" in
    ''|@*) die "this release names no signing key; refusing to install it" ;;
  esac
fi

# Every tool the install relies on, checked before anything is written. systemd is
# a check; a missing package is installed with pacman, or listed without it.
# Runs see the system's programs only (ProtectHome=yes), on their unit's PATH.
runs_path=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
on_runs_path() { (PATH=$runs_path; command -v "$1" >/dev/null 2>&1); }
warn() { echo "install.sh: $*" >&2; }
has_pacman=0
command -v pacman >/dev/null 2>&1 && has_pacman=1
checks="" missing="" packages="" wants="" absent="" claude_tarball=0
add() { # <pacman package> <what it is>
  case " $packages " in *" $1 "*) return 0 ;; esac
  packages="$packages $1"
  missing="$missing
  - $2"
}
need() { command -v "$1" >/dev/null 2>&1 || add "$2" "$3"; }
# What runs and the app use, not the install itself: installed when pacman can,
# otherwise named. A want that cannot be installed is a warning, never fatal.
want() { # <command> <pacman package> <what it is>
  on_runs_path "$1" && return 0
  if [ "$has_pacman" = 1 ]; then wants="$wants
$2 $3"; else absent="$absent
  - $3"; fi
}
command -v systemctl >/dev/null 2>&1 || checks="$checks
  - systemd (systemctl)"
[ -n "$binary" ] || [ -n "${DVB_MINISIGN:-}" ] || need minisign minisign "minisign (verifies the release)"
if [ "$engine" = 1 ]; then
  need git git "git"
  need curl curl "curl"
  need tar tar "tar"
  need sha256sum coreutils "coreutils (sha256sum)"
  need groupadd shadow "the shadow-utils/passwd package (groupadd, useradd, usermod)"
  need useradd shadow "the shadow-utils/passwd package (groupadd, useradd, usermod)"
  need usermod shadow "the shadow-utils/passwd package (groupadd, useradd, usermod)"
  [ -x /usr/lib/polkit-1/polkitd ] || [ -x /usr/libexec/polkitd ] || add polkit "polkit (the polkitd package)"
  want codex openai-codex "the Codex CLI (codex)"
  if on_runs_path claude || [ "$has_pacman" = 0 ]; then want claude claude-code "the Claude Code CLI (claude)"
  elif pacman -Si claude-code >/dev/null 2>&1; then want claude claude-code "the Claude Code CLI (claude)"
  else
    # Plain Arch has no claude-code package: its native build is fetched from npm's
    # registry below and checked, and nothing in it runs at install.
    claude_tarball=1
    need sha512sum coreutils "coreutils (sha512sum)"
  fi
  # rustup and mise fetch the toolchain a project pins on first use (dvb-run).
  want rustup rustup "rustup (Rust toolchains)"
  want mise mise "mise (Node versions a project pins)"
fi
on_runs_path chromium || on_runs_path google-chrome-stable || want chromium chromium "Chromium"
if [ -n "$engine_host" ]; then
  need ssh openssh "an ssh client (openssh-client)"
  command -v systemd-tmpfiles >/dev/null 2>&1 || checks="$checks
  - systemd (systemd-tmpfiles)"
fi
if [ -n "$checks" ] || { [ -n "$missing" ] && [ "$has_pacman" = 0 ]; }; then
  die "missing prerequisites; install these, then run this again (nothing was changed):$checks$missing"
fi
if [ -n "$wants" ] && [ "$has_pacman" = 1 ]; then
  printf 'install.sh: installs missing tools where available:%s\n' "$wants"
fi
if [ -n "$packages" ]; then
  unknown=""
  for package in $packages; do
    pacman -Si "$package" >/dev/null 2>&1 || unknown="$unknown $package"
  done
  [ -z "$unknown" ] \
    || die "pacman's repositories have no$unknown; install the missing tools, then run this again (nothing was changed):$missing"
  echo "install.sh: installing with pacman:$missing"
  # shellcheck disable=SC2086
  pacman -S --needed --noconfirm $packages \
    || die "pacman could not install these; fix what it reported above, then run this again:$missing"
fi
# One package at a time, so a missing or failing one leaves the others installed.
printf '%s\n' "$wants" | while read -r package what; do
  [ -n "$package" ] || continue
  # rustup provides Rust and conflicts with Arch's rust/cargo/rustfmt packages.
  # In this transaction only, accept conflict removal (ALPM question bit 4).
  set --
  [ "$package" != rustup ] || set -- --ask=4
  if ! pacman -Si "$package" >/dev/null 2>&1; then
    warn "pacman's repositories have no $package; runs and the app need $what installed system-wide"
  else
    if [ "$package" = rustup ]; then
      conflicts=""
      for conflict in rust cargo rustfmt; do
        installed=$(pacman -Q "$conflict" 2>/dev/null) || continue
        # Queries can resolve provides; only the named package is a conflict.
        [ "${installed%% *}" != "$conflict" ] || conflicts="$conflicts $conflict"
      done
      [ -z "$conflicts" ] || warn "replacing pacman's$conflicts with rustup (provides cargo; runs need the toolchain a project pins)"
    fi
    if ! pacman -S --needed --noconfirm "$@" "$package" </dev/null; then
      warn "pacman could not install $package; runs and the app need $what installed system-wide"
    fi
  fi
done
if [ "$claude_tarball" = 1 ]; then
  # The version dvb-run's stream parser was recorded against (claude.rs RECORDED_VERSION),
  # checked against the registry's sha512 recorded here.
  claude_version=2.1.286
  claude_sha512=3e754bf190847afdc87b12de39cf7f4117ea4435194609ec68c2dda6394137455a89e793adfee66762498b6f6e7b6280a87beae92b0e69d5391bfb219c513830
  claude_tmp=$(mktemp -d)
  trap 'rm -rf "$claude_tmp"' EXIT
  if curl -fsSL -o "$claude_tmp/claude.tgz" \
      "${DVB_CLAUDE_BASE:-https://registry.npmjs.org/@anthropic-ai/claude-code-linux-x64/-}/claude-code-linux-x64-$claude_version.tgz"; then
    printf '%s  %s\n' "$claude_sha512" "$claude_tmp/claude.tgz" | sha512sum -c - >/dev/null \
      || die "the checksum of Claude Code $claude_version does not match"
    tar -xzf "$claude_tmp/claude.tgz" -C "$claude_tmp" package/claude
    install -m 0755 "$claude_tmp/package/claude" /usr/local/bin/claude
  fi
  rm -rf "$claude_tmp"
  trap - EXIT
fi
[ -z "$absent" ] || warn "not installed (no pacman); runs and the app need these system-wide:$absent"
if [ "$engine" = 1 ] && ! on_runs_path claude; then
  codex_ready=""
  if on_runs_path codex; then codex_ready="; Codex is ready"; fi
  warn "Claude Code isn't installed; install it system-wide using https://code.claude.com/docs/en/setup#install-claude-code$codex_ready."
fi
if [ "$engine" = 1 ] && [ "$has_pacman" = 1 ] && ! on_runs_path codex; then
  warn "runs will not find codex on $runs_path"
fi
if [ "$engine" = 1 ] && on_runs_path cargo && ! on_runs_path rustup; then
  warn "runs use the system's Rust and ignore a project's rust-toolchain.toml; replace the rust package with rustup to honour it"
fi

if [ -z "$binary" ]; then
  command -v curl >/dev/null || die "curl is needed"
  tmp=$(mktemp -d)
  trap 'rm -rf "$tmp"' EXIT
  ver=${release_tag#v}
  tarball="dvb-$ver-x86_64-linux.tar.gz"

  # For a private fork's releases, a token (DVB_GITHUB_TOKEN, or a
  # file named by --token-file / DVB_GITHUB_TOKEN_FILE) opens the GitHub API.
  # It reaches curl on stdin, never in an argument.
  token="${DVB_GITHUB_TOKEN:-}"
  [ -n "$token_file" ] || token_file="${DVB_GITHUB_TOKEN_FILE:-}"
  if [ -z "$token" ] && [ -n "$token_file" ]; then
    [ -r "$token_file" ] || die "cannot read the token file $token_file"
    token=$(head -n 1 "$token_file")
  fi
  # DVB_RELEASE_BASE is for tests and mirrors: any URL curl reads, e.g. file:///dir.
  if [ -n "${DVB_RELEASE_BASE:-}" ] || [ -z "$token" ]; then
    base="${DVB_RELEASE_BASE:-https://github.com/broken-branch/dvb/releases/download}/$release_tag"
    fetch_file() { # <name> <required: 1|0>
      curl -fsSL -o "$tmp/$1" "$base/$1" 2>/dev/null && return 0
      [ "$2" = 0 ] && return 1
      die "cannot download $base/$1 (a private fork's releases need --token-file <file with a GitHub token>)"
    }
  else
    api="${DVB_API_BASE:-https://api.github.com}/repos/broken-branch/dvb"
    with_token() { printf 'header = "Authorization: Bearer %s"\n' "$token" | curl --config - "$@"; }
    with_token -fsSL -H "Accept: application/vnd.github+json" -o "$tmp/release.json" "$api/releases/tags/$release_tag" \
      || die "cannot read release $release_tag from $api (is the token valid for this repository?)"
    fetch_file() { # <name> <required: 1|0>
      url=$(awk -v want="$1" '
        /"url": *"[^"]*\/assets\/[^"]*"/ { u = $0; sub(/.*"url": *"/, "", u); sub(/".*/, "", u) }
        $0 ~ "\"name\": *\"" want "\"" { print u; exit }' "$tmp/release.json")
      if [ -z "$url" ]; then
        [ "$2" = 0 ] && return 1
        die "release $release_tag has no asset $1"
      fi
      with_token -fsSL -H "Accept: application/octet-stream" -o "$tmp/$1" "$url" 2>/dev/null && return 0
      [ "$2" = 0 ] && return 1
      die "cannot download asset $1 of release $release_tag"
    }
  fi
  fetch_file "$tarball" 1
  fetch_file SHA256SUMS 1

  fetch_file SHA256SUMS.minisig 0 || die "this release has no signature; refusing to install it"
  minisign_bin="${DVB_MINISIGN:-minisign}"
  command -v "$minisign_bin" >/dev/null \
    || die "minisign is needed to verify this release (install the minisign package, then run this again)"
  "$minisign_bin" -Vm "$tmp/SHA256SUMS" -x "$tmp/SHA256SUMS.minisig" -P "$release_key" >/dev/null \
    || die "the signature on SHA256SUMS does not match this release's key"
  (cd "$tmp" && grep " $tarball\$" SHA256SUMS | sha256sum -c - >/dev/null) \
    || die "the checksum of $tarball does not match SHA256SUMS"
  tar -xzf "$tmp/$tarball" -C "$tmp"
  binary="$tmp/dvb-$ver/dvb"
  here="$tmp/dvb-$ver/install"
fi
[ -x "$binary" ] || die "--binary <path to dvb>: not an executable file"
version=$("$binary" --version | awk '{print $2}')
[ -n "$version" ] || die "$binary --version printed no version"

# Every file the chosen roles install must exist before anything changes. Beside
# a --binary from a release tarball, they are in the install/ directory next to it.
needed=""
[ "$engine" = 1 ] && needed="dvb.service dvb.socket dvb-run.socket dvb-run-claude@.service dvb-run-codex@.service dvb-run-fake@.service dvb-run-.service.d/10-common.conf 50-dvb.rules"
icon_sizes="16 32 48 64 128 256 512"
if [ "$cockpit" = 1 ]; then
  needed="$needed dvb-desk.service dvb.desktop icons/dvb.svg"
  for size in $icon_sizes; do needed="$needed icons/dvb-$size.png"; done
fi
needed=${needed# }
if [ ! -f "$here/${needed%% *}" ] && [ -d "$(dirname "$binary")/install" ]; then
  here=$(cd "$(dirname "$binary")/install" && pwd)
fi
# shellcheck disable=SC2086
for f in $needed; do
  [ -f "$here/$f" ] || die "missing $here/$f (the unit files are in the install/ directory of the release)"
done

if [ "$engine" = 1 ]; then
  sccache_version=0.12.0
  sccache_archive="sccache-v$sccache_version-x86_64-unknown-linux-musl"
  sccache_sha256=b0e89ead6899224a4ba2b90e9073bf1ce036d95bab30f3dc33c1e1468bc4ad44
  sccache_tmp=$(mktemp -d)
  trap 'rm -rf "$sccache_tmp"; [ -z "${tmp:-}" ] || rm -rf "$tmp"' EXIT
  curl -fsSL -o "$sccache_tmp/sccache.tar.gz" \
    "https://github.com/mozilla/sccache/releases/download/v$sccache_version/$sccache_archive.tar.gz" \
    || die "cannot download sccache $sccache_version"
  printf '%s  %s\n' "$sccache_sha256" "$sccache_tmp/sccache.tar.gz" | sha256sum -c - >/dev/null \
    || die "the checksum of sccache $sccache_version does not match"
  tar -xzf "$sccache_tmp/sccache.tar.gz" -C "$sccache_tmp"
  install -m 0755 "$sccache_tmp/$sccache_archive/sccache" /usr/local/bin/sccache
fi

# The binary: /opt/dvb/versions/<v>/dvb, and current pointing at it.
install -d -m 0755 /opt/dvb /opt/dvb/versions "/opt/dvb/versions/$version"
install -m 0755 "$binary" "/opt/dvb/versions/$version/dvb"
ln -sfn "versions/$version" /opt/dvb/current
ln -sfn /opt/dvb/current/dvb /usr/local/bin/dvb
# What this box is, for `dvb update` (a cockpit's socket may be forwarded from elsewhere).
if [ "$engine" = 1 ]; then : > /opt/dvb/role-engine; fi
if [ "$cockpit" = 1 ]; then : > /opt/dvb/role-cockpit; fi

if [ "$engine" = 1 ]; then
  # Users: dvb runs the engine and is in group dvb-worker, so the run
  # directories it makes keep their group and it can read what workers write.
  getent group dvb-worker >/dev/null || groupadd --system dvb-worker
  id dvb-worker >/dev/null 2>&1 || useradd --system --gid dvb-worker \
    --no-create-home --home-dir /nonexistent --shell /usr/sbin/nologin dvb-worker
  getent group dvb >/dev/null || groupadd --system dvb
  id dvb >/dev/null 2>&1 || useradd --system --gid dvb \
    --no-create-home --home-dir /nonexistent --shell /usr/sbin/nologin dvb
  usermod -aG dvb-worker dvb
  # The engine updates itself: it unpacks new versions here and moves
  # `current`. Workers only read it.
  chown -R dvb:dvb /opt/dvb

  # Paths. mirrors/ and work/ are setgid so what the engine makes in
  # them belongs to group dvb-worker, which runs read.
  if [ ! -e /var/lib/dvb ] && [ ! -L /var/lib/dvb ]; then
    if [ "$(stat -f -c %T /var/lib)" = btrfs ] && command -v btrfs >/dev/null 2>&1; then
      btrfs subvolume create /var/lib/dvb >/dev/null
      chmod 0755 /var/lib/dvb
      echo "dvb $version: created /var/lib/dvb subvolume; keeps dvb's data out of system snapshots"
    else
      install -d -m 0755 -o root -g root /var/lib/dvb
    fi
  fi
  install -d -m 0700 -o dvb -g dvb /var/lib/dvb/state
  install -d -m 2750 -o dvb -g dvb-worker /var/lib/dvb/mirrors
  install -d -m 2770 -o dvb -g dvb-worker /var/lib/dvb/work
  install -d -m 2770 -o dvb-worker -g dvb-worker /var/lib/dvb/mirrors/.cache
  # logins/ and the vendor dirs are traverse-only for the engine (group dvb, 0710), so
  # GET /logins can stat the known credential file names; the files stay 0600 dvb-worker.
  install -d -m 0710 -o dvb-worker -g dvb \
    /var/lib/dvb/logins /var/lib/dvb/logins/claude /var/lib/dvb/logins/codex
  # Installing over a running install (the way to move to a release that cannot
  # update itself): stop the engine, keep a copy of its database first.
  if [ -f /var/lib/dvb/state/dvb.sqlite ]; then
    systemctl stop dvb.service 2>/dev/null || true
    install -d -m 0700 -o dvb -g dvb /var/lib/dvb/state/backups
    install -m 0600 -o dvb -g dvb /var/lib/dvb/state/dvb.sqlite \
      "/var/lib/dvb/state/backups/pre-install-$(date -u +%Y%m%dT%H%M%SZ).sqlite"
  fi
  id -u "$owner" > /var/lib/dvb/state/owner
  chown dvb:dvb /var/lib/dvb/state/owner
  chmod 0600 /var/lib/dvb/state/owner

  # Units and the polkit rule.
  for f in dvb.service dvb.socket dvb-run.socket \
    dvb-run-claude@.service dvb-run-codex@.service dvb-run-fake@.service; do
    install -m 0644 "$here/$f" /etc/systemd/system/
  done
  install -D -m 0644 "$here/dvb-run-.service.d/10-common.conf" \
    /etc/systemd/system/dvb-run-.service.d/10-common.conf
  # The engine warns while this differs from its launch::UNITS_VERSION.
  printf '2\n' > /var/lib/dvb/state/units-version
  chmod 0644 /var/lib/dvb/state/units-version
  install -d -m 0755 /etc/polkit-1/rules.d
  install -m 0644 "$here/50-dvb.rules" /etc/polkit-1/rules.d/50-dvb.rules

  if [ ! -e /etc/systemd/system/dvb.slice ] && [ ! -L /etc/systemd/system/dvb.slice ]; then
    runs_memory_gb=$(awk '/^MemTotal:/ { print int($2 / 1024 / 1024 / 2) }' /proc/meminfo)
    [ -n "$runs_memory_gb" ] || die "cannot read RAM from /proc/meminfo"
    [ "$runs_memory_gb" -ge 2 ] || runs_memory_gb=2
    runs_memory_high_mb=$((runs_memory_gb * 896))
    cat > /etc/systemd/system/dvb.slice <<EOF
[Unit]
Description=dvb runs

[Slice]
MemoryMax=${runs_memory_gb}G
MemoryHigh=${runs_memory_high_mb}M
EOF
    chmod 0644 /etc/systemd/system/dvb.slice
  fi

  systemctl daemon-reload
  systemctl enable dvb.service
  systemctl enable --now dvb.socket dvb-run.socket
  systemctl restart dvb.service
  echo "dvb $version: engine installed for $owner; the owner API is /run/dvb/engine.sock"
  # An install over an existing one keeps the identity saved there.
  if [ ! -f /var/lib/dvb/state/publication.json ]; then
    echo "dvb $version: publishing commits as the default dvb <dvb@dvb.invalid>; to change it: dvb settings publication --name <name> --email <email>"
  fi
fi

if [ "$cockpit" = 1 ]; then
  # Without an engine on this box the desk updates its own copy.
  [ "$engine" = 1 ] || chown -R "$owner" /opt/dvb
  install -d -m 0755 /etc/systemd/user /etc/systemd/user/dvb-desk.service.d /usr/share/applications
  install -m 0644 "$here/dvb-desk.service" /etc/systemd/user/dvb-desk.service
  # Enabled for every session manager, but it runs in the owner's only: the
  # engine answers no one else, and another desk would take the owner's port.
  printf '[Unit]\nConditionUser=%s\n' "$owner" > /etc/systemd/user/dvb-desk.service.d/10-owner.conf
  chmod 0644 /etc/systemd/user/dvb-desk.service.d/10-owner.conf
  install -m 0644 "$here/dvb.desktop" /usr/share/applications/dvb.desktop
  # The desktop entry's Icon=dvb. The desk keeps the owner's own copy current
  # after updates, which do not run this script.
  for size in $icon_sizes; do
    install -D -m 0644 "$here/icons/dvb-$size.png" "/usr/share/icons/hicolor/${size}x$size/apps/dvb.png"
  done
  install -D -m 0644 "$here/icons/dvb.svg" /usr/share/icons/hicolor/scalable/apps/dvb.svg
  if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -f -t /usr/share/icons/hicolor \
      || warn "gtk-update-icon-cache failed; launchers may show dvb's icon only after their next cache update"
  fi
  user_units="dvb-desk.service"
  if [ -n "$engine_host" ]; then
    install -d -m 0755 /etc/tmpfiles.d
    owner_group=$(id -gn "$owner")
    printf 'd /run/dvb 0755 %s %s -\n' "$owner" "$owner_group" > /etc/tmpfiles.d/dvb-cockpit.conf
    systemd-tmpfiles --create /etc/tmpfiles.d/dvb-cockpit.conf
    cat > /etc/systemd/user/dvb-forward.service <<EOF
[Unit]
Description=dvb engine socket from $engine_host
After=network-online.target
ConditionUser=$owner
[Service]
ExecStart=/usr/bin/ssh -N -o BatchMode=yes -o ExitOnForwardFailure=yes -o ServerAliveInterval=30 -o StreamLocalBindUnlink=yes -L /run/dvb/engine.sock:/run/dvb/engine.sock $engine_host
Restart=always
RestartSec=5
[Install]
WantedBy=default.target
EOF
    chmod 0644 /etc/tmpfiles.d/dvb-cockpit.conf /etc/systemd/user/dvb-forward.service
    user_units="dvb-forward.service $user_units"
    echo "dvb $version: socket forward from $engine_host installed for $owner"
  fi
  # shellcheck disable=SC2086
  systemctl --global enable $user_units
  if [ -d "$(getent passwd "$owner" | cut -d: -f6)/.config/omarchy/extensions" ]; then echo "dvb: on Omarchy, the desk will add a dvb submenu to your menu once at first start"; fi
  # Start them in the owner's running session; otherwise its next login does.
  # shellcheck disable=SC2086
  if systemctl --user -M "$owner@" daemon-reload 2>/dev/null \
    && systemctl --user -M "$owner@" restart $user_units 2>/dev/null; then
    echo "dvb $version: cockpit installed and started for $owner; open it with: dvb app"
  else
    echo "dvb $version: cockpit installed; it starts when $owner next logs in"
  fi
fi

if command -v omarchy >/dev/null 2>&1; then
  echo "Omarchy bar widget: omarchy plugin add https://github.com/broken-branch/omarchy-dvb --enable"
fi
